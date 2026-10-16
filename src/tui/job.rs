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
use crate::report::{self, JobCanceled, Level, Progress, Prompt, Reporter};
use crate::setup;
use crate::vault::backup::{BackupOpts, cmd_backup};
use crate::vault::import::{ImportOpts, cmd_import};

#[derive(Debug)]
pub enum JobKind {
    Auto,
    Partition,
    Repo,
    Packages,
    Services,
    Import(ImportOpts),
    Export(BackupOpts),
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
            JobKind::Auto => "Automatic setup".into(),
            JobKind::Partition => "Set up data partition".into(),
            JobKind::Repo => "Add kud3n's repository".into(),
            JobKind::Packages => "Install packages".into(),
            JobKind::Services => "Set up services".into(),
            JobKind::Import(o) => format!("Restore {}", o.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            JobKind::Export(o) => format!("Back up {}", o.target),
            #[cfg(test)]
            JobKind::Fake(_) => "Fake job".into(),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, JobKind::Auto)
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
        let msg = match result {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(e)) if e.is::<Reported>() => Err("Action failed".to_string()),
            Ok(Err(e)) => Err(format!("{e:#}")),
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

fn run(kind: JobKind) -> Result<Outcome> {
    if !kind.is_auto() {
        ctx::clear_issues();
    }
    Ok(match kind {
        JobKind::Auto => {
            let start = setup::run_automatic_steps()?;
            Outcome::Summary(setup::collect_summary(start))
        }
        JobKind::Partition => {
            setup::setup_data_partition(false, 0, 1, 1)?;
            finished("Data partition step finished")
        }
        JobKind::Repo => {
            setup::add_kud3n_repo(false, 0, 1, 1)?;
            finished("Repository step finished")
        }
        JobKind::Packages => {
            setup::install_packages(false, 0, 1, 1);
            finished("Package installation finished")
        }
        JobKind::Services => {
            setup::setup_services(false, 0, 1, 1)?;
            finished("Services step finished")
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
