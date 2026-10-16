//! The seam between the engines (setup steps, vault) and whoever shows their progress.
//!
//! Engines never print, prompt or draw directly; they call the free functions here, which
//! go to the reporter installed for the current thread. The main thread uses
//! [`CliReporter`] (the plain terminal behaviour); a TUI job thread installs its own.
//! A thread-local keeps every engine signature unchanged, the same way `ctx()` does.

use std::cell::RefCell;
use std::sync::Arc;

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
    /// True when a full-screen UI owns the terminal: children must not inherit it.
    fn owns_terminal(&self) -> bool {
        false
    }
}

/// Plain terminal output, exactly as kas always printed it.
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
#[allow(dead_code)] // used by the TUI job thread (next commit)
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
