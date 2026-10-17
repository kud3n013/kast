//! Full-screen interface (Ratatui + crossterm). `run` owns the terminal from launch to quit:
//! one event loop on the UI thread, input read on a helper thread, slow work on job threads.

mod app;
mod event;
mod job;
mod screens;
mod theme;
mod widgets;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::panic;
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

use crate::ctx::{self, ctx};
use crate::logio::{self, Report};
use crate::report;
use app::{App, Command, Header, update, view};
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

pub const JOB_THREAD: &str = "kast-job";

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

/// Set while a program kast handed the terminal to runs: Ctrl+C then reaches kast too (raw mode
/// is off), and it is meant for that program.
static SUSPENDED: AtomicBool = AtomicBool::new(false);

pub fn suspended() -> bool {
    SUSPENDED.load(Ordering::SeqCst)
}

/// Leave the full-screen UI, run `f` on the real terminal, then come back with a full redraw.
/// For programs that must own the terminal themselves (the log in a pager or editor).
fn suspend<T>(term: &mut Term, f: impl FnOnce() -> T) -> T {
    INPUT_PAUSED.store(true, Ordering::SeqCst);
    while !INPUT_IDLE.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    SUSPENDED.store(true, Ordering::SeqCst);
    restore_terminal();
    let out = f();
    // Keys typed for the program that it never read must not reach the app
    let _ = nix::sys::termios::tcflush(io::stdin(), nix::sys::termios::FlushArg::TCIFLUSH);
    let _ = enter_terminal();
    while ct::poll(Duration::ZERO).unwrap_or(false) {
        if ct::read().is_err() {
            break;
        }
    }
    SUSPENDED.store(false, Ordering::SeqCst);
    // Full redraw. Not `term.clear()`: it asks the terminal for the cursor position and waits
    // for the answer. A blank frame makes ratatui's idea of the screen blank, so the next
    // frame repaints every cell.
    let _ = execute!(io::stdout(), ratatui::crossterm::terminal::Clear(ratatui::crossterm::terminal::ClearType::All));
    let _ = term.draw(|f| f.render_widget(ratatui::widgets::Clear, f.area()));
    INPUT_PAUSED.store(false, Ordering::SeqCst);
    out
}

/// Drop keys, clicks and pastes queued before or during a suspend; keep everything else.
fn drop_queued_input(rx: &Receiver<Event>, tx: &Sender<Event>) {
    let keep: Vec<Event> = rx.try_iter().filter(|e| !matches!(e, Event::Key(_) | Event::Mouse(_) | Event::Paste(_))).collect();
    for e in keep {
        let _ = tx.send(e);
    }
}

/// `e` / `E`: the log in a pager or editor. Nothing to report when it ran fine.
fn view_log(term: &mut Term, editor: bool) -> Option<Report> {
    let argv = match logio::viewer(editor, &ctx().log_file, &logio::os::env, &logio::os::have) {
        Ok(a) => a,
        Err(e) => return Some(Report::err(e)),
    };
    let status = suspend(term, || logio::viewer_command(&argv).status());
    match status {
        Ok(s) if s.success() || s.code().is_none() => None,
        Ok(s) => Some(Report::err(format!("{} exited with status {}", argv[0], s.code().unwrap_or_default()))),
        Err(e) => Some(Report::err(format!("Could not run {}: {e}", argv[0]))),
    }
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

/// `list`: the package list when it's known up front (`--offline`, `--packages-file`);
/// otherwise it's downloaded in the background while the menu shows.
pub fn run(start: Start, list: Option<crate::pkglist::PkgList>) -> Result<()> {
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
    let len = log_len();
    let mut marks = LogMarks { session: len, job: len };
    match list {
        Some(pl) => app.packages = screens::packages::PkgState::Ready(Arc::new(pl)),
        None => execute_command(Command::FetchPackages, &tx, &replies, &mut marks),
    }
    if let Some(cmd) = first {
        execute_command(cmd, &tx, &replies, &mut marks);
    }
    loop {
        app.now = crate::ui::now();
        app.now_ms = started.elapsed().as_millis() as u64;
        term.draw(|f| view(&app, f))?;
        let Some(ev) = next_event(&rx, app.animating()) else { break };
        app.now_ms = started.elapsed().as_millis() as u64;
        match update(&mut app, ev) {
            Some(Command::ViewLog { editor }) => {
                let report = view_log(&mut term, editor);
                drop_queued_input(&rx, &tx);
                if let Ok(size) = term.size() {
                    let _ = tx.send(Event::Resize(size.width, size.height));
                }
                if let Some(r) = report {
                    let _ = tx.send(Event::Loaded(event::Loaded::LogIo(r)));
                }
            }
            Some(Command::Osc52(bytes)) => {
                use std::io::Write;
                let out = term.backend_mut();
                if let Err(e) = out.write_all(&bytes).and_then(|_| out.flush()) {
                    let _ = tx.send(Event::Loaded(event::Loaded::LogIo(logio::os::copy_failed(&e, logio::TUI_HINT))));
                }
            }
            Some(cmd) => execute_command(cmd, &tx, &replies, &mut marks),
            None => {}
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

/// Log file lengths when the app and the last job started: where `y` copies from.
struct LogMarks {
    session: u64,
    job: u64,
}

fn log_len() -> u64 {
    std::fs::metadata(&ctx().log_file).map(|m| m.len()).unwrap_or(0)
}

/// Hand a copy result to the app: a toast, or OSC 52 bytes for the event loop to write.
fn copied(tx: &Sender<Event>, c: logio::os::Copied) {
    let ev = match c {
        logio::os::Copied::Done(r) => event::Loaded::LogIo(r),
        logio::os::Copied::Osc52 { bytes, report } => event::Loaded::Osc52 { bytes, report },
    };
    let _ = tx.send(Event::Loaded(ev));
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

fn execute_command(cmd: Command, tx: &Sender<Event>, replies: &Arc<Replies>, marks: &mut LogMarks) {
    match cmd {
        Command::Start(kind) => {
            marks.job = log_len();
            job::spawn(kind, tx.clone(), replies.clone())
        }
        Command::Answer { id, yes } => replies.answer(id, yes),
        Command::Cancel => {
            report::request_cancel();
            replies.decline_all();
        }
        Command::LoadCandidates { dir, user, zips_only } => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Loaded(event::Loaded::Candidates(screens::import::load_candidates(&dir, &user, zips_only))));
            });
        }
        Command::LoadInstalled(names) => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let set = names.into_iter().filter(|n| crate::setup::pkg_installed(n)).collect();
                let _ = tx.send(Event::Loaded(event::Loaded::Installed(set)));
            });
        }
        Command::MkdirUser(path) => {
            let result = crate::ctx::mkdir_user(&path).map_err(|e| format!("Could not create {}: {e}", path.display()));
            let _ = tx.send(Event::Loaded(event::Loaded::DirCreated { path, result }));
        }
        Command::Inspect { path, pick } => {
            let result = screens::import::inspect(&path);
            let _ = tx.send(Event::Loaded(event::Loaded::Inspected { path, pick, result }));
        }
        Command::Complete(value) => {
            let (value, matches) = screens::import::complete_path(&value, &ctx().target_home);
            let _ = tx.send(Event::Loaded(event::Loaded::Completion { value, matches }));
        }
        Command::LoadExportTargets => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let targets = screens::export::load_targets();
                let ts = screens::export::timestamp();
                let _ = tx.send(Event::Loaded(event::Loaded::ExportTargets { targets, ts }));
            });
        }
        Command::FetchPackages => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let pl = crate::pkglist::load(false, crate::pkglist::curl_fetch);
                let _ = tx.send(Event::Loaded(event::Loaded::PackageList(Box::new(pl))));
            });
        }
        Command::LoadStatus => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Loaded(event::Loaded::Status(Box::new(crate::vault::status::collect_status()))));
            });
        }
        Command::OpenLog => {
            let _ = tx.send(Event::Loaded(event::Loaded::Log(read_log_tail())));
        }
        // Needs the terminal: handled in the event loop
        Command::ViewLog { .. } => {}
        Command::CopyLog(scope) => {
            let from = match scope {
                app::LogScope::Session => marks.session,
                app::LogScope::Job => marks.job,
            };
            let tx = tx.clone();
            std::thread::spawn(move || copied(&tx, logio::os::copy_log(&ctx().log_file, from, logio::TUI_HINT)));
        }
        Command::CopyLogPath => {
            let tx = tx.clone();
            std::thread::spawn(move || copied(&tx, logio::os::copy(&ctx().log_file.display().to_string(), "the log path", logio::TUI_HINT)));
        }
        // Written by the event loop, between frames
        Command::Osc52(_) => {}
        Command::OpenLogFile | Command::ShowLogFile => {
            let tx = tx.clone();
            let show = matches!(cmd, Command::ShowLogFile);
            std::thread::spawn(move || {
                let path = &ctx().log_file;
                let r = if show { logio::os::show_in_folder(path, logio::TUI_HINT) } else { logio::os::open_file(path, logio::TUI_HINT) };
                let _ = tx.send(Event::Loaded(event::Loaded::LogIo(r)));
            });
        }
    }
}
