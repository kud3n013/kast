//! Plain-terminal output: line prompts, the text dashboards of scripted runs, monitored
//! command execution, and the progress math shared with the full-screen UI.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nix::sys::termios::{self, InputFlags, LocalFlags, SetArg};

use crate::ctx::{self, ctx, log, log_stdio};
use crate::report::{self, Progress};

pub const C_RESET: &str = "\x1b[0m";
pub const C_BOLD: &str = "\x1b[1m";
pub const C_RED: &str = "\x1b[31m";
pub const C_GREEN: &str = "\x1b[32m";
pub const C_YELLOW: &str = "\x1b[33m";
pub const C_BLUE: &str = "\x1b[34m";
pub const C_MAGENTA: &str = "\x1b[35m";
pub const C_CYAN: &str = "\x1b[36m";
pub const C_WHITE: &str = "\x1b[37m";
pub const C_GRAY: &str = "\x1b[90m";

const BOX_H: &str = "─";
const BOX_V: &str = "│";
const BOX_TL: &str = "╭";
const BOX_TR: &str = "╮";
const BOX_BL: &str = "╰";
const BOX_BR: &str = "╯";
const BOX_VL: &str = "├";
const BOX_VR: &str = "┤";

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
static SPINNER_IDX: AtomicUsize = AtomicUsize::new(0);

// --- Plain terminal (scripted subcommands and the line-based fallbacks) ----------------
// The full-screen UI lives in `tui`; this only serves output that scrolls.

fn out(s: &str) {
    let mut o = io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

/// `stty echo ixon icanon icrnl`
fn termios_restore_cooked() {
    let stdin = io::stdin();
    if let Ok(mut t) = termios::tcgetattr(stdin.as_fd()) {
        t.local_flags |= LocalFlags::ECHO | LocalFlags::ICANON;
        t.input_flags |= InputFlags::IXON | InputFlags::ICRNL;
        let _ = termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &t);
    }
}

/// Visible cursor and a cooked terminal, ready for a line prompt.
pub fn restore_terminal_interactive() {
    out("\x1b[?25h");
    termios_restore_cooked();
}

pub fn clear_screen() {
    out("\x1b[H\x1b[2J");
}

/// Discard pending keyboard input (typeahead during long tasks).
fn flush_input() {
    let _ = termios::tcflush(io::stdin().as_fd(), termios::FlushArg::TCIFLUSH);
}

/// Prompt for a line in cooked mode; returns it trimmed (empty on EOF).
pub fn read_choice(prompt: &str) -> String {
    restore_terminal_interactive();
    out(prompt);
    let mut line = Vec::new();
    while let Some(b) = read_byte() {
        if b == b'\n' {
            break;
        }
        line.push(b);
    }
    String::from_utf8_lossy(&line).trim().to_string()
}

/// One byte straight from fd 0. std's Stdin is buffered, which would swallow the rest of
/// an escape sequence (and typeahead) where poll() and tcflush() can't see it.
fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        match nix::unistd::read(io::stdin().as_fd(), &mut b) {
            Ok(1) => return Some(b[0]),
            Err(nix::errno::Errno::EINTR) => continue,
            _ => return None,
        }
    }
}

fn term_cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ only writes into the winsize struct we pass.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

/// (box width, inner width) for the current terminal.
pub fn term_dimensions() -> (usize, usize) {
    let usable = term_cols().saturating_sub(2).clamp(36, 96);
    (usable, (usable - 4).max(20))
}

// --- String & layout helpers ---------------------------------------------------------

pub fn strip_ansi(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut look = chars.clone();
            look.next();
            while matches!(look.peek(), Some(ch) if ch.is_ascii_digit() || *ch == ';') {
                look.next();
            }
            if matches!(look.peek(), Some(ch) if ch.is_ascii_alphabetic()) {
                look.next();
                chars = look;
                continue;
            }
        }
        res.push(c);
    }
    res
}

pub fn sanitize_log(s: &str) -> String {
    strip_ansi(&s.replace(['\r', '\n'], "").replace('\t', "  "))
}

pub fn box_line(text: &str, inner: usize) -> String {
    let raw = strip_ansi(text);
    let mut raw_len = raw.chars().count();
    let mut text = text.to_string();
    if raw_len > inner {
        // Truncating mid-escape would break colors, so fall back to the plain text
        let src = if text.contains('\x1b') { &raw } else { &text };
        text = format!("{}...", src.chars().take(inner.saturating_sub(3)).collect::<String>());
        raw_len = inner;
    }
    let pad = " ".repeat(inner - raw_len);
    format!("{C_CYAN}{BOX_V}{C_RESET} {text}{pad} {C_CYAN}{BOX_V}{C_RESET}\x1b[K\n")
}

fn hline(box_w: usize, l: &str, r: &str) -> String {
    format!("{C_CYAN}{l}{}{r}{C_RESET}\x1b[K\n", BOX_H.repeat(box_w - 2))
}

// --- Time & progress math ------------------------------------------------------------

pub fn now() -> i64 {
    chrono::Local::now().timestamp()
}

pub fn format_seconds(s: i64) -> String {
    let s = s.max(0);
    let (m, sec) = (s / 60, s % 60);
    if m >= 60 { format!("{:02}:{:02}:{:02}", m / 60, m % 60, sec) } else { format!("{m:02}:{sec:02}") }
}

pub fn calc_eta_countdown(start: i64, current: i64, total: i64, baseline: i64) -> String {
    eta_countdown_at(now(), start, current, total, baseline)
}

/// `calc_eta_countdown` at a given time (the TUI passes its own clock).
pub fn eta_countdown_at(now: i64, start: i64, current: i64, total: i64, baseline: i64) -> String {
    let elapsed = (now - start).max(0);
    if total <= 0 || current >= total {
        return "00:00".into();
    }
    let remaining = total - current;
    let est = if current <= 0 { (remaining * baseline - elapsed).max(5) } else { remaining * (elapsed / current).max(1) };
    format_seconds(est)
}

/// The default automatic run (`kast auto`): upgrade, repo, packages, services.
pub const STANDARD_WEIGHTS: [i64; 4] = [25, 5, 60, 10];

static STEP_WEIGHTS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

/// The weights of the run that starts now (one per step); dashboards carry them.
pub fn set_step_weights(w: Vec<i64>) {
    if let Ok(mut g) = STEP_WEIGHTS.lock() {
        *g = w;
    }
}

fn step_weights() -> Vec<i64> {
    STEP_WEIGHTS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Overall percentage of an automatic run: the finished steps' weights plus the current
/// step's share, normalised over all of the run's steps. Reaches exactly 100 at the end.
pub fn calc_overall_progress(weights: &[i64], o_step: i64, s_step: i64, s_total: i64) -> i64 {
    let weights = if weights.is_empty() { &STANDARD_WEIGHTS[..] } else { weights };
    let total: i64 = weights.iter().sum::<i64>().max(1);
    let idx = ((o_step.max(1) - 1) as usize).min(weights.len());
    let cum: i64 = weights.iter().take(idx).sum();
    let cur = weights.get(idx).copied().unwrap_or(0);
    let s_total = s_total.max(1);
    let s_step = s_step.clamp(0, s_total);
    ((cum * s_total + s_step * cur) * 100 / (total * s_total)).min(100)
}

/// Seconds left in the automatic run, from its overall percentage.
pub fn overall_eta(o_pct: i64, o_elapsed: i64) -> i64 {
    if o_pct >= 100 {
        0
    } else if o_pct >= 2 {
        (o_elapsed * 100 / o_pct - o_elapsed).max(5)
    } else {
        (420 - o_elapsed).max(10)
    }
}

pub fn draw_bar(percent: i64, width: i64) -> String {
    let p = percent.clamp(0, 100);
    let w = width.max(1);
    let filled = p * w / 100;
    "▰".repeat(filled as usize) + &"▱".repeat((w - filled) as usize)
}

fn info_str(inner: usize, pct: i64, elapsed: &str, eta: &str) -> String {
    if inner < 45 {
        format!(" {pct:3}% │ ETA:{eta}")
    } else if inner < 58 {
        format!(" {pct:3}% │ {elapsed} │ ETA:{eta}")
    } else {
        format!(" {pct:3}% │ Elapsed: {elapsed} │ ETA: {eta}")
    }
}

fn bar_line(inner: usize, color: &str, pct: i64, info: &str) -> String {
    let w = (inner as i64 - 2 - info.chars().count() as i64).max(4);
    box_line(&format!("{color}[{}]{C_RESET}{info}", draw_bar(pct, w)), inner)
}

/// Progress context of one setup step; renders the dual (auto) or single dashboard.
pub struct Dash {
    pub auto: bool,
    pub o_step: i64,
    pub o_total: i64,
    pub o_start: i64,
    pub title: String,
    pub s_start: i64,
}

impl Dash {
    pub fn new(auto: bool, o_start: i64, o_step: i64, o_total: i64, title: &str) -> Self {
        Dash { auto, o_step, o_total, o_start, title: title.to_string(), s_start: now() }
    }

    pub fn render(&self, s_step: i64, s_total: i64, item: &str, recent_log: &str, spinner: Option<&str>) {
        report::progress(&Progress {
            auto: self.auto,
            o_step: self.o_step,
            o_total: self.o_total,
            weights: if self.auto { step_weights() } else { Vec::new() },
            o_start: self.o_start,
            title: self.title.clone(),
            s_start: self.s_start,
            s_step,
            s_total,
            item: item.to_string(),
            recent_log: recent_log.to_string(),
            mark: spinner.map(str::to_string),
        });
    }

    /// Run `cmd` with output appended to the log while the dashboard shows its latest
    /// line. stdin is /dev/null, so any prompt fails fast instead of hanging unseen.
    /// Returns the exit code (127 if it could not be started).
    pub fn exec_monitored(&self, s_step: i64, s_total: i64, item: &str, cmd: Command) -> i32 {
        let pacman = runs_pacman(&cmd);
        if pacman {
            ctx::note_pacman_start();
        }
        // A canceled job unwinds inside exec_piped, so the note survives only then
        let rc = if report::owns_terminal() { self.exec_piped(s_step, s_total, item, cmd) } else { self.exec_logged(s_step, s_total, item, cmd) };
        if pacman {
            ctx::note_pacman_done();
        }
        rc
    }

    fn exec_logged(&self, s_step: i64, s_total: i64, item: &str, mut cmd: Command) -> i32 {
        log("INFO", &format!("Executing monitored task: {item} ({})", describe(&cmd)));
        flush_input();
        let child = cmd.stdin(Stdio::null()).stdout(log_stdio()).stderr(log_stdio()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                log("ERROR", &format!("Could not start {}: {e}", describe(&cmd)));
                return 127;
            }
        };
        let mut last_log = String::from("Starting task...");
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => {}
                Err(_) => break None,
            }
            let idx = (SPINNER_IDX.load(Ordering::Relaxed) + 1) % SPINNER.len();
            SPINNER_IDX.store(idx, Ordering::Relaxed);
            if let Some(line) = last_log_line() {
                last_log = line;
            }
            self.render(s_step, s_total, item, &last_log, Some(SPINNER[idx]));
            std::thread::sleep(Duration::from_millis(200));
        };
        flush_input();
        match status {
            Some(st) => st.code().unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(0)),
            None => 1,
        }
    }

    /// Under the full-screen UI: the child's output comes through a pipe, is appended to the
    /// log as-is and is shown line by line. The child gets its own process group so a
    /// canceled job can stop it (and whatever sudo/yay started) as a whole.
    fn exec_piped(&self, s_step: i64, s_total: i64, item: &str, mut cmd: Command) -> i32 {
        use std::os::unix::process::CommandExt;
        report::checkpoint();
        log("INFO", &format!("Executing monitored task: {item} ({})", describe(&cmd)));
        let spawned = io::pipe().and_then(|(reader, writer)| {
            cmd.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer).process_group(0);
            let child = cmd.spawn()?;
            Ok((reader, child))
        });
        // Close our copies of the pipe's write end, or the reader never sees EOF
        drop(cmd);
        let (reader, mut child) = match spawned {
            Ok(v) => v,
            Err(e) => {
                log("ERROR", &format!("Could not start {item}: {e}"));
                return 127;
            }
        };
        report::set_child(Some(child.id() as i32));
        let rep = report::current();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            pump_output(reader, rep.as_ref());
            let _ = done_tx.send(());
        });
        self.render(s_step, s_total, item, "Starting task...", None);
        let status = child.wait();
        report::set_child(None);
        // A daemon the child started (gpg-agent from a pacman hook) can keep the pipe open
        // after the child is gone: don't wait for EOF forever
        let _ = done_rx.recv_timeout(Duration::from_millis(500));
        report::checkpoint();
        match status {
            Ok(st) => st.code().unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(0)),
            Err(_) => 1,
        }
    }
}

/// Copy a child's output into the log file and to the reporter, line by line.
fn pump_output(mut reader: io::PipeReader, rep: &dyn report::Reporter) {
    let mut split = report::LineSplitter::default();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        crate::ctx::log_raw(&buf[..n]);
        for piece in split.feed(&buf[..n]) {
            match piece {
                report::Piece::Line(l) => rep.output_line(&l),
                report::Piece::Partial(l) => rep.output_partial(&l),
            }
        }
    }
    if let Some(rest) = split.finish() {
        rep.output_line(&rest);
    }
}

/// The text dashboard of the plain terminal mode (dual for the automatic run, single otherwise).
pub fn draw_dashboard(p: &Progress) {
    let (s_step, s_total, item, recent_log) = (p.s_step, p.s_total, p.item.as_str(), p.recent_log.as_str());
    let spinner = p.mark.as_deref().unwrap_or(SPINNER[SPINNER_IDX.load(Ordering::Relaxed)]);
    let (box_w, inner) = term_dimensions();
    let now = now();
    let s_pct = if s_total > 0 { s_step * 100 / s_total } else { 0 };
    let s_elapsed = format_seconds(now - p.s_start);
    let s_eta = calc_eta_countdown(p.s_start, s_step, s_total, 15);
    let title = &p.title;

    let mut buf = hline(box_w, BOX_TL, BOX_TR);
    if p.auto {
        let o_pct = calc_overall_progress(&p.weights, p.o_step, s_step, s_total);
        let o_elapsed = (now - p.o_start).max(0);
        let o_eta = overall_eta(o_pct, o_elapsed);
        let heading = if inner < 56 { "◆ ARCH LINUX SETUP ◆" } else { "◆ ARCH LINUX / CACHYOS SETUP CONTROLLER ◆" };
        buf += &box_line(&format!("{C_BOLD}{C_WHITE}{heading}{C_RESET}"), inner);
        buf += &hline(box_w, BOX_VL, BOX_VR);
        let (o_step, o_total) = (p.o_step, p.o_total);
        let o_header = if inner < 56 {
            format!("{C_BLUE}{C_BOLD}[ALL]{C_RESET} Step {o_step}/{o_total}: {title}")
        } else {
            format!("{C_BLUE}{C_BOLD}[UNIVERSAL PROGRESS]{C_RESET} Step {o_step}/{o_total}: {title}")
        };
        buf += &box_line(&o_header, inner);
        let o_info = info_str(inner, o_pct, &format_seconds(o_elapsed), &format_seconds(o_eta));
        buf += &bar_line(inner, C_GREEN, o_pct, &o_info);
        buf += &hline(box_w, BOX_VL, BOX_VR);
        let s_header = if inner < 56 {
            format!("{C_MAGENTA}{C_BOLD}[STATE]{C_RESET} {s_step}/{s_total}: {title}")
        } else {
            format!("{C_MAGENTA}{C_BOLD}[STATE PROGRESS]{C_RESET} {title} ({s_step}/{s_total})")
        };
        buf += &box_line(&s_header, inner);
        buf += &bar_line(inner, C_YELLOW, s_pct, &info_str(inner, s_pct, &s_elapsed, &s_eta));
        buf += &box_line(&format!("  {C_BOLD}Active:{C_RESET} {item}"), inner);
    } else {
        let upper = title.to_uppercase();
        let heading = if inner < 56 { format!("◆ TASK: {upper} ◆") } else { format!("◆ TASK EXECUTION: {upper} ◆") };
        buf += &box_line(&format!("{C_BOLD}{C_WHITE}{heading}{C_RESET}"), inner);
        buf += &hline(box_w, BOX_VL, BOX_VR);
        let header = if inner < 56 {
            format!("{C_MAGENTA}{C_BOLD}[PROGRESS]{C_RESET} {s_step}/{s_total}: {item}")
        } else {
            format!("{C_MAGENTA}{C_BOLD}[PROGRESS]{C_RESET} {s_step}/{s_total} - {item}")
        };
        buf += &box_line(&header, inner);
        buf += &bar_line(inner, C_GREEN, s_pct, &info_str(inner, s_pct, &s_elapsed, &s_eta));
    }
    buf += &hline(box_w, BOX_VL, BOX_VR);
    let clean = sanitize_log(recent_log);
    buf += &box_line(&format!("{C_CYAN}{spinner}{C_RESET} {C_GRAY}Log: {clean}{C_RESET}"), inner);
    buf += &hline(box_w, BOX_BL, BOX_BR);
    out(&format!("\x1b[H{buf}"));
}

/// pacman itself, or yay (which runs pacman through sudo).
fn runs_pacman(cmd: &Command) -> bool {
    cmd.get_program() == "pacman" || cmd.get_args().any(|a| a == "yay")
}

fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

/// Last non-blank line of the log (progress bars rewrite with \r, so the last
/// \r-segment), with the "[ts] [LEVEL] " prefix removed.
fn last_log_line() -> Option<String> {
    let mut f = File::open(&ctx().log_file).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(8192))).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text.split(['\n', '\r']).rev().find(|l| !l.trim().is_empty())?.to_string();
    let mut line = line.as_str();
    for tag in ["[INFO] ", "[WARN] ", "[ERROR] "] {
        if let Some(i) = line.find(tag) {
            line = &line[i + tag.len()..];
        }
    }
    Some(line.to_string())
}

pub fn sleep_secs(s: f64) {
    std::thread::sleep(Duration::from_secs_f64(s));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_format() {
        assert_eq!(format_seconds(-3), "00:00");
        assert_eq!(format_seconds(75), "01:15");
        assert_eq!(format_seconds(3725), "01:02:05");
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[36mhi\x1b[0m there\x1b[K"), "hi there");
        assert_eq!(strip_ansi("plain [x]"), "plain [x]");
        assert_eq!(sanitize_log("a\tb\r\n"), "a  b");
    }

    #[test]
    fn box_line_pads_and_truncates() {
        let l = box_line("abc", 10);
        assert!(strip_ansi(&l).starts_with("│ abc        │"));
        let t = box_line(&"x".repeat(30), 10);
        assert!(strip_ansi(&t).contains("xxxxxxx..."));
    }

    #[test]
    fn overall_progress_weights() {
        let w = STANDARD_WEIGHTS;
        assert_eq!(calc_overall_progress(&w, 1, 0, 2), 0);
        assert_eq!(calc_overall_progress(&w, 3, 1, 2), 30 + 30);
        assert_eq!(calc_overall_progress(&w, 4, 2, 2), 100);
        assert_eq!(calc_overall_progress(&[], 3, 1, 2), 60, "no weights: the standard run");
        // Any subset of steps: monotonic, and exactly 100 at the end of the last step
        for w in [vec![25], vec![10, 15, 25, 5, 60, 10, 15], vec![15, 15], vec![60, 10], vec![10, 5]] {
            let mut last = 0;
            for o in 1..=w.len() as i64 {
                for s in 0..=7 {
                    let p = calc_overall_progress(&w, o, s, 7);
                    assert!(p >= last && p <= 100, "{w:?} step {o} {s}/7 gave {p} after {last}");
                    last = p;
                }
            }
            assert_eq!(last, 100, "{w:?}");
        }
    }

    #[test]
    fn bar_width() {
        assert_eq!(draw_bar(50, 4), "▰▰▱▱");
        assert_eq!(draw_bar(150, 2), "▰▰");
    }
}
