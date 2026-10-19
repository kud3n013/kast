//! App state and the pure `update` / `view` pair. `update` never does I/O: anything slow or
//! side-effecting comes back as a `Command` for the runtime in `mod.rs` to carry out.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;

pub use super::event::Event;
use super::event::{JobMsg, Loaded, Outcome};
use super::job::JobKind;
use super::screens::drive;
use super::screens::export::{self, ExportState};
use super::screens::help;
use super::screens::import::{self, ImportState};
use super::screens::log::{self, LogState};
use super::screens::menu::{self, Action, ITEMS, MenuState};
use super::screens::packages::{self, Installed, PackagesState, PkgState};
use super::screens::plasma::{self, PlasmaReq, PlasmaState};
use super::screens::run::{self, Finished, RunState};
use super::screens::status::{self, StatusState};
use super::screens::summary;
use super::screens::undo::{self as undo_screen, UndoState};
use super::screens::wizard::{self, Wizard};
use super::theme::Theme;
use super::widgets::{self, Dialog, MIN_H, MIN_W};
use crate::setup::Summary;

/// Who kast runs for, shown in the header bar (copied out of `ctx()` so drawing stays pure).
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub host: String,
    pub root: bool,
    pub log_file: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Ok,
    Error,
    Info,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    /// `App::now_ms` after which it disappears
    pub until_ms: u64,
}

#[derive(Debug)]
pub enum Screen {
    Menu,
    Run(Box<RunState>),
    Summary(Box<Summary>),
    Import(Box<ImportState>),
    Export(Box<ExportState>),
    Status(Box<StatusState>),
    /// Option 4: the app picker
    Packages(Box<PackagesState>),
    /// Option 1: the steps, a stage for each that needs choices, a review
    Auto(Box<Wizard>),
    /// `u` undo changes, `x` uninstall apps
    Undo(Box<UndoState>),
    /// `s` the scheduled backup
    Schedule(Box<super::screens::schedule::ScheduleState>),
    /// `p` KDE Plasma settings: profiles, save, apply, export, import, config check
    Plasma(Box<PlasmaState>),
    /// Option 2: pick a partition, mount point, options, folders, review
    Partition(Box<super::screens::partition::PartState>),
    /// The log file, opened from the screen kept alongside
    Log(Box<LogState>, Box<Screen>),
}

#[derive(Debug, Clone)]
pub enum ModalKind {
    /// An engine question; the job waits for the answer
    Ask(u64),
    CancelJob,
    /// Start this job on Yes
    Confirm(Box<JobKind>),
    /// Automatic run with no app checked: Yes drops the packages step
    SkipPackages,
    /// Delete these runs' file copies
    Prune(Vec<String>),
    /// Install proton-drive from the repository
    DriveInstall,
    /// Information about Proton Drive; Yes checks again
    DriveRecheck,
    /// Keyring locked: Yes keeps the session in pass from now on
    DriveUsePass,
    DriveSignOut,
    /// Run this Plasma request on Yes
    Plasma(Box<PlasmaReq>),
}

/// A yes/no dialog over the current screen. Esc always answers No.
#[derive(Debug, Clone)]
pub struct Modal {
    pub kind: ModalKind,
    pub title: String,
    pub text: String,
    pub details: Vec<String>,
    pub yes: &'static str,
    pub no: &'static str,
    pub yes_selected: bool,
}

/// Side effects requested by `update`.
#[derive(Debug)]
pub enum Command {
    /// Start a job thread (the run screen is already showing)
    Start(JobKind),
    /// Reply to an engine question
    Answer {
        id: u64,
        yes: bool,
    },
    /// Stop the running job
    Cancel,
    /// Read the log file (answered with `Loaded::Log`)
    OpenLog,
    /// Find backups (answered with `Loaded::Candidates`)
    LoadCandidates {
        dir: std::path::PathBuf,
        user: String,
        zips_only: bool,
    },
    /// Which of these packages are installed (answered with `Loaded::Installed`)
    LoadInstalled(Vec<String>),
    /// Measure what a restore's snapshot would copy (answered with `Loaded::SnapshotSize`)
    SnapshotSize {
        file: std::path::PathBuf,
        apps: Vec<String>,
    },
    /// Look for btrfs and snapshot tools (answered with `Loaded::Snapshots`)
    DetectSnapshots,
    /// Read the journal (answered with `Loaded::Undo`)
    LoadUndo,
    /// Find the installed apps (answered with `Loaded::Uninstall`)
    LoadUninstall,
    /// Delete these runs' file copies (answered with `Loaded::Pruned`)
    PruneUndo(Vec<String>),
    /// Create a folder as the target user (answered with `Loaded::DirCreated`)
    MkdirUser(std::path::PathBuf),
    /// Look at a backup file (answered with `Loaded::Inspected`)
    Inspect {
        path: std::path::PathBuf,
        pick: bool,
    },
    /// Tab completion of a typed path (answered with `Loaded::Completion`)
    Complete(String),
    /// Detect what can be backed up (answered with `Loaded::ExportTargets`)
    LoadExportTargets,
    /// Collect the app status (answered with `Loaded::Status`)
    LoadStatus,
    /// Show the log file in a pager (or an editor) on the real terminal, the app suspended
    ViewLog {
        editor: bool,
    },
    /// Open the log file with the desktop's default app (answered with `Loaded::LogIo`)
    OpenLogFile,
    /// Show the log file in the file manager (answered with `Loaded::LogIo`)
    ShowLogFile,
    /// Copy the log of this session or job (answered with `Loaded::LogIo` or `Loaded::Osc52`)
    CopyLog(LogScope),
    /// Copy the log file's path (answered like `CopyLog`)
    CopyLogPath,
    /// Write this OSC 52 clipboard sequence to the terminal, between frames
    Osc52(Vec<u8>),
    /// Download the package list again (answered with `Loaded::PackageList`)
    FetchPackages,
    /// Several of these, in order
    Many(Vec<Command>),
    /// Find proton-drive and check the sign-in (answered with `Loaded::Drive`)
    DriveCheck {
        refresh: bool,
    },
    /// Start `proton-drive auth login` (answered with `Loaded::SignInUrl`, then `SignInDone`)
    DriveLogin,
    /// Stop the sign-in process
    DriveLoginCancel,
    /// `proton-drive auth logout` (answered with `Loaded::DriveLoggedOut`)
    DriveLogout,
    /// Save the credential store setting (answered with `Loaded::DriveStore`)
    DriveSetStore(String),
    /// The machines with backups on Drive (answered with `Loaded::DriveHosts`)
    DriveHosts,
    /// A machine's backups and sidecars (answered with `Loaded::DriveBackups`)
    DriveBackups(String),
    /// Download a backup to the restore staging folder (answered with `Loaded::DriveDownloaded`)
    DriveDownload(Box<crate::drive::RemoteBackup>),
    DriveCancelTransfer,
    /// Wipe the restore staging folder
    DriveWipeRestore,
    /// Read the scheduled backup's state (answered with `Loaded::Schedule`)
    LoadSchedule,
    /// Turn the schedule on (or update it), with this local copy folder (answered with `Loaded::ScheduleDone`)
    ScheduleApply(crate::schedule::Spec, Option<std::path::PathBuf>),
    ScheduleDisable,
    ScheduleRun,
    CopySignIn(crate::proton::SignInUrl),
    OpenSignIn(crate::proton::SignInUrl),
    /// A Plasma settings request (answered with `Loaded::Plasma`)
    Plasma(PlasmaReq),
    /// Look at the disks and /etc/fstab (answered with `Loaded::Partitions`)
    LoadPartitions,
    /// Check a typed mount point on the machine (answered with `Loaded::Mountpoint`)
    CheckMountpoint(String),
    /// List the subvolumes of a btrfs partition (answered with `Loaded::Subvolumes`)
    LoadSubvolumes(Box<crate::partition::Partition>),
}

/// Which part of the log `y` copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogScope {
    /// Since the app started
    Session,
    /// Since the last job started
    Job,
}

pub struct App {
    pub header: Header,
    pub theme: Theme,
    pub size: (u16, u16),
    /// Wall clock (seconds) and a millisecond clock, set by the runtime
    pub now: i64,
    pub now_ms: u64,
    /// Spinner frame, advanced on ticks
    pub frame: usize,
    pub screen: Screen,
    pub menu: MenuState,
    pub help: bool,
    pub modal: Option<Modal>,
    pub toast: Option<Toast>,
    /// The package list, fetched in the background when the app opens
    pub packages: PkgState,
    pub installed: Installed,
    /// Whether /etc/pacman.conf has `[archrepo]` (read once; the picker leaves out
    /// archrepo's packages that can't be installed without it)
    pub repo_configured: bool,
    /// The automatic run's choices, kept while the menu shows
    pub wizard: Option<Box<Wizard>>,
    /// What the machine has for snapshots (looked at once, when the automatic run opens)
    pub snapshots: Option<crate::snapshot::Detected>,
    /// Proton Drive as last checked, and whether a check is running
    pub drive: Option<Box<super::screens::drive::DriveInfo>>,
    pub drive_checking: bool,
    /// The sign-in window
    pub signin: Option<super::screens::drive::SignIn>,
    /// Folder check for the default folders (tests replace it)
    pub dir_exists: fn(&std::path::Path) -> bool,
    pub quit: bool,
    last_click: Option<(usize, u64)>,
}

const DOUBLE_CLICK_MS: u64 = 400;
const PAGE: usize = 10;

pub fn is_ctrl_c(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'C'))
}

impl App {
    pub fn new(header: Header, theme: Theme, size: (u16, u16)) -> Self {
        App {
            header,
            theme,
            size,
            now: 0,
            now_ms: 0,
            frame: 0,
            screen: Screen::Menu,
            menu: MenuState::default(),
            help: false,
            modal: None,
            toast: None,
            packages: PkgState::Fetching,
            installed: Installed::Unknown,
            repo_configured: !cfg!(test) && crate::setup::repo_configured(),
            wizard: None,
            snapshots: None,
            drive: None,
            drive_checking: false,
            signin: None,
            dir_exists: |p| p.is_dir(),
            quit: false,
            last_click: None,
        }
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind, until_ms: self.now_ms + 5000 });
    }

    pub fn job_running(&self) -> bool {
        matches!(&self.screen, Screen::Run(r) if r.running())
    }

    /// Whether something on screen animates (the runtime only ticks then).
    pub fn animating(&self) -> bool {
        let loading = match &self.screen {
            Screen::Status(s) => s.loading,
            Screen::Import(s) => s.busy(),
            Screen::Export(s) => s.loading,
            Screen::Packages(_) => matches!(self.packages, PkgState::Fetching) || self.installed == Installed::Loading,
            Screen::Undo(s) => s.loading(),
            Screen::Schedule(s) => s.loading || s.busy,
            Screen::Plasma(s) => s.working(),
            Screen::Partition(s) => s.busy(),
            Screen::Auto(w) => match w.stage {
                wizard::Stage::Partition => w.partition.as_ref().is_some_and(|p| p.busy()),
                wizard::Stage::Upgrade => self.snapshots.is_none(),
                wizard::Stage::Apps => matches!(self.packages, PkgState::Fetching) || self.installed == Installed::Loading,
                wizard::Stage::Import => w.import.as_ref().is_some_and(ImportState::busy),
                wizard::Stage::Export => w.export.as_ref().is_some_and(|s| s.loading),
                _ => false,
            },
            _ => false,
        };
        self.toast.is_some() || self.job_running() || loading || self.signin.is_some() || self.drive_checking
    }

    fn too_small(&self) -> bool {
        self.size.0 < MIN_W || self.size.1 < MIN_H
    }

    pub(crate) fn body_area(&self) -> Rect {
        let [_, body, _] = areas(Rect::new(0, 0, self.size.0, self.size.1));
        body
    }

    fn help_keys(&self) -> (&'static str, &'static [(&'static str, &'static str)]) {
        match &self.screen {
            Screen::Menu => ("Main menu", &menu::HELP),
            Screen::Run(_) => ("Progress", &run::HELP),
            Screen::Summary(_) => ("Summary", &summary::HELP),
            Screen::Import(s) if s.mode == import::Mode::Inspect => ("Inspect a backup", &import::HELP),
            Screen::Import(_) => ("Backups", &import::HELP),
            Screen::Status(_) => ("App status", &status::HELP),
            Screen::Export(_) => ("Back up", &export::HELP),
            Screen::Log(..) => ("Log", &log::HELP),
            Screen::Packages(_) => ("Install packages", &packages::HELP),
            Screen::Auto(w) => wizard::help(w),
            Screen::Undo(s) if s.mode == undo_screen::Mode::Uninstall => ("Uninstall apps", &undo_screen::HELP),
            Screen::Undo(_) => ("Undo changes", &undo_screen::HELP),
            Screen::Schedule(_) => ("Scheduled backup", &super::screens::schedule::HELP),
            Screen::Plasma(_) => ("KDE Plasma settings", &plasma::HELP),
            Screen::Partition(_) => ("Data partition", &super::screens::partition::HELP),
        }
    }

    fn run_state(&mut self) -> Option<&mut RunState> {
        match &mut self.screen {
            Screen::Run(r) => Some(r),
            _ => None,
        }
    }
}

fn areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area)
}

pub fn update(app: &mut App, ev: Event) -> Option<Command> {
    match ev {
        Event::Resize(w, h) => {
            app.size = (w, h);
            None
        }
        Event::Tick => {
            app.frame = app.frame.wrapping_add(1);
            if app.toast.as_ref().is_some_and(|t| app.now_ms >= t.until_ms) {
                app.toast = None;
            }
            None
        }
        Event::Paste(text) => {
            import::on_paste(app, &text);
            export::on_paste(app, &text);
            plasma::on_paste(app, &text);
            None
        }
        Event::Key(k) if k.kind == KeyEventKind::Release => None,
        Event::Key(k) => on_key(app, k),
        Event::Mouse(m) => on_mouse(app, m),
        Event::Job(m) => on_job(app, m),
        Event::Loaded(Loaded::LogIo(r)) => {
            app.toast(if r.ok { ToastKind::Ok } else { ToastKind::Error }, r.text);
            None
        }
        Event::Loaded(Loaded::Osc52 { bytes, report }) => {
            app.toast(ToastKind::Info, report.text);
            Some(Command::Osc52(bytes))
        }
        Event::Loaded(Loaded::PackageList(pl)) => {
            app.packages = PkgState::Ready(std::sync::Arc::new(*pl));
            app.installed = Installed::Unknown;
            packages::sync(app)
        }
        Event::Loaded(Loaded::SnapshotSize { file, bytes }) => {
            import::on_snapshot_size(app, file, bytes);
            None
        }
        Event::Loaded(Loaded::Snapshots(d)) => {
            wizard::on_detected(app, d);
            None
        }
        Event::Loaded(Loaded::Undo(d)) => {
            undo_screen::on_undo(app, *d);
            None
        }
        Event::Loaded(Loaded::Uninstall(d)) => {
            undo_screen::on_uninstall(app, *d);
            None
        }
        Event::Loaded(Loaded::Pruned(freed)) => {
            app.toast(ToastKind::Ok, format!("Pruned: {} freed", crate::undo::human(freed)));
            Some(Command::LoadUndo)
        }
        Event::Loaded(Loaded::Drive(info)) => {
            drive::on_info(app, *info);
            None
        }
        Event::Loaded(Loaded::SignInUrl(url)) => drive::on_url(app, url),
        Event::Loaded(Loaded::SignInDone(r)) => drive::on_done(app, r),
        Event::Loaded(Loaded::DriveLoggedOut(r)) => drive::on_logged_out(app, r),
        Event::Loaded(Loaded::DriveStore(r)) => drive::on_store_set(app, r),
        Event::Loaded(Loaded::Schedule(i)) => {
            super::screens::schedule::on_info(app, *i);
            None
        }
        Event::Loaded(Loaded::ScheduleDone(r)) => super::screens::schedule::on_done(app, r),
        Event::Loaded(Loaded::Plasma(r)) => plasma::on_resp(app, *r),
        Event::Loaded(Loaded::Partitions(r)) => {
            super::screens::partition::on_scan(app, r);
            None
        }
        Event::Loaded(Loaded::Mountpoint { raw, result }) => super::screens::partition::on_mountpoint(app, raw, result),
        Event::Loaded(Loaded::Subvolumes(r)) => {
            super::screens::partition::on_subvolumes(app, r);
            None
        }
        Event::Loaded(Loaded::DriveHosts(r)) => {
            import::on_hosts(app, r);
            None
        }
        Event::Loaded(Loaded::DriveBackups { host, result }) => import::on_backups(app, host, result),
        Event::Loaded(Loaded::DriveDownloaded { remote, result }) => import::on_downloaded(app, remote, result),
        Event::Loaded(Loaded::Installed(set)) => {
            app.installed = Installed::Known(set);
            packages::sync(app)
        }
        Event::Loaded(Loaded::DirCreated { path, result }) => {
            export::on_dir_created(app, path, result);
            None
        }
        Event::Loaded(Loaded::Log(lines)) => {
            let back = std::mem::replace(&mut app.screen, Screen::Menu);
            app.screen = Screen::Log(Box::new(LogState::from_lines(lines)), Box::new(back));
            None
        }
        Event::Loaded(Loaded::Candidates(list)) => import::on_candidates(app, list),
        Event::Loaded(Loaded::Inspected { path, pick, result }) => import::on_inspected(app, path, pick, result),
        Event::Loaded(Loaded::Completion { value, matches }) => {
            export::on_completion(app, value.clone(), matches.clone());
            super::screens::partition::on_completion(app, value.clone(), matches.clone());
            import::on_completion(app, value, matches);
            None
        }
        Event::Loaded(Loaded::Status(r)) => {
            status::on_report(app, r);
            None
        }
        Event::Loaded(Loaded::ExportTargets { targets, ts }) => {
            export::on_targets(app, targets, ts);
            None
        }
    }
}

fn on_job(app: &mut App, m: JobMsg) -> Option<Command> {
    if let JobMsg::Ask { id, prompt } = m {
        if app.run_state().is_none_or(|r| r.cancel_requested) {
            return Some(Command::Answer { id, yes: false });
        }
        app.help = false;
        app.modal = Some(Modal {
            kind: ModalKind::Ask(id),
            title: "Question".into(),
            text: prompt.text,
            details: prompt.details,
            yes: "Yes",
            no: "No",
            yes_selected: prompt.default,
        });
        return None;
    }
    if let JobMsg::Done(result) = m {
        // A job that ends while one of its dialogs is open no longer needs the answer
        app.modal = None;
        // An upload may have found the session expired, an install added proton-drive:
        // check Proton Drive again next time it's needed
        app.drive = None;
        match result {
            Err(e) => {
                app.screen = Screen::Menu;
                app.toast(ToastKind::Error, format!("{e}. Details: {}", app.header.log_file));
            }
            Ok(Outcome::Summary(s)) => {
                app.toast = None;
                app.screen = Screen::Summary(s);
            }
            Ok(outcome) => {
                let stale_lock = matches!(outcome, Outcome::CanceledStaleLock(_));
                let r = app.run_state()?;
                let (finished, toast) = match outcome {
                    Outcome::Summary(_) => unreachable!("handled above"),
                    Outcome::Finished { message, issues } => {
                        let kind = if issues.is_empty() { ToastKind::Ok } else { ToastKind::Error };
                        (Finished::Ok { message: message.clone(), issues }, (kind, message))
                    }
                    Outcome::Canceled(msg) | Outcome::CanceledStaleLock(msg) => (Finished::Canceled(msg.clone()), (ToastKind::Info, msg)),
                };
                r.finished = Some(finished);
                r.scroll.bottom();
                app.toast(toast.0, toast.1);
                if stale_lock {
                    app.modal = Some(Modal {
                        kind: ModalKind::Confirm(Box::new(JobKind::RemovePacmanLock)),
                        title: "pacman lock".into(),
                        text: "Remove the lock pacman left behind?".into(),
                        details: vec![
                            format!("pacman was stopped mid-run and left {} behind.", crate::ctx::PACMAN_LOCK),
                            "No pacman is running now. Until the lock is gone, every pacman run".into(),
                            "fails with \"unable to lock database\".".into(),
                        ],
                        yes: "Remove",
                        no: "Keep",
                        yes_selected: false,
                    });
                }
            }
        }
        return None;
    }
    let r = app.run_state()?;
    match m {
        JobMsg::Progress(p) => r.on_progress(p),
        JobMsg::Log { level, line } => r.push_log(level, line),
        JobMsg::Transient(line) => r.log.set_transient(line),
        JobMsg::Ask { .. } | JobMsg::Done(_) => {}
    }
    None
}

fn on_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    if app.signin.is_some() {
        return drive::signin_key(app, k);
    }
    if app.modal.is_some() {
        return modal_key(app, k);
    }
    if is_ctrl_c(&k) {
        return match &app.screen {
            Screen::Run(_) if app.job_running() => ask_cancel(app),
            Screen::Run(_) => {
                app.screen = Screen::Menu;
                None
            }
            Screen::Menu => {
                app.quit = true;
                None
            }
            Screen::Auto(_) => {
                wizard::leave(app);
                None
            }
            Screen::Import(_) => {
                app.screen = Screen::Menu;
                Some(Command::DriveWipeRestore)
            }
            Screen::Summary(_)
            | Screen::Export(_)
            | Screen::Status(_)
            | Screen::Packages(_)
            | Screen::Undo(_)
            | Screen::Schedule(_)
            | Screen::Partition(_)
            | Screen::Plasma(_) => {
                app.screen = Screen::Menu;
                None
            }
            Screen::Log(..) => {
                go_back(app);
                None
            }
        };
    }
    if app.help {
        app.help = false;
        return None;
    }
    if app.too_small() {
        if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
            if app.job_running() {
                return ask_cancel(app);
            }
            app.quit = true;
        }
        return None;
    }
    if k.code == KeyCode::Char('?') && !text_field_open(app) {
        app.help = true;
        return None;
    }
    match app.screen {
        Screen::Menu => menu_key(app, k),
        Screen::Run(_) => run_key(app, k),
        Screen::Summary(_) => match k.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                app.screen = Screen::Menu;
                None
            }
            KeyCode::Char('l') => Some(Command::OpenLog),
            _ => log_action(&k, LogScope::Job, false),
        },
        Screen::Log(..) => log_key(app, k),
        Screen::Import(_) => import::key(app, k),
        Screen::Export(_) => export::key(app, k),
        Screen::Status(_) => status::key(app, k),
        Screen::Packages(_) => packages::key(app, k),
        Screen::Auto(_) => wizard::key(app, k),
        Screen::Undo(_) => undo_screen::key(app, k),
        Screen::Schedule(_) => super::screens::schedule::key(app, k),
        Screen::Plasma(_) => plasma::key(app, k),
        Screen::Partition(_) => super::screens::partition::key(app, k),
    }
}

/// A text field has the keyboard (letters must reach it, not shortcuts).
fn text_field_open(app: &App) -> bool {
    import::state_ref(app).is_some_and(ImportState::typing) || export::editing(app) || plasma::editing(app) || super::screens::partition::editing(app)
}

fn modal_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let m = app.modal.as_mut()?;
    if is_ctrl_c(&k) {
        // Ctrl+C on an engine question: answer No, then offer to cancel the whole job
        let cmd = answer(app, false);
        if app.job_running() {
            ask_cancel(app);
        }
        return cmd;
    }
    match k.code {
        KeyCode::Char('y' | 'Y') => answer(app, true),
        KeyCode::Char('r') if matches!(m.kind, ModalKind::DriveRecheck) => answer(app, true),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => answer(app, false),
        KeyCode::Enter => {
            let yes = m.yes_selected;
            answer(app, yes)
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h' | 'l') => {
            m.yes_selected = !m.yes_selected;
            None
        }
        _ => None,
    }
}

fn answer(app: &mut App, yes: bool) -> Option<Command> {
    let m = app.modal.take()?;
    match m.kind {
        ModalKind::Ask(id) => Some(Command::Answer { id, yes }),
        ModalKind::CancelJob if yes => {
            let r = app.run_state()?;
            r.cancel_requested = true;
            Some(Command::Cancel)
        }
        ModalKind::CancelJob => None,
        ModalKind::Confirm(kind) if yes => Some(start_job(app, *kind)),
        ModalKind::Confirm(_) => None,
        ModalKind::SkipPackages if yes => wizard::skip_packages(app),
        ModalKind::SkipPackages => None,
        ModalKind::Prune(ids) if yes => Some(Command::PruneUndo(ids)),
        ModalKind::Prune(_) => None,
        ModalKind::DriveInstall if yes => Some(start_job(app, JobKind::InstallProton)),
        ModalKind::DriveRecheck if yes => Some(drive::check(app, true)),
        ModalKind::DriveUsePass if yes => Some(Command::DriveSetStore("pass".into())),
        ModalKind::DriveSignOut if yes => Some(Command::DriveLogout),
        ModalKind::Plasma(req) if yes => {
            if let Screen::Plasma(s) = &mut app.screen {
                s.busy = true;
            }
            Some(Command::Plasma(*req))
        }
        ModalKind::DriveInstall | ModalKind::DriveRecheck | ModalKind::DriveUsePass | ModalKind::DriveSignOut | ModalKind::Plasma(_) => None,
    }
}

fn ask_cancel(app: &mut App) -> Option<Command> {
    if app.run_state().is_some_and(|r| r.cancel_requested) {
        return None;
    }
    app.modal = Some(Modal {
        kind: ModalKind::CancelJob,
        title: "Cancel".into(),
        text: "Cancel this job?".into(),
        details: vec!["The running command is stopped and the job ends at its next step.".into(), "Steps that already finished are not undone.".into()],
        yes: "Cancel job",
        no: "Keep running",
        yes_selected: false,
    });
    None
}

/// `e E o f y Y`: get the log out of kast. While a job runs only the copies are allowed (the
/// others suspend the terminal or start programs).
fn log_action(k: &KeyEvent, scope: LogScope, running: bool) -> Option<Command> {
    if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match k.code {
        KeyCode::Char('y') => Some(Command::CopyLog(scope)),
        KeyCode::Char('Y') => Some(Command::CopyLogPath),
        _ if running => None,
        KeyCode::Char('e') => Some(Command::ViewLog { editor: false }),
        KeyCode::Char('E') => Some(Command::ViewLog { editor: true }),
        KeyCode::Char('o') => Some(Command::OpenLogFile),
        KeyCode::Char('f') => Some(Command::ShowLogFile),
        _ => None,
    }
}

fn menu_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    if let Some(cmd) = log_action(&k, LogScope::Session, false) {
        return Some(cmd);
    }
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => app.menu.up(),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => app.menu.down(),
        KeyCode::Home | KeyCode::Char('g') => app.menu.selected = 0,
        KeyCode::End | KeyCode::Char('G') => app.menu.selected = ITEMS.len() - 1,
        KeyCode::Enter => return activate(app, app.menu.action()),
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('l') => return Some(Command::OpenLog),
        KeyCode::Char(c @ ('u' | 'x' | 's' | 'p')) => {
            app.menu.selected = ITEMS.iter().position(|i| i.key == c).unwrap_or(0);
            return activate(app, app.menu.action());
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = c as usize - '1' as usize;
            app.menu.selected = idx;
            return activate(app, ITEMS[idx].action);
        }
        _ => {}
    }
    None
}

fn run_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let running = app.job_running();
    if let Some(cmd) = log_action(&k, LogScope::Job, running) {
        return Some(cmd);
    }
    let r = app.run_state()?;
    let len = r.log.len();
    match k.code {
        KeyCode::PageUp => r.scroll.up(PAGE, len),
        KeyCode::PageDown => r.scroll.down(PAGE),
        KeyCode::Up | KeyCode::Char('k') => r.scroll.up(1, len),
        KeyCode::Down | KeyCode::Char('j') => r.scroll.down(1),
        KeyCode::Home | KeyCode::Char('g') => r.scroll.top(len),
        KeyCode::End | KeyCode::Char('G') => r.scroll.bottom(),
        KeyCode::Esc | KeyCode::Char('q') if running => return ask_cancel(app),
        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') if !running => app.screen = Screen::Menu,
        KeyCode::Char('l') if !running => return Some(Command::OpenLog),
        _ => {}
    }
    None
}

fn log_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    if let Some(cmd) = log_action(&k, LogScope::Session, false) {
        return Some(cmd);
    }
    let Screen::Log(st, _) = &mut app.screen else { return None };
    let len = st.log.len();
    match k.code {
        KeyCode::PageUp => st.scroll.up(PAGE, len),
        KeyCode::PageDown => st.scroll.down(PAGE),
        KeyCode::Up | KeyCode::Char('k') => st.scroll.up(1, len),
        KeyCode::Down | KeyCode::Char('j') => st.scroll.down(1),
        KeyCode::Home | KeyCode::Char('g') => st.scroll.top(len),
        KeyCode::End | KeyCode::Char('G') => st.scroll.bottom(),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => go_back(app),
        _ => {}
    }
    None
}

/// Leave the log viewer for the screen it was opened from.
fn go_back(app: &mut App) {
    if let Screen::Log(_, back) = std::mem::replace(&mut app.screen, Screen::Menu) {
        app.screen = *back;
    }
}

/// Start what a menu entry stands for.
pub fn activate(app: &mut App, action: Action) -> Option<Command> {
    let kind = match action {
        Action::Quit => {
            app.quit = true;
            return None;
        }
        Action::Auto => return wizard::open(app),
        Action::Packages => {
            app.screen = Screen::Packages(Box::new(PackagesState::new(false)));
            return packages::sync(app);
        }
        Action::Partition => return super::screens::partition::open(app),
        Action::Repo => JobKind::Repo,
        Action::Services => JobKind::Services,
        Action::Import => {
            let dir = import::default_dir(&app.header.home, app.dir_exists);
            app.screen = Screen::Import(Box::new(ImportState::new(import::Mode::Restore, false, dir)));
            return drive::check_if_unknown(app);
        }
        Action::Export => {
            let dir = export::default_dir(&app.header.home, app.dir_exists);
            app.screen = Screen::Export(Box::new(ExportState::new(&app.header.user, false, dir)));
            return Some(match drive::check_if_unknown(app) {
                Some(check) => Command::Many(vec![Command::LoadExportTargets, check]),
                None => Command::LoadExportTargets,
            });
        }
        Action::Status => {
            app.screen = Screen::Status(Box::new(StatusState { loading: true, ..Default::default() }));
            return Some(Command::LoadStatus);
        }
        Action::Undo => {
            app.screen = Screen::Undo(Box::new(UndoState::new(undo_screen::Mode::Undo)));
            return Some(Command::LoadUndo);
        }
        Action::Uninstall => {
            app.screen = Screen::Undo(Box::new(UndoState::new(undo_screen::Mode::Uninstall)));
            return Some(Command::LoadUninstall);
        }
        Action::Schedule => return super::screens::schedule::open(app),
        Action::Plasma => return plasma::open(app),
        Action::Info => {
            let dir = import::default_dir(&app.header.home, app.dir_exists);
            app.screen = Screen::Import(Box::new(ImportState::new(import::Mode::Inspect, false, dir)));
            return drive::check_if_unknown(app);
        }
    };
    Some(start_job(app, kind))
}

pub fn start_job(app: &mut App, kind: JobKind) -> Command {
    app.toast = None;
    let mut run = RunState::new(kind.title(), kind.is_auto(), app.now);
    if kind.is_auto() {
        run.steps = kind.steps();
    }
    run.packages = kind.packages().map(|pl| packages::source_text(&app.theme, pl));
    app.screen = Screen::Run(Box::new(run));
    Command::Start(kind)
}

fn on_mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    if app.help || app.modal.is_some() || app.signin.is_some() || app.too_small() {
        return None;
    }
    if matches!(app.screen, Screen::Import(_)) {
        let body = app.body_area();
        return import::mouse(app, m, body);
    }
    if matches!(app.screen, Screen::Export(_)) {
        return export::mouse(app, m);
    }
    if matches!(app.screen, Screen::Status(_)) {
        status::mouse(app, m);
        return None;
    }
    match &mut app.screen {
        Screen::Menu => match m.kind {
            MouseEventKind::ScrollUp => app.menu.up(),
            MouseEventKind::ScrollDown => app.menu.down(),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = menu::row_at(app.body_area(), m.column, m.row)?;
                let double = app.last_click.is_some_and(|(r, t)| r == row && app.now_ms.saturating_sub(t) <= DOUBLE_CLICK_MS);
                app.menu.selected = row;
                if double {
                    app.last_click = None;
                    return activate(app, ITEMS[row].action);
                }
                app.last_click = Some((row, app.now_ms));
            }
            _ => {}
        },
        Screen::Run(r) => match m.kind {
            MouseEventKind::ScrollUp => r.scroll.up(3, r.log.len()),
            MouseEventKind::ScrollDown => r.scroll.down(3),
            _ => {}
        },
        Screen::Log(st, _) => match m.kind {
            MouseEventKind::ScrollUp => st.scroll.up(3, st.log.len()),
            MouseEventKind::ScrollDown => st.scroll.down(3),
            _ => {}
        },
        Screen::Packages(_) => return packages::mouse(app, m),
        Screen::Auto(_) => return wizard::mouse(app, m),
        Screen::Undo(_) => return undo_screen::mouse(app, m),
        Screen::Summary(_) | Screen::Import(_) | Screen::Export(_) | Screen::Status(_) | Screen::Schedule(_) | Screen::Plasma(_) | Screen::Partition(_) => {}
    }
    None
}

pub fn view(app: &App, f: &mut Frame) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        widgets::too_small(f, area, &app.theme);
        return;
    }
    let [top, body, bottom] = areas(area);
    widgets::header(f, top, &app.header, &app.theme);
    let hints: &[(&str, &str)] = match &app.screen {
        Screen::Menu => {
            menu::view(f, body, app);
            &menu::HINTS
        }
        Screen::Run(r) => {
            run::view(f, body, app, r);
            if r.running() { &run::HINTS_RUNNING } else { &run::HINTS_DONE }
        }
        Screen::Summary(s) => {
            summary::view(f, body, &app.theme, s);
            &summary::HINTS
        }
        Screen::Log(st, _) => {
            log::view(f, body, &app.theme, st, &app.header.log_file);
            &log::HINTS
        }
        Screen::Import(st) => {
            import::view(f, body, app, st);
            import::hints(st)
        }
        Screen::Export(st) => {
            export::view(f, body, app, st);
            export::hints(st)
        }
        Screen::Status(st) => {
            status::view(f, body, app, st);
            &status::HINTS
        }
        Screen::Auto(w) => {
            wizard::view(f, body, app, w);
            wizard::hints(app, w)
        }
        Screen::Undo(st) => {
            undo_screen::view(f, body, app, st);
            undo_screen::hints(st)
        }
        Screen::Packages(st) => {
            packages::view(f, body, app, st);
            packages::hints(app)
        }
        Screen::Schedule(st) => {
            super::screens::schedule::view(f, body, app, st);
            &super::screens::schedule::HINTS
        }
        Screen::Plasma(st) => {
            plasma::view(f, body, app, st);
            plasma::hints(st)
        }
        Screen::Partition(st) => {
            super::screens::partition::view(f, body, app, st);
            super::screens::partition::hints(st)
        }
    };
    let hints: &[(&str, &str)] = if let Some(s) = &app.signin {
        drive::signin_hints(s)
    } else if app.modal.is_some() {
        &[("y/n", "answer"), ("←/→", "choose"), ("Enter", "confirm"), ("Esc", "no")]
    } else {
        hints
    };
    widgets::footer(f, bottom, hints, &app.theme);
    if let Some(m) = &app.modal {
        modal_view(f, body, app, m);
    }
    if let Some(s) = &app.signin {
        drive::signin_view(f, body, app, s);
    }
    if app.help {
        let (title, keys) = app.help_keys();
        help::view(f, area, &app.theme, title, keys);
    }
    widgets::toast(f, body, app);
}

fn modal_view(f: &mut Frame, area: Rect, app: &App, m: &Modal) {
    let theme = &app.theme;
    let mut lines = vec![Line::styled(m.text.clone(), theme.bold())];
    if !m.details.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(m.details.iter().map(|d| Line::raw(format!("  {d}"))));
    }
    let d = Dialog { title: &m.title, lines, yes: m.yes, no: m.no, yes_selected: m.yes_selected };
    widgets::dialog(f, area, theme, &d);
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::pkglist::{PkgList, Reason, Source};
    use crate::report::{Level, Progress, Prompt};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    pub fn header() -> Header {
        Header {
            user: "p3ter".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/p3ter".into(),
            host: "archbox".into(),
            root: true,
            log_file: "/var/log/kast.log".into(),
        }
    }

    pub fn app(w: u16, h: u16) -> App {
        let mut a = App::new(header(), Theme::new(true, false), (w, h));
        // Default folders as on a machine with the data partition, whatever this one has
        a.dir_exists = |p| p == std::path::Path::new("/media/data/backups") || p == std::path::Path::new("/media/data");
        // As on a machine without proton-drive (no check is started)
        a.drive = Some(Box::new(super::super::screens::drive::tests::missing(false)));
        a
    }

    pub fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE })
    }

    pub fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    pub fn render(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
        t.draw(|f| view(app, f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    pub fn progress(o_step: i64, s_step: i64, s_total: i64, item: &str, mark: Option<&str>) -> Progress {
        Progress {
            auto: true,
            o_step,
            o_total: 4,
            weights: Vec::new(),
            o_start: 1000,
            title: "Install Packages".into(),
            s_start: 1100,
            s_step,
            s_total,
            item: item.into(),
            recent_log: String::new(),
            mark: mark.map(str::to_string),
        }
    }

    fn job(app: &mut App, m: JobMsg) -> Option<Command> {
        update(app, Event::Job(m))
    }

    #[test]
    fn menu_navigation_wraps() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Up));
        assert_eq!(a.menu.selected, ITEMS.len() - 1);
        update(&mut a, key(KeyCode::Down));
        assert_eq!(a.menu.selected, 0);
        update(&mut a, key(KeyCode::Char('j')));
        update(&mut a, key(KeyCode::Char('j')));
        update(&mut a, key(KeyCode::Char('k')));
        assert_eq!(a.menu.selected, 1);
        update(&mut a, key(KeyCode::End));
        assert_eq!(a.menu.action(), Action::Quit);
        update(&mut a, key(KeyCode::Home));
        assert_eq!(a.menu.selected, 0);
    }

    #[test]
    fn digits_and_enter_start_actions() {
        let mut a = app(80, 24);
        assert!(matches!(update(&mut a, key(KeyCode::Char('3'))), Some(Command::Start(JobKind::Repo))));
        assert!(matches!(a.screen, Screen::Run(_)));
        assert_eq!(a.menu.selected, 2);
        let mut a = app(80, 24);
        // The automatic run starts with its step list (and a look for snapshot tools), not the job
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DetectSnapshots)));
        assert!(matches!(&a.screen, Screen::Auto(w) if w.stage == wizard::Stage::Steps));
        assert!(!a.quit);
    }

    pub fn online_list() -> PkgList {
        let edited = crate::pkglist::BUILTIN_TEXT
            .replace("  { name = \"steam\" },\n", "")
            .replace("  { name = \"haruna\" },", "  { name = \"haruna\" },\n  { name = \"gimp\" },");
        PkgList { source: Source::Online { fetched: "04:39".into() }, list: crate::pkglist::parse(&edited).unwrap(), discovery_error: None }
    }

    pub fn loaded(a: &mut App, pl: PkgList) -> Option<Command> {
        update(a, Event::Loaded(Loaded::PackageList(Box::new(pl))))
    }

    pub fn installed(a: &mut App, names: &[&str]) {
        update(a, Event::Loaded(Loaded::Installed(names.iter().map(|n| n.to_string()).collect())));
    }

    fn picker(a: &App) -> &super::super::screens::checklist::Checklist {
        match &a.screen {
            Screen::Packages(st) => st.picker.as_ref().unwrap(),
            Screen::Auto(w) => w.apps.picker.as_ref().unwrap(),
            _ => panic!("no picker"),
        }
    }

    /// Put the cursor on the package `name` and press Space.
    fn toggle_pkg(a: &mut App, name: &str) {
        let i = picker(a).rows.iter().position(|r| matches!(r, super::super::screens::checklist::Row::Item(it) if it.key == name)).unwrap();
        match &mut a.screen {
            Screen::Packages(st) => st.picker.as_mut().unwrap().cursor = i,
            Screen::Auto(w) => w.apps.picker.as_mut().unwrap().cursor = i,
            _ => {}
        }
        update(a, key(KeyCode::Char(' ')));
    }

    #[test]
    fn app_picker_selects_and_follows_the_add_on_rule() {
        let mut a = app(80, 24);
        assert!(update(&mut a, key(KeyCode::Char('4'))).is_none());
        assert!(matches!(&a.screen, Screen::Packages(st) if !st.auto));
        assert!(a.animating(), "the spinner turns while fetching");
        // Nothing starts before the list is on screen
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(a.toast.as_ref().is_some_and(|t| t.text.starts_with("Still fetching")));
        assert!(update(&mut a, key(KeyCode::Char('r'))).is_none(), "no retry while fetching");

        // The list arrives: everything checked, the installed check starts off the UI thread
        let Some(Command::LoadInstalled(names)) = loaded(&mut a, online_list()) else { panic!("expected the installed check") };
        assert_eq!(names.len(), 36);
        assert_eq!(a.installed, Installed::Loading);
        assert_eq!(picker(&a).count(), (36, 36));
        let headers: Vec<String> =
            picker(&a).rows.iter().filter_map(|r| if let super::super::screens::checklist::Row::Header(h) = r { Some(h.clone()) } else { None }).collect();
        assert_eq!(headers, ["Official repositories", "kud3n's repository", "Add-ons"]);
        installed(&mut a, &["steam", "obsidian"]);
        assert_eq!(picker(&a).item("obsidian").unwrap().tag.as_deref(), Some("installed"));

        // Unchecking zotero (not installed) greys out its six add-ons
        toggle_pkg(&mut a, "zotero");
        assert_eq!(picker(&a).count(), (29, 36));
        let ocr = picker(&a).item("zotero-ocr").unwrap();
        assert!(!ocr.enabled && ocr.checked && ocr.note.as_deref() == Some("needs zotero"));
        // The same with zotero installed: its add-ons stay pickable
        installed(&mut a, &["zotero"]);
        assert_eq!(picker(&a).count(), (35, 36));
        toggle_pkg(&mut a, "gimp");

        update(&mut a, key(KeyCode::Enter));
        let m = a.modal.as_ref().expect("a confirmation first");
        assert_eq!(m.text, "Install 34 of 36 packages?");
        assert!(m.details.iter().any(|d| d == "Left out: zotero, gimp"));
        let Some(Command::Start(JobKind::Packages(pl, Some(sel)))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a start") };
        assert!(pl.list.contains("gimp"));
        assert_eq!(sel.len(), 34);
        assert!(!sel.contains(&"zotero".to_string()) && sel.contains(&"zotero-ocr".to_string()));
        assert_eq!(sel.first().map(String::as_str), Some("zen-browser-bin"), "list order");
        let Screen::Run(r) = &a.screen else { panic!("run screen") };
        assert!(r.packages.as_ref().is_some_and(|l| l.to_string().contains("Online · kud3n013/kast@main · fetched 04:39 · 36 packages")));

        // All checked: no selection, the same as the CLI
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('4')));
        loaded(&mut a, online_list());
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(JobKind::Packages(_, None)))));
        // None checked: no confirmation
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('4')));
        loaded(&mut a, online_list());
        update(&mut a, key(KeyCode::Char('n')));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(a.modal.is_none() && a.toast.as_ref().unwrap().kind == ToastKind::Error);
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    /// The online list plus two packages found in archrepo's database: a standalone one and an
    /// add-on of steam.
    fn discovered_list() -> PkgList {
        let mut pl = online_list();
        let found = |name: &str, depends: &[&str]| crate::pkglist::discover::Found {
            name: name.into(),
            description: Some(format!("{name} from the database")),
            depends: depends.iter().map(|d| d.to_string()).collect(),
        };
        crate::pkglist::add_discovered(&mut pl, Ok(vec![found("kdenlive-extras", &["kdenlive"]), found("fresh-app", &["glibc"])]));
        assert_eq!(pl.list.discovered(), 2);
        pl
    }

    #[test]
    fn discovered_packages_are_offered_in_their_group_with_the_add_on_rule() {
        let mut a = app(80, 24);
        a.repo_configured = true;
        update(&mut a, key(KeyCode::Char('4')));
        let Some(Command::LoadInstalled(names)) = loaded(&mut a, discovered_list()) else { panic!("expected the installed check") };
        assert_eq!(names.len(), 38);
        assert!(names.contains(&"archrepo/fresh-app".to_string()) && names.contains(&"archrepo/kdenlive-extras".to_string()));
        assert_eq!(picker(&a).count(), (38, 38));
        installed(&mut a, &["fcitx5"]);
        assert_eq!(picker(&a).item("archrepo/fresh-app").unwrap().label, "fresh-app");

        // Unchecking kdenlive (not installed) greys out its discovered add-on, like any other
        toggle_pkg(&mut a, "kdenlive");
        let extras = picker(&a).item("archrepo/kdenlive-extras").unwrap();
        assert!(!extras.enabled && extras.note.as_deref() == Some("needs kdenlive"));
        assert_eq!(picker(&a).count(), (36, 38));
        toggle_pkg(&mut a, "archrepo/fresh-app");

        // The job gets what was checked, and nothing it wasn't shown
        update(&mut a, key(KeyCode::Enter));
        let Some(Command::Start(JobKind::Packages(pl, Some(sel)))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a start") };
        assert_eq!(pl.list.discovered(), 2);
        assert_eq!(sel.len(), 35);
        assert!(!sel.contains(&"archrepo/fresh-app".to_string()) && !sel.contains(&"archrepo/kdenlive-extras".to_string()));

        // All checked: no selection, which means exactly the packages that were shown
        let mut a = app(80, 24);
        a.repo_configured = true;
        update(&mut a, key(KeyCode::Char('4')));
        loaded(&mut a, discovered_list());
        update(&mut a, key(KeyCode::Enter));
        let Some(Command::Start(JobKind::Packages(pl, None))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a start with everything") };
        assert_eq!(pl.list.count(), 38);
    }

    #[test]
    fn discovered_packages_are_left_out_while_the_repository_is_not_set_up() {
        // Menu option 4 with no [archrepo] in pacman.conf
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('4')));
        let Some(Command::LoadInstalled(names)) = loaded(&mut a, discovered_list()) else { panic!("expected the installed check") };
        assert_eq!(names.len(), 36);
        assert!(picker(&a).item("archrepo/fresh-app").is_none());
        update(&mut a, key(KeyCode::Enter));
        let Some(Command::Start(JobKind::Packages(pl, None))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a start") };
        assert_eq!(pl.list.count(), 36, "the job's list has none of them either, so None can't install them");
        assert_eq!(pl.list.discovered(), 0);
    }

    #[test]
    fn the_automatic_run_offers_them_only_with_the_repository_step() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('1')));
        loaded(&mut a, discovered_list());
        let Screen::Auto(w) = &mut a.screen else { panic!("wizard") };
        assert!(w.chosen().contains(&crate::setup::Step::Repo));
        // Repo unchecked, [archrepo] not configured: they could not be installed
        w.steps.item_mut("repo").unwrap().checked = false;
        w.stage = super::super::screens::wizard::Stage::Apps;
        packages::sync(&mut a);
        assert!(picker(&a).item("archrepo/fresh-app").is_none());
        assert_eq!(picker(&a).count(), (36, 36));
        toggle_pkg(&mut a, "zotero");
        // Checking the step brings them back; the other choices stay
        let Screen::Auto(w) = &mut a.screen else { panic!("wizard") };
        w.steps.item_mut("repo").unwrap().checked = true;
        let cmd = packages::sync(&mut a);
        assert!(matches!(cmd, Some(Command::LoadInstalled(n)) if n.len() == 38), "asks about the names now shown");
        assert!(picker(&a).item("archrepo/fresh-app").is_some_and(|i| i.checked));
        assert!(!picker(&a).item("zotero").unwrap().checked);
        let plan = match &a.screen {
            Screen::Auto(w) => w.plan(),
            _ => unreachable!(),
        };
        assert_eq!(plan.packages.as_ref().map(Vec::len), Some(31));
    }

    #[test]
    fn a_retry_with_another_list_keeps_the_checks() {
        let mut a = app(80, 24);
        a.repo_configured = true;
        update(&mut a, key(KeyCode::Char('4')));
        loaded(&mut a, PkgList::builtin(Reason::Network("timeout".into())));
        toggle_pkg(&mut a, "zotero");
        toggle_pkg(&mut a, "wine");
        assert_eq!(picker(&a).count(), (28, 36));
        update(&mut a, key(KeyCode::Char('r')));
        loaded(&mut a, discovered_list());
        assert!(!picker(&a).item("zotero").unwrap().checked && !picker(&a).item("wine").unwrap().checked);
        assert!(picker(&a).item("archrepo/fresh-app").unwrap().checked, "new names start checked");
        assert!(!picker(&a).item("gimp").is_some_and(|i| !i.checked));
    }

    #[test]
    fn snapshot_app_picker_with_discovered_packages() {
        let mut a = picker_app(140, 40, Some(discovered_list()));
        a.repo_configured = true;
        packages::sync(&mut a);
        toggle_pkg(&mut a, "archrepo/kdenlive-extras");
        insta::assert_snapshot!("picker_discovered_140x40", render(&a));
        let mut a = picker_app(80, 24, Some(discovered_list()));
        a.repo_configured = true;
        packages::sync(&mut a);
        update(&mut a, key(KeyCode::End));
        insta::assert_snapshot!("picker_discovered_80x24", render(&a));
        // Left out until the repository exists: the details say so
        let a = picker_app(140, 40, Some(discovered_list()));
        insta::assert_snapshot!("picker_discovered_hidden_140x40", render(&a));
    }

    #[test]
    fn retry_only_after_a_failed_download() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('4')));
        loaded(&mut a, PkgList::builtin(Reason::Offline));
        assert!(update(&mut a, key(KeyCode::Char('r'))).is_none(), "--offline was asked for");
        loaded(&mut a, PkgList::builtin(Reason::Network("timeout".into())));
        assert!(matches!(update(&mut a, key(KeyCode::Char('r'))), Some(Command::FetchPackages)));
        assert!(matches!(a.packages, PkgState::Fetching));
        loaded(&mut a, online_list());
        assert!(update(&mut a, key(KeyCode::Char('r'))).is_none());
    }

    fn picker_app(w: u16, h: u16, list: Option<PkgList>) -> App {
        let mut a = app(w, h);
        update(&mut a, key(KeyCode::Char('4')));
        if let Some(pl) = list {
            loaded(&mut a, pl);
            installed(&mut a, &["zotero", "steam", "fcitx5", "beeper"]);
        }
        a
    }

    #[test]
    fn snapshot_app_picker_online() {
        let mut a = picker_app(80, 24, Some(online_list()));
        toggle_pkg(&mut a, "archrepo/freedownloadmanager");
        toggle_pkg(&mut a, "zotero");
        insta::assert_snapshot!("picker_online_80x24", render(&a));
        let mut a = picker_app(140, 40, Some(online_list()));
        toggle_pkg(&mut a, "archrepo/freedownloadmanager");
        update(&mut a, key(KeyCode::End));
        insta::assert_snapshot!("picker_online_140x40", render(&a));
    }

    #[test]
    fn snapshot_app_picker_builtin_with_reason() {
        let pl = || Some(PkgList::builtin(Reason::Network("timeout".into())));
        insta::assert_snapshot!("picker_builtin_80x24", render(&picker_app(80, 24, pl())));
        insta::assert_snapshot!("picker_builtin_140x40", render(&picker_app(140, 40, pl())));
    }

    #[test]
    fn snapshot_app_picker_fetching() {
        insta::assert_snapshot!("picker_fetching_80x24", render(&picker_app(80, 24, None)));
        insta::assert_snapshot!("picker_fetching_140x40", render(&picker_app(140, 40, None)));
    }

    #[test]
    fn quit_keys() {
        for ev in [key(KeyCode::Char('q')), key(KeyCode::Esc), ctrl('c')] {
            let mut a = app(80, 24);
            assert!(update(&mut a, ev).is_none());
            assert!(a.quit);
        }
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Enter));
        assert!(a.quit);
    }

    #[test]
    fn help_opens_and_any_key_closes() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('?')));
        assert!(a.help);
        assert!(update(&mut a, key(KeyCode::Char('q'))).is_none());
        assert!(!a.help && !a.quit);
    }

    #[test]
    fn double_click_runs() {
        let mut a = app(120, 30);
        let click = |row| Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 5, row, modifiers: KeyModifiers::NONE });
        // Header at y=0, list border at y=1, first item at y=2
        assert!(update(&mut a, click(4)).is_none());
        assert_eq!(a.menu.selected, 2);
        a.now_ms += 100;
        assert!(matches!(update(&mut a, click(4)), Some(Command::Start(JobKind::Repo))));
        let mut a = app(120, 30);
        update(&mut a, click(4));
        a.now_ms += 5000;
        assert!(update(&mut a, click(4)).is_none());
    }

    fn running_auto(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        a.now = 1000;
        update(&mut a, key(KeyCode::Char('1')));
        loaded(&mut a, online_list());
        update(&mut a, Event::Loaded(Loaded::Snapshots(crate::snapshot::test_cases()[0].1.clone())));
        // Steps → upgrade → apps → review → start
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(JobKind::Auto(..)))));
        a.now = 1300;
        job(&mut a, JobMsg::Progress(progress(3, 12, 41, "Installing obsidian (pacman)", None)));
        job(&mut a, JobMsg::Log { level: Level::Info, line: "Package zotero already installed.".into() });
        job(&mut a, JobMsg::Log { level: Level::Error, line: "Failed to install steam. Recorded in /var/log/kast.log".into() });
        job(&mut a, JobMsg::Log { level: Level::Raw, line: "resolving dependencies...".into() });
        job(&mut a, JobMsg::Transient("obsidian-1.9.12   45%".into()));
        a
    }

    #[test]
    fn escape_asks_before_canceling() {
        let mut a = running_auto(80, 24);
        assert!(update(&mut a, key(KeyCode::Esc)).is_none());
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::CancelJob));
        // Enter takes the safe default: keep running
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(a.modal.is_none() && a.job_running());
        update(&mut a, ctrl('c'));
        update(&mut a, key(KeyCode::Left));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Cancel)));
        assert!(a.run_state().unwrap().cancel_requested);
        // A second request doesn't ask again
        assert!(update(&mut a, key(KeyCode::Esc)).is_none());
        assert!(a.modal.is_none());
        job(&mut a, JobMsg::Done(Ok(Outcome::Canceled("Canceled".into()))));
        assert!(!a.job_running());
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn stale_pacman_lock_offers_removal() {
        let canceled = || {
            let mut a = running_auto(80, 24);
            update(&mut a, ctrl('c'));
            update(&mut a, key(KeyCode::Left));
            update(&mut a, key(KeyCode::Enter));
            job(&mut a, JobMsg::Done(Ok(Outcome::CanceledStaleLock("Canceled; pacman left its lock behind".into()))));
            a
        };
        let mut a = canceled();
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::Confirm(ref k) if matches!(**k, JobKind::RemovePacmanLock)));
        insta::assert_snapshot!(render(&a));
        // Esc keeps the lock
        assert!(update(&mut a, key(KeyCode::Esc)).is_none());
        assert!(a.modal.is_none() && !a.job_running());
        // Enter takes the safe default too; y removes it
        let mut a = canceled();
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        let mut a = canceled();
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::Start(JobKind::RemovePacmanLock))));
    }

    #[test]
    fn engine_questions_default_and_escape() {
        let mut a = running_auto(80, 24);
        let ask = |id, default| JobMsg::Ask { id, prompt: Prompt::yes_no("Close/kill running application(s) (zen)?", default) };
        job(&mut a, ask(7, true));
        assert!(a.modal.as_ref().unwrap().yes_selected);
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Answer { id: 7, yes: true })));
        job(&mut a, ask(8, true));
        // Esc is always the safe answer, whatever the default
        assert!(matches!(update(&mut a, key(KeyCode::Esc)), Some(Command::Answer { id: 8, yes: false })));
        job(&mut a, ask(9, false));
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::Answer { id: 9, yes: true })));
        // Ctrl+C: No to the question, then the cancel dialog
        job(&mut a, ask(10, true));
        assert!(matches!(update(&mut a, ctrl('c')), Some(Command::Answer { id: 10, yes: false })));
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::CancelJob));
    }

    #[test]
    fn questions_after_cancel_are_declined() {
        let mut a = running_auto(80, 24);
        a.run_state().unwrap().cancel_requested = true;
        assert!(matches!(job(&mut a, JobMsg::Ask { id: 3, prompt: Prompt::yes_no("q", true) }), Some(Command::Answer { id: 3, yes: false })));
    }

    #[test]
    fn failed_job_returns_to_menu_with_toast() {
        let mut a = running_auto(80, 24);
        job(&mut a, JobMsg::Done(Err("Action failed".into())));
        assert!(matches!(a.screen, Screen::Menu));
        let t = a.toast.as_ref().unwrap();
        assert_eq!(t.kind, ToastKind::Error);
        assert!(t.text.contains("/var/log/kast.log"));
    }

    #[test]
    fn snapshot_menu_80x24() {
        insta::assert_snapshot!(render(&app(80, 24)));
    }

    #[test]
    fn snapshot_menu_140x40() {
        let mut a = app(140, 40);
        update(&mut a, key(KeyCode::Char('j')));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_too_small() {
        let mut a = app(50, 15);
        insta::assert_snapshot!(render(&a));
        update(&mut a, key(KeyCode::Char('q')));
        assert!(a.quit);
    }

    #[test]
    fn snapshot_help_ascii() {
        let mut a = App::new(header(), Theme::new(false, true), (80, 24));
        update(&mut a, key(KeyCode::Char('?')));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_run_auto_80x24() {
        insta::assert_snapshot!(render(&running_auto(80, 24)));
    }

    #[test]
    fn snapshot_run_auto_140x40() {
        insta::assert_snapshot!(render(&running_auto(140, 40)));
    }

    #[test]
    fn snapshot_engine_question() {
        let mut a = running_auto(80, 24);
        let p = Prompt::yes_no("Close/kill running application(s) (zen obsidian) to proceed with restore?", true)
            .with_details(vec!["zen (PID: 4242 4243)".into(), "obsidian (PID: 5150)".into()]);
        job(&mut a, JobMsg::Ask { id: 1, prompt: p });
        insta::assert_snapshot!(render(&a));
    }

    fn summary(issues: Vec<String>, howdy: bool) -> Summary {
        Summary {
            elapsed: "12:34".into(),
            user: "p3ter".into(),
            uid: 1000,
            gid: 1000,
            partition: None,
            repo_configured: true,
            log_file: "/var/log/kast.log".into(),
            packages: "Online · kud3n013/kast@main · fetched 04:39 · 36 packages".into(),
            skipped: Vec::new(),
            import: None,
            export: None,
            rollback: None,
            issues,
            howdy_enrolled: howdy,
        }
    }

    #[test]
    fn auto_run_ends_on_summary_and_log_returns_there() {
        let mut a = running_auto(80, 24);
        job(&mut a, JobMsg::Done(Ok(Outcome::Summary(Box::new(summary(vec![], true))))));
        assert!(matches!(a.screen, Screen::Summary(_)));
        assert!(matches!(update(&mut a, key(KeyCode::Char('l'))), Some(Command::OpenLog)));
        update(&mut a, Event::Loaded(Loaded::Log(vec!["[x] [INFO] hello".into()])));
        assert!(matches!(a.screen, Screen::Log(..)));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Summary(_)));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(a.screen, Screen::Menu));
    }

    fn shift(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT))
    }

    /// The command each log key gives, as a short name.
    fn log_keys(a: &mut App) -> Vec<(char, String)> {
        let mut out = Vec::new();
        for c in ['e', 'E', 'o', 'f', 'y', 'Y'] {
            let ev = if c.is_uppercase() { shift(c) } else { key(KeyCode::Char(c)) };
            let screen_before = std::mem::discriminant(&a.screen);
            let got = match update(a, ev) {
                Some(Command::ViewLog { editor }) => if editor { "editor" } else { "pager" }.to_string(),
                Some(Command::OpenLogFile) => "open".into(),
                Some(Command::ShowLogFile) => "show".into(),
                Some(Command::CopyLog(LogScope::Session)) => "copy session".into(),
                Some(Command::CopyLog(LogScope::Job)) => "copy job".into(),
                Some(Command::CopyLogPath) => "copy path".into(),
                Some(other) => format!("{other:?}"),
                None => "-".into(),
            };
            assert_eq!(screen_before, std::mem::discriminant(&a.screen), "{c} changed the screen");
            out.push((c, got));
        }
        out
    }

    fn expect(scope: &str) -> Vec<(char, String)> {
        [('e', "pager"), ('E', "editor"), ('o', "open"), ('f', "show"), ('y', scope), ('Y', "copy path")].into_iter().map(|(c, s)| (c, s.to_string())).collect()
    }

    #[test]
    fn log_keys_on_each_screen() {
        let mut a = app(80, 24);
        assert_eq!(log_keys(&mut a), expect("copy session"));
        assert!(update(&mut a, ctrl('e')).is_none());

        let mut a = running_auto(80, 24);
        let running: Vec<(char, String)> = expect("copy job").into_iter().map(|(c, s)| (c, if "yY".contains(c) { s } else { "-".into() })).collect();
        assert_eq!(log_keys(&mut a), running);

        job(&mut a, JobMsg::Done(Ok(Outcome::Finished { message: "Done".into(), issues: vec![] })));
        assert_eq!(log_keys(&mut a), expect("copy job"));

        a.screen = Screen::Summary(Box::new(summary(vec![], true)));
        assert_eq!(log_keys(&mut a), expect("copy job"));

        update(&mut a, Event::Loaded(Loaded::Log(vec!["[x] [INFO] hello".into()])));
        assert!(matches!(a.screen, Screen::Log(..)));
        assert_eq!(log_keys(&mut a), expect("copy session"));
    }

    #[test]
    fn log_keys_leave_dialogs_and_text_fields_alone() {
        // A question takes y as its answer
        let mut a = running_auto(80, 24);
        job(&mut a, JobMsg::Ask { id: 4, prompt: Prompt::yes_no("q", false) });
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::Answer { id: 4, yes: true })));
        // A typed folder takes the letters, ? and q included
        let mut a = app(80, 24);
        let mut st = ImportState::new(import::Mode::Restore, false, "/tmp/".into());
        st.stage = import::Stage::Folder;
        a.screen = Screen::Import(Box::new(st));
        for c in "eofyY?q".chars() {
            assert!(update(&mut a, key(KeyCode::Char(c))).is_none());
        }
        assert!(!a.help && !a.quit);
        let Screen::Import(st) = &a.screen else { panic!() };
        assert_eq!(st.folder.value, "/tmp/eofyY?q");
    }

    #[test]
    fn log_action_results_become_toasts_or_osc52() {
        let mut a = app(80, 24);
        update(&mut a, Event::Loaded(Loaded::LogIo(crate::logio::Report::err("No pager found"))));
        assert_eq!(a.toast.as_ref().map(|t| (t.kind, t.text.as_str())), Some((ToastKind::Error, "No pager found")));
        let report = crate::logio::Report::ok("Copied 3 lines via terminal (OSC 52)");
        let cmd = update(&mut a, Event::Loaded(Loaded::Osc52 { bytes: b"\x1b]52;c;eA==\x07".to_vec(), report }));
        assert!(matches!(cmd, Some(Command::Osc52(ref b)) if b.starts_with(b"\x1b]52")));
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Info);
    }

    #[test]
    fn snapshot_log_viewer_with_path_and_wrapped_toast() {
        let mut a = app(80, 24);
        update(&mut a, Event::Loaded(Loaded::Log(vec!["[2026-10-17 10:00:00] [INFO] Adding archrepo".into(), "resolving dependencies...".into()])));
        let report = crate::logio::Report::ok(format!("Copied 412 lines via terminal (OSC 52); {}", crate::logio::OSC52_NOTE));
        update(&mut a, Event::Loaded(Loaded::Osc52 { bytes: vec![], report }));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_footers_at_minimum_width() {
        let mut a = app(60, 20);
        let menu = render(&a);
        update(&mut a, Event::Loaded(Loaded::Log(vec!["[2026-10-17 10:00:00] [INFO] Adding archrepo".into()])));
        insta::assert_snapshot!(format!("{menu}{}", render(&a)));
    }

    #[test]
    fn help_fits_at_minimum_size() {
        let mut a = app(60, 20);
        update(&mut a, key(KeyCode::Char('?')));
        let menu = render(&a);
        let mut a = running_auto(60, 20);
        update(&mut a, key(KeyCode::Char('?')));
        let run = render(&a);
        insta::assert_snapshot!(format!("{menu}{run}"));
    }

    #[test]
    fn snapshot_log_viewer_ascii() {
        let mut a = App::new(header(), Theme::new(false, true), (80, 24));
        update(&mut a, Event::Loaded(Loaded::Log(vec!["[2026-10-17 10:00:00] [INFO] Adding archrepo".into()])));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_summary_with_issues_80x24() {
        let mut a = app(80, 24);
        let issues = vec!["System upgrade (pacman -Syuu) failed with exit code 1".into(), "Package failed to install: steam".into()];
        a.screen = Screen::Summary(Box::new(summary(issues, false)));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_summary_with_issues_140x40() {
        let mut a = app(140, 40);
        let issues = vec!["System upgrade (pacman -Syuu) failed with exit code 1".into(), "Could not enable/start searxng".into()];
        a.screen = Screen::Summary(Box::new(summary(issues, false)));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_summary_ok_140x40() {
        let mut a = app(140, 40);
        let mut s = summary(vec![], true);
        s.partition = Some("/media/data mounted & user folders symlinked".into());
        a.screen = Screen::Summary(Box::new(s));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_run_finished_with_issues() {
        let mut a = running_auto(80, 24);
        job(&mut a, JobMsg::Progress(progress(4, 4, 4, "Completed", Some("✔"))));
        let issues = vec!["Package failed to install: steam".to_string(), "Could not enable/start searxng".to_string()];
        job(&mut a, JobMsg::Done(Ok(Outcome::Finished { message: "Services step finished".into(), issues })));
        a.toast = None;
        insta::assert_snapshot!(render(&a));
    }
}
