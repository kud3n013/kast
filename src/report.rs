//! The seam between the engines (setup steps, vault) and whoever shows their progress.
//!
//! Engines never print, prompt or draw directly; they call the free functions here, which
//! go to the reporter installed for the current thread. The main thread uses
//! [`CliReporter`] (the plain terminal behaviour); a TUI job thread installs its own.
//! A thread-local keeps every engine signature unchanged, the same way `ctx()` does.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::ui::{self, C_BLUE, C_BOLD, C_CYAN, C_GREEN, C_RED, C_RESET, C_YELLOW};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
    /// `==> message` headline of a vault step
    Step,
    /// Unprefixed line on stderr
    Raw,
    /// Empty line on stdout (spacing in CLI output)
    Blank,
}

/// A yes/no question from an engine. `text` is the question without the `[Y/n]: ` suffix;
/// `details` are extra lines a dialog can show (e.g. running apps and their PIDs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub default: bool,
    pub details: Vec<String>,
}

impl Prompt {
    pub fn yes_no(text: impl Into<String>, default: bool) -> Self {
        Prompt { text: text.into(), default, details: Vec::new() }
    }

    pub fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }
}

/// One dashboard update: where the overall run and the current step are.
#[derive(Clone, Debug)]
pub struct Progress {
    pub auto: bool,
    pub o_step: i64,
    pub o_total: i64,
    /// Weights of the run's steps (automatic run), for the overall percentage
    pub weights: Vec<i64>,
    pub o_start: i64,
    pub title: String,
    pub s_start: i64,
    pub s_step: i64,
    pub s_total: i64,
    pub item: String,
    pub recent_log: String,
    /// Fixed mark instead of the spinner (✔ / ✘ when an item finished)
    pub mark: Option<String>,
}

pub trait Reporter: Send + Sync {
    fn log(&self, level: Level, msg: &str);
    fn progress(&self, p: &Progress);
    fn ask(&self, p: &Prompt) -> bool;
    /// A line that was just appended to the log file (`ctx::log`).
    fn file_logged(&self, _level: &str, _msg: &str) {}
    /// A complete line of a monitored child's output.
    fn output_line(&self, _line: &str) {}
    /// A `\r`-rewritten line of a monitored child that is still in progress.
    fn output_partial(&self, _line: &str) {}
    /// True when a full-screen UI owns the terminal: children must not inherit it.
    fn owns_terminal(&self) -> bool {
        false
    }
}

/// Plain terminal output, exactly as kast always printed it.
pub struct CliReporter;

impl Reporter for CliReporter {
    fn log(&self, level: Level, msg: &str) {
        match level {
            Level::Info => println!("{C_BLUE}[INFO]{C_RESET} {msg}"),
            Level::Success => println!("{C_GREEN}[SUCCESS]{C_RESET} {msg}"),
            Level::Warn => eprintln!("{C_YELLOW}[WARN]{C_RESET} {msg}"),
            Level::Error => eprintln!("{C_RED}[ERROR]{C_RESET} {msg}"),
            Level::Step => println!("{C_CYAN}==>{C_RESET} {C_BOLD}{msg}{C_RESET}"),
            Level::Raw => eprintln!("{msg}"),
            Level::Blank => println!(),
        }
    }

    fn progress(&self, p: &Progress) {
        ui::draw_dashboard(p);
    }

    /// `[Y/n]` accepts an empty answer or "y"; `[y/N]` only "y".
    fn ask(&self, p: &Prompt) -> bool {
        let suffix = if p.default { "[Y/n]" } else { "[y/N]" };
        let answer = ui::read_choice(&format!("{} {suffix}: ", p.text));
        answer.eq_ignore_ascii_case("y") || (p.default && answer.is_empty())
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<dyn Reporter>>> = const { RefCell::new(None) };
}

/// Install `r` for the current thread (a TUI job thread does this first thing).
pub fn install(r: Arc<dyn Reporter>) {
    CURRENT.with(|c| *c.borrow_mut() = Some(r));
}

/// The reporter of this thread; `CliReporter` unless one was installed.
pub fn current() -> Arc<dyn Reporter> {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(|| Arc::new(CliReporter))
}

pub fn log(level: Level, msg: &str) {
    current().log(level, msg);
}

pub fn blank() {
    current().log(Level::Blank, "");
}

pub fn progress(p: &Progress) {
    current().progress(p);
}

pub fn ask(p: &Prompt) -> bool {
    current().ask(p)
}

pub fn owns_terminal() -> bool {
    current().owns_terminal()
}

// --- Cancellation (TUI jobs) ---------------------------------------------------------------

/// Unwind payload that ends a canceled job at its next checkpoint. Raised with
/// `resume_unwind`, so no panic message is printed; the job thread catches it.
#[derive(Debug)]
pub struct JobCanceled;

static CANCEL: AtomicBool = AtomicBool::new(false);
/// Process group of the monitored child that is running right now (TUI only).
static CHILD_PGID: Mutex<Option<i32>> = Mutex::new(None);

pub fn reset_cancel() {
    CANCEL.store(false, Ordering::SeqCst);
}

pub fn cancel_requested() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/// Stop the current job: the engines see `ctx::canceled()`, the running child gets SIGTERM
/// (SIGKILL after 3 s if it is still there) and the job ends at its next checkpoint.
pub fn request_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
    crate::ctx::set_canceled(true);
    kill_child(Duration::from_secs(3));
}

/// SIGTERM the running child's process group; SIGKILL it after `grace` if it is still the
/// running child. sudo relays SIGTERM to the command it runs.
pub fn kill_child(grace: Duration) {
    let Some(pgid) = *CHILD_PGID.lock().unwrap() else { return };
    let group = Pid::from_raw(-pgid);
    let _ = kill(group, Signal::SIGTERM);
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        if *CHILD_PGID.lock().unwrap() == Some(pgid) {
            let _ = kill(group, Signal::SIGKILL);
        }
    });
}

pub fn set_child(pgid: Option<i32>) {
    *CHILD_PGID.lock().unwrap() = pgid;
}

/// `cmd.status()`, except on a TUI job thread: there the child gets its own process group
/// and is registered, so a cancel can stop it, and a canceled job ends right after it.
pub fn run_tracked(cmd: &mut std::process::Command) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    if !owns_terminal() {
        return cmd.status();
    }
    checkpoint();
    let mut child = cmd.process_group(0).spawn()?;
    set_child(Some(child.id() as i32));
    let status = child.wait();
    set_child(None);
    // Before the caller looks at the exit status: a killed child is a cancel, not a failure
    checkpoint();
    status
}

/// End the job here if a cancel was confirmed (only ever true on a TUI job thread).
pub fn checkpoint() {
    if cancel_requested() && owns_terminal() {
        std::panic::resume_unwind(Box::new(JobCanceled));
    }
}

/// Splits a child's output into lines. A `\r` without `\n` (progress bars) yields the text
/// so far as a partial line that the next piece replaces.
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    pending_cr: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Line(String),
    Partial(String),
}

impl LineSplitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if b != b'\n' {
                    let text = clean(&self.buf);
                    self.buf.clear();
                    if !text.trim().is_empty() {
                        out.push(Piece::Partial(text));
                    }
                }
            }
            match b {
                b'\n' => {
                    out.push(Piece::Line(clean(&self.buf)));
                    self.buf.clear();
                }
                b'\r' => self.pending_cr = true,
                _ => self.buf.push(b),
            }
        }
        out
    }

    /// Whatever is left when the output ends.
    pub fn finish(&mut self) -> Option<String> {
        let text = clean(&self.buf);
        self.buf.clear();
        (!text.trim().is_empty()).then_some(text)
    }
}

fn clean(bytes: &[u8]) -> String {
    ui::sanitize_log(&String::from_utf8_lossy(bytes))
}

pub fn file_logged(level: &str, msg: &str) {
    if let Some(r) = CURRENT.with(|c| c.borrow().clone()) {
        r.file_logged(level, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        logs: Mutex<Vec<(Level, String)>>,
    }

    impl Reporter for Recorder {
        fn log(&self, level: Level, msg: &str) {
            self.logs.lock().unwrap().push((level, msg.to_string()));
        }
        fn progress(&self, _p: &Progress) {}
        fn ask(&self, p: &Prompt) -> bool {
            p.default
        }
    }

    #[test]
    fn splitter_handles_cr_progress_and_crlf() {
        let mut sp = LineSplitter::default();
        assert_eq!(sp.feed(b"one\ntw"), vec![Piece::Line("one".into())]);
        assert_eq!(sp.feed(b"o\r\n"), vec![Piece::Line("two".into())]);
        // pacman-style progress: each \r rewrites the line
        assert_eq!(sp.feed(b" 10%\r 50%\r100%\n"), vec![Piece::Partial(" 10%".into()), Piece::Partial(" 50%".into()), Piece::Line("100%".into())]);
        assert_eq!(sp.feed(b"\x1b[1mbold\x1b[0m\ttab"), vec![]);
        assert_eq!(sp.finish(), Some("bold  tab".into()));
        assert_eq!(sp.finish(), None);
    }

    #[test]
    fn installed_reporter_is_per_thread() {
        let rec = Arc::new(Recorder::default());
        let r2 = rec.clone();
        std::thread::spawn(move || {
            install(r2);
            log(Level::Warn, "hello");
            assert!(ask(&Prompt::yes_no("q", true)));
            assert!(!owns_terminal());
        })
        .join()
        .unwrap();
        assert_eq!(*rec.logs.lock().unwrap(), vec![(Level::Warn, "hello".to_string())]);
        // This thread still has the CLI default
        assert!(!owns_terminal());
    }
}
