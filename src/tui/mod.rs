//! Full-screen interface (Ratatui + crossterm). `run` owns the terminal from launch to quit:
//! one event loop on the UI thread, input read on a helper thread, slow work on job threads.

// Pieces used by screens that are still being ported
#![allow(dead_code)]

mod app;
mod event;
mod job;
mod screens;
mod theme;
mod widgets;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self as ct, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};

use crate::ctx::{self, Reported, ctx};
use crate::report;
use crate::ui::C_RED;
use crate::ui::C_RESET;
use app::{App, Command, Header, ToastKind, update, view};
use event::Event;
use screens::menu::Action;

type Term = Terminal<CrosstermBackend<Stdout>>;

// --- Terminal lifecycle ------------------------------------------------------------------

static ACTIVE: AtomicBool = AtomicBool::new(false);

fn enter_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide, EnableMouseCapture, EnableBracketedPaste)
}

/// Undo everything `enter_terminal` did. Safe to call any number of times and from any
/// thread (guard drop, panic hook, signal thread).
pub fn restore_terminal() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Panics on the UI thread restore the terminal before the message is printed. Panics on a
/// job thread only go to the log: the job reports them as a failure and the UI stays up.
fn install_panic_hook() {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some(JOB_THREAD) {
            ctx::log("ERROR", &format!("Internal error in job: {info}"));
            return;
        }
        restore_terminal();
        prev(info);
        ctx::cleanup_temp_state();
    }));
}

pub const JOB_THREAD: &str = "kas-job";

// --- Input thread --------------------------------------------------------------------------

/// While set, the input thread stops reading so a suspended program owns the keyboard.
static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);
static INPUT_IDLE: AtomicBool = AtomicBool::new(false);

fn spawn_input(tx: Sender<Event>) {
    std::thread::spawn(move || {
        loop {
            if INPUT_PAUSED.load(Ordering::SeqCst) {
                INPUT_IDLE.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            INPUT_IDLE.store(false, Ordering::SeqCst);
            match ct::poll(Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            let ev = match ct::read() {
                Ok(ct::Event::Key(k)) => Event::Key(k),
                Ok(ct::Event::Mouse(m)) => Event::Mouse(m),
                Ok(ct::Event::Paste(s)) => Event::Paste(s),
                Ok(ct::Event::Resize(w, h)) => Event::Resize(w, h),
                Ok(_) => continue,
                Err(_) => return,
            };
            if tx.send(ev).is_err() {
                return;
            }
        }
    });
}

/// Leave the full-screen UI, run `f` on the real terminal, then come back with a full redraw.
fn suspend<T>(term: &mut Term, f: impl FnOnce() -> T) -> T {
    INPUT_PAUSED.store(true, Ordering::SeqCst);
    while !INPUT_IDLE.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    restore_terminal();
    let out = f();
    let _ = enter_terminal();
    let _ = term.clear();
    INPUT_PAUSED.store(false, Ordering::SeqCst);
    out
}

// --- Runtime -------------------------------------------------------------------------------

fn header() -> Header {
    let c = ctx();
    Header {
        user: c.target_user.clone(),
        uid: c.target_uid,
        gid: c.target_gid,
        home: c.target_home.display().to_string(),
        host: nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|_| "unknown".into()),
        root: c.is_root,
        log_file: c.log_file.display().to_string(),
    }
}

/// Where the app opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Menu,
    Import,
    Export,
}

pub fn run(start: Start) -> Result<()> {
    install_panic_hook();
    enter_terminal()?;
    let _guard = TerminalGuard;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = term.size()?;
    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());

    let mut app = App::new(header(), theme::Theme::from_env(), (size.width, size.height));
    let replies = Arc::new(Replies::default());
    let started = Instant::now();
    app.now = crate::ui::now();
    let first = match start {
        Start::Menu => None,
        Start::Import => app::activate(&mut app, Action::Import),
        Start::Export => app::activate(&mut app, Action::Export),
    };
    if let Some(cmd) = first {
        execute_command(&mut app, &mut term, cmd, &tx, &replies);
    }
    loop {
        app.now = crate::ui::now();
        app.now_ms = started.elapsed().as_millis() as u64;
        term.draw(|f| view(&app, f))?;
        let Some(ev) = next_event(&rx, app.animating()) else { break };
        app.now_ms = started.elapsed().as_millis() as u64;
        if let Some(cmd) = update(&mut app, ev) {
            execute_command(&mut app, &mut term, cmd, &tx, &replies);
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

/// Block for the next event; while something animates, wake up every 100 ms for a tick.
fn next_event(rx: &Receiver<Event>, animating: bool) -> Option<Event> {
    if !animating {
        return rx.recv().ok();
    }
    match rx.recv_timeout(Duration::from_millis(100)) {
        Ok(ev) => Some(ev),
        Err(RecvTimeoutError::Timeout) => Some(Event::Tick),
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// The last lines of the log file (it can grow large: only the tail is read).
fn read_log_tail() -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(&ctx().log_file) else { return vec!["(log file not readable)".into()] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(2 * 1024 * 1024)));
    let mut bytes = Vec::new();
    let _ = f.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(screens::log::MAX_LINES);
    lines[skip..].iter().map(|l| crate::ui::sanitize_log(l)).collect()
}

/// Reply channels of engine questions that wait for an answer, by question id.
#[derive(Default)]
pub struct Replies(Mutex<HashMap<u64, Sender<bool>>>);

impl Replies {
    pub fn insert(&self, id: u64, tx: Sender<bool>) {
        self.0.lock().unwrap().insert(id, tx);
    }

    fn answer(&self, id: u64, yes: bool) {
        if let Some(tx) = self.0.lock().unwrap().remove(&id) {
            let _ = tx.send(yes);
        }
    }

    fn decline_all(&self) {
        for (_, tx) in self.0.lock().unwrap().drain() {
            let _ = tx.send(false);
        }
    }
}

fn execute_command(app: &mut App, term: &mut Term, cmd: Command, tx: &Sender<Event>, replies: &Arc<Replies>) {
    match cmd {
        Command::Start(kind) => job::spawn(kind, tx.clone(), replies.clone()),
        Command::Answer { id, yes } => replies.answer(id, yes),
        Command::Cancel => {
            report::request_cancel();
            replies.decline_all();
        }
        Command::LoadCandidates { zips_only } => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Loaded(event::Loaded::Candidates(screens::import::load_candidates(zips_only))));
            });
        }
        Command::Inspect { path, pick } => {
            let result = screens::import::inspect(&path);
            let _ = tx.send(Event::Loaded(event::Loaded::Inspected { path, pick, result }));
        }
        Command::Complete(value) => {
            let (value, matches) = screens::import::complete_path(&value, &ctx().target_home);
            let _ = tx.send(Event::Loaded(event::Loaded::Completion { value, matches }));
        }
        Command::OpenLog => {
            let _ = tx.send(Event::Loaded(event::Loaded::Log(read_log_tail())));
        }
        Command::Legacy(action) => {
            let result = suspend(term, || legacy_action(action));
            match result {
                Ok(()) => {}
                Err(msg) => app.toast(ToastKind::Error, msg),
            }
        }
    }
}

/// Actions not yet ported to the full-screen UI run on the plain terminal.
fn legacy_action(action: Action) -> std::result::Result<(), String> {
    use crate::{interactive, setup, ui, vault};
    ui::init_terminal();
    let result = panic::catch_unwind(AssertUnwindSafe(|| match action {
        Action::Auto => setup::run_automatic(),
        Action::Partition => setup::setup_data_partition(false, 0, 1, 1),
        Action::Repo => setup::add_kud3n_repo(false, 0, 1, 1),
        Action::Packages => {
            setup::install_packages(false, 0, 1, 1);
            Ok(())
        }
        Action::Services => setup::setup_services(false, 0, 1, 1),
        Action::Import => interactive::import_data(),
        Action::Export => interactive::export_data(),
        Action::Status => {
            ui::clear_screen();
            vault::status::cmd_status();
            ui::read_choice("\nPress Enter to return to menu...");
            Ok(())
        }
        Action::Info => {
            ui::clear_screen();
            let p = ui::read_choice("Archive to inspect: ");
            let r = vault::info::cmd_info(Some(std::path::Path::new(&p)));
            ui::read_choice("\nPress Enter to return to menu...");
            r
        }
        Action::Quit => Ok(()),
    }));
    ctx::cleanup_temp_state();
    let msg = match result {
        Ok(Ok(())) => {
            ui::cleanup_terminal();
            return Ok(());
        }
        Ok(Err(e)) if e.is::<Reported>() => String::new(),
        Ok(Err(e)) => format!(": {e:#}"),
        Err(_) => " (internal error)".into(),
    };
    println!("\n{C_RED}[ERROR]{C_RESET} Action failed{msg}. See {}", ctx().log_file.display());
    ui::read_choice("Press Enter to return to menu...");
    ui::cleanup_terminal();
    Err(format!("Action failed{msg}"))
}
