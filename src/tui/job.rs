//! Long work runs on a job thread, never on the UI thread. The engines report through a
//! `TuiReporter`, which turns their output, progress and questions into `JobMsg`s.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};

use anyhow::Result;

use super::JOB_THREAD;
use super::event::{Event, JobMsg, Outcome};
use crate::ctx::{self, Reported};
use crate::pkglist::PkgList;
use crate::report::{self, JobCanceled, Level, Progress, Prompt, Reporter};
use crate::setup::{self, AutoPlan};
use crate::vault::backup::{BackupOpts, cmd_backup};
use crate::vault::import::{ImportOpts, cmd_import};

#[derive(Debug, Clone)]
pub enum JobKind {
    /// The steps and choices made before starting, with the package list the user saw
    /// (None when the plan installs no packages)
    Auto(Option<Arc<PkgList>>, Box<AutoPlan>),
    /// Mount the chosen partition (the decisions are all in the plan)
    Partition(Box<crate::partition::apply::Plan>),
    Repo,
    /// The list and the chosen package names (None = all)
    Packages(Arc<PkgList>, Option<Vec<String>>),
    Services,
    Import(ImportOpts),
    Export(BackupOpts),
    /// Back up and upload to Proton Drive
    Upload(BackupOpts, crate::drive::UploadOpts),
    /// Undo these changes (in this order)
    Undo(Vec<crate::undo::Item>),
    /// Remove these packages
    Uninstall(Vec<crate::undo::Item>),
    /// Install proton-drive from kud3n's repository
    InstallProton,
    /// Remove /var/lib/pacman/db.lck after a canceled pacman left it behind
    RemovePacmanLock,
    /// Install the packages a Plasma profile needs (pacman -S --needed)
    PlasmaDeps(Vec<String>),
    /// Scripted messages, for tests of the run screen and prompts
    #[cfg(test)]
    Fake(Vec<FakeStep>),
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub enum FakeStep {
    Progress(Progress),
    Log(Level, String),
    Ask(Prompt),
    Fail(String),
    Panic,
}

impl JobKind {
    pub fn title(&self) -> String {
        match self {
            JobKind::Auto(..) => "Automatic setup".into(),
            JobKind::Partition(_) => "Set up data partition".into(),
            JobKind::Repo => "Add kud3n's repository".into(),
            JobKind::Packages(..) => "Install packages".into(),
            JobKind::Services => "Set up services".into(),
            JobKind::InstallProton => "Install proton-drive".into(),
            JobKind::Import(o) => format!("Restore {}", o.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            JobKind::Export(o) => format!("Back up {}", o.target),
            JobKind::Upload(o, _) => format!("Back up {} to Proton Drive", o.target),
            JobKind::RemovePacmanLock => "Remove the pacman lock".into(),
            JobKind::PlasmaDeps(_) => "Install Plasma profile packages".into(),
            JobKind::Undo(_) => "Undo changes".into(),
            JobKind::Uninstall(_) => "Uninstall apps".into(),
            #[cfg(test)]
            JobKind::Fake(_) => "Fake job".into(),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, JobKind::Auto(..))
    }

    /// The steps of an automatic run, in order.
    pub fn steps(&self) -> Vec<&'static str> {
        match self {
            JobKind::Auto(_, plan) => plan.steps.iter().map(|s| s.label()).collect(),
            _ => Vec::new(),
        }
    }

    /// The package list the job installs, if it installs packages.
    pub fn packages(&self) -> Option<&PkgList> {
        match self {
            JobKind::Auto(Some(pl), plan) if plan.has(setup::Step::Packages) => Some(pl),
            JobKind::Packages(pl, _) => Some(pl),
            _ => None,
        }
    }
}

/// Sends what the engines report to the UI; `ask` blocks the job until the user answers.
pub struct TuiReporter {
    tx: Sender<Event>,
    next_id: AtomicU64,
    replies: Arc<super::Replies>,
}

impl TuiReporter {
    fn send(&self, m: JobMsg) {
        let _ = self.tx.send(Event::Job(m));
    }
}

fn level_tag(level: Level) -> &'static str {
    match level {
        Level::Warn => "WARN",
        Level::Error => "ERROR",
        _ => "INFO",
    }
}

impl Reporter for TuiReporter {
    fn log(&self, level: Level, msg: &str) {
        report::checkpoint();
        if level == Level::Blank {
            return;
        }
        // Vault messages only went to the terminal before; keep them in the log too
        let plain = crate::ui::strip_ansi(msg);
        ctx::log_file_only(level_tag(level), &plain);
        self.send(JobMsg::Log { level, line: plain });
    }

    fn progress(&self, p: &Progress) {
        report::checkpoint();
        self.send(JobMsg::Progress(p.clone()));
    }

    fn ask(&self, p: &Prompt) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = mpsc::channel();
        self.replies.insert(id, reply_tx);
        self.send(JobMsg::Ask { id, prompt: p.clone() });
        let yes = reply_rx.recv().unwrap_or(false);
        report::checkpoint();
        yes
    }

    fn file_logged(&self, level: &str, msg: &str) {
        let level = match level {
            "WARN" => Level::Warn,
            "ERROR" => Level::Error,
            _ => Level::Info,
        };
        self.send(JobMsg::Log { level, line: msg.to_string() });
    }

    fn output_line(&self, line: &str) {
        self.send(JobMsg::Log { level: Level::Raw, line: line.to_string() });
    }

    fn output_partial(&self, line: &str) {
        self.send(JobMsg::Transient(line.to_string()));
    }

    fn owns_terminal(&self) -> bool {
        true
    }
}

pub fn spawn(kind: JobKind, tx: Sender<Event>, replies: Arc<super::Replies>) {
    report::reset_cancel();
    ctx::set_canceled(false);
    let done_tx = tx.clone();
    let err_tx = tx.clone();
    let spawned = std::thread::Builder::new().name(JOB_THREAD.into()).spawn(move || {
        report::install(Arc::new(TuiReporter { tx, next_id: AtomicU64::new(1), replies }));
        let result = panic::catch_unwind(AssertUnwindSafe(|| run(kind)));
        ctx::cleanup_temp_state();
        // A restore from Proton Drive is done with its downloaded copy (decrypted app data)
        crate::drive::wipe_restore();
        let msg = match result {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(e)) if e.is::<Reported>() => Err("Action failed".to_string()),
            Ok(Err(e)) => Err(format!("{e:#}")),
            // A pacman stopped mid-run can leave its lock: the next pacman would fail
            Err(p) if p.is::<JobCanceled>() && ctx::stale_pacman_lock(ctx::pacman_interrupted(), std::time::Duration::from_secs(4)) => {
                Ok(Outcome::CanceledStaleLock("Canceled; pacman left its lock behind".into()))
            }
            Err(p) if p.is::<JobCanceled>() => Ok(Outcome::Canceled("Canceled".into())),
            Err(_) => Err("Internal error (details in the log)".into()),
        };
        let _ = done_tx.send(Event::Job(JobMsg::Done(msg)));
    });
    if let Err(e) = spawned {
        let _ = err_tx.send(Event::Job(JobMsg::Done(Err(format!("Could not start the job: {e}")))));
    }
}

fn finished(message: &str) -> Outcome {
    Outcome::Finished { message: message.into(), issues: ctx::take_issues() }
}

/// The changes of a job are recorded for undo (the automatic run records its own).
fn run(kind: JobKind) -> Result<Outcome> {
    let (step, dry) = match &kind {
        JobKind::Packages(..) => ("packages", false),
        JobKind::Repo => ("repo", false),
        JobKind::Services => ("services", false),
        JobKind::Partition(_) => ("partition", false),
        JobKind::InstallProton => ("packages", false),
        JobKind::Import(o) => ("import", o.dry_run),
        JobKind::Export(o) => ("export", o.dry_run),
        JobKind::Upload(..) => ("export", false),
        _ => return run_job(kind),
    };
    crate::journal::recorded(&[step], dry, || run_job(kind))
}

fn run_job(kind: JobKind) -> Result<Outcome> {
    if !kind.is_auto() {
        ctx::clear_issues();
    }
    Ok(match kind {
        JobKind::Auto(list, plan) => {
            if let Some(l) = list.as_deref().filter(|_| plan.has(setup::Step::Packages)) {
                report::log(Level::Info, &format!("Package list: {}", l.source_line()));
            }
            let out = setup::run_plan(&plan, list.as_deref())?;
            Outcome::Summary(Box::new(setup::plan_summary(&plan, &out, list.as_deref())))
        }
        JobKind::Partition(plan) => {
            let r = setup::setup_data_partition(&plan, false, 0, 1, 1)?;
            for (dir, why) in &r.kept {
                ctx::add_issue(format!("~/{dir} was kept where it is: {why}"));
            }
            finished(&format!("{} is mounted at {} ({})", plan.part.path, r.mountpoint, crate::partition::apply::UNDO_HINT))
        }
        JobKind::Repo => {
            setup::add_kud3n_repo(false, 0, 1, 1)?;
            finished("Repository step finished")
        }
        JobKind::Packages(list, selection) => {
            report::log(Level::Info, &format!("Package list: {}", list.source_line()));
            if let Some(s) = &selection {
                report::log(Level::Info, &format!("Selected {} of {} packages", s.len(), list.list.count()));
            }
            setup::install_packages(&list.list, selection.as_deref(), false, 0, 1, 1);
            finished("Package installation finished")
        }
        JobKind::Services => {
            setup::setup_services(false, 0, 1, 1)?;
            finished("Services step finished")
        }
        JobKind::InstallProton => {
            setup::install_one(crate::proton::PROTON_PKG, "Install proton-drive")?;
            crate::proton::forget_status();
            finished("proton-drive installed: open Import or Export again to use Proton Drive")
        }
        JobKind::Import(o) => {
            let dat = crate::vault::is_dat(&o.file);
            cmd_import(o)?;
            if ctx::canceled() {
                Outcome::Canceled("Nothing was restored".into())
            } else {
                finished(if dat { "Face model restored" } else { "Restore complete" })
            }
        }
        JobKind::Export(o) => {
            let out = o.output.clone().map(|p| p.display().to_string()).unwrap_or_default();
            cmd_backup(o)?;
            if ctx::canceled() { Outcome::Canceled("No backup was created".into()) } else { finished(&format!("Backup saved to {out}")) }
        }
        JobKind::Upload(o, up) => match crate::drive::backup_upload(o, &up)? {
            crate::drive::Uploaded::Done { remote, identical: true, .. } => finished(&format!("Already on Proton Drive (identical): {remote}")),
            crate::drive::Uploaded::Done { remote, trashed, .. } => {
                let n = trashed.iter().filter(|t| !t.ends_with(crate::drive::SIDECAR_EXT)).count();
                let extra = if n > 0 { format!("; {n} older moved to Drive's trash") } else { String::new() };
                finished(&format!("Uploaded to Proton Drive: {remote}{extra}"))
            }
            crate::drive::Uploaded::Unchanged { since } => finished(&format!("Nothing changed since the last upload ({since}): not uploaded")),
            crate::drive::Uploaded::Nothing => finished("Nothing was backed up"),
            crate::drive::Uploaded::Canceled => Outcome::Canceled("No backup was uploaded".into()),
        },
        JobKind::Undo(items) => {
            let dir = crate::journal::state_dir();
            let user = ctx::ctx().target_user.clone();
            let n = items.len() as i64;
            let out = crate::journal::recorded(&["undo"], false, || {
                crate::undo::execute(&items, &dir, &user, &mut |i, l| crate::ui::Dash::new(false, 0, 1, 1, "Undo changes").render(i as i64, n, l, l, None))
            })?;
            for f in &out.failed {
                ctx::add_issue(format!("Not undone: {f}"));
            }
            for n in &out.notes {
                ctx::add_issue(format!("Note: {n}"));
            }
            finished(&format!("{} change(s) undone", out.done.len()))
        }
        JobKind::Uninstall(items) => {
            let out = crate::journal::recorded(&["uninstall"], false, || crate::undo::run_uninstall(&items, &mut |l| report::log(Level::Step, l)));
            for f in &out.failed {
                ctx::add_issue(format!("Not removed: {f}"));
            }
            finished(&format!("{} app(s) removed", out.done.len()))
        }
        JobKind::PlasmaDeps(pkgs) => {
            let mut args = vec!["-S", "--needed", "--noconfirm"];
            args.extend(pkgs.iter().map(String::as_str));
            if !ctx::ok_logged("pacman", &args) {
                anyhow::bail!("pacman could not install {}", pkgs.join(" "));
            }
            finished(&format!("{} package(s) installed", pkgs.len()))
        }
        JobKind::RemovePacmanLock => {
            if ctx::remove_stale_pacman_lock() {
                report::log(Level::Success, &format!("Removed {}", ctx::PACMAN_LOCK));
                finished("pacman lock removed")
            } else {
                report::log(Level::Warn, &format!("{} is in use or already gone; left as it is", ctx::PACMAN_LOCK));
                finished("pacman lock left in place")
            }
        }
        #[cfg(test)]
        JobKind::Fake(steps) => {
            for step in steps {
                match step {
                    FakeStep::Progress(p) => report::progress(&p),
                    FakeStep::Log(level, line) => report::log(level, &line),
                    FakeStep::Ask(p) => {
                        if !report::ask(&p) {
                            return Ok(Outcome::Canceled("Declined".into()));
                        }
                    }
                    FakeStep::Fail(msg) => anyhow::bail!(msg),
                    FakeStep::Panic => panic!("injected panic"),
                }
            }
            finished("Fake job finished")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::Replies;
    use crate::tui::app::tests::{app, key, progress};
    use crate::tui::app::{App, Command, Screen, ToastKind, start_job, update};
    use crate::tui::screens::run::Finished;
    use ratatui::crossterm::event::KeyCode;
    use std::time::Duration;

    /// Drive a fake job through the real thread, channel and `update`; `on_ask` presses keys.
    fn drive(steps: Vec<FakeStep>, mut on_ask: impl FnMut(&mut App, &Replies) -> Option<Command>) -> App {
        let (tx, rx) = mpsc::channel();
        let replies = Arc::new(Replies::default());
        let mut a = app(80, 24);
        let Command::Start(kind) = start_job(&mut a, JobKind::Fake(steps)) else { panic!() };
        // Until the job has ended (its cleanup wipes the vault's staging folder)
        let _staging = ctx::staging_test_lock();
        spawn(kind, tx, replies.clone());
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(10)).expect("job hung");
            let done = matches!(ev, Event::Job(JobMsg::Done(_)));
            let mut cmd = update(&mut a, ev);
            if a.modal.is_some() {
                cmd = on_ask(&mut a, &replies).or(cmd);
            }
            match cmd {
                Some(Command::Answer { id, yes }) => replies.answer(id, yes),
                Some(Command::Cancel) => {
                    report::request_cancel();
                    replies.decline_all();
                }
                _ => {}
            }
            if done {
                return a;
            }
        }
    }

    fn run_state(a: &App) -> &crate::tui::screens::run::RunState {
        match &a.screen {
            Screen::Run(r) => r,
            _ => panic!("not on the run screen"),
        }
    }

    #[test]
    fn fake_job_round_trip() {
        let steps = vec![
            FakeStep::Progress(progress(1, 1, 2, "Refreshing databases", None)),
            FakeStep::Log(Level::Warn, "Application 'zen' is running (PID: 42).".into()),
            FakeStep::Ask(Prompt::yes_no("Close/kill running application(s) (zen)?", true)),
            FakeStep::Log(Level::Success, "zen terminated successfully.".into()),
        ];
        let a = drive(steps.clone(), |a, _| update(a, key(KeyCode::Enter)));
        let r = run_state(&a);
        assert_eq!(r.finished, Some(Finished::Ok { message: "Fake job finished".into(), issues: vec![] }));
        assert_eq!(r.log.len(), 2);
        assert_eq!(r.progress.as_ref().unwrap().item, "Refreshing databases");

        // Esc declines: the fake job treats that as canceled
        let a = drive(steps, |a, _| update(a, key(KeyCode::Esc)));
        assert_eq!(run_state(&a).finished, Some(Finished::Canceled("Declined".into())));

        // A failing job goes back to the menu with an error toast
        let a = drive(vec![FakeStep::Fail("boom".into())], |_, _| None);
        assert!(matches!(a.screen, Screen::Menu));
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Error);
        // So does a panic inside the job; the UI keeps running
        let a = drive(vec![FakeStep::Panic], |_, _| None);
        assert!(matches!(a.screen, Screen::Menu));
        assert!(a.toast.as_ref().unwrap().text.starts_with("Internal error"));

        // Cancel while a question is open: declined, then the job unwinds at its checkpoint
        let steps = vec![FakeStep::Ask(Prompt::yes_no("Proceed?", false)), FakeStep::Log(Level::Info, "must not run".into())];
        let a = drive(steps, |a, _| {
            update(a, crate::tui::app::tests::ctrl('c'));
            update(a, key(KeyCode::Char('y')))
        });
        let r = run_state(&a);
        assert_eq!(r.finished, Some(Finished::Canceled("Canceled".into())));
        assert_eq!(r.log.len(), 0);
        report::reset_cancel();
    }
}
