//! Everything the UI thread reacts to arrives as one `Event` on one channel.

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::report::{Level, Progress, Prompt};
use std::path::PathBuf;

use super::screens::export::TargetInfo;
use super::screens::import::{Candidate, Inspected};
use crate::setup::Summary;

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    /// Redraw tick (only while something animates)
    Tick,
    Job(JobMsg),
    /// Results of a load the runtime did for a screen
    Loaded(Loaded),
}

#[derive(Debug)]
pub enum Loaded {
    /// The last lines of the log file
    Log(Vec<String>),
    Candidates(Vec<Candidate>),
    /// A file the user highlighted (`pick` false) or chose
    Inspected {
        path: PathBuf,
        pick: bool,
        result: Result<Option<Inspected>, String>,
    },
    Completion {
        value: String,
        matches: Vec<String>,
    },
    ExportTargets {
        targets: Vec<TargetInfo>,
        ts: String,
    },
    SnapshotSize {
        file: PathBuf,
        bytes: u64,
    },
    Snapshots(crate::snapshot::Detected),
    Undo(Box<crate::tui::screens::undo::UndoData>),
    Uninstall(Box<crate::tui::screens::undo::UninstallData>),
    /// Bytes freed by a prune
    Pruned(u64),
    /// The installed packages among those asked about
    Installed(std::collections::HashSet<String>),
    DirCreated {
        path: PathBuf,
        result: Result<(), String>,
    },
    Status(Box<crate::vault::status::StatusReport>),
    /// The package list for this session (downloaded, or the built-in one and why)
    PackageList(Box<crate::pkglist::PkgList>),
    Drive(Box<crate::tui::screens::drive::DriveInfo>),
    /// The sign-in address (prints as hidden)
    SignInUrl(crate::proton::SignInUrl),
    /// The sign-in process ended (the error scrubbed)
    SignInDone(Result<(), String>),
    DriveLoggedOut(Result<(), String>),
    DriveStore(Result<(), String>),
    DriveHosts(Result<Vec<String>, String>),
    Schedule(Box<crate::tui::screens::schedule::ScheduleInfo>),
    /// The answer to a Plasma settings request
    Plasma(Box<crate::tui::screens::plasma::PlasmaResp>),
    /// A schedule change or run finished (the message for a toast)
    ScheduleDone(Result<String, String>),
    DriveBackups {
        host: String,
        result: Result<Vec<crate::drive::RemoteBackup>, String>,
    },
    DriveDownloaded {
        remote: String,
        result: Result<PathBuf, String>,
    },
    /// How a log action went (shown as a toast)
    LogIo(crate::logio::Report),
    /// No clipboard program worked: copy through the terminal with these bytes
    Osc52 {
        bytes: Vec<u8>,
        report: crate::logio::Report,
    },
}

/// Messages from a job thread (the engines, through `TuiReporter`).
#[derive(Debug)]
pub enum JobMsg {
    Progress(Progress),
    Log {
        level: Level,
        line: String,
    },
    /// A `\r`-rewritten line still in progress (replaced by the next one)
    Transient(String),
    /// An engine question; the job waits until `Command::Answer` with the same id
    Ask {
        id: u64,
        prompt: Prompt,
    },
    Done(Result<Outcome, String>),
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The automatic run: show its summary screen
    Summary(Box<Summary>),
    /// Any other action: a closing message plus the issues it recorded
    Finished { message: String, issues: Vec<String> },
    /// Stopped by the user (a confirmed cancel or a declined engine prompt)
    Canceled(String),
    /// Canceled while pacman ran, and pacman left its lock behind: offer to remove it
    CanceledStaleLock(String),
}
