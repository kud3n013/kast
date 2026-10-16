//! Terminal handling, dual/single progress dashboards and monitored command execution.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use nix::sys::termios::{self, LocalFlags, InputFlags, SetArg, Termios};

use crate::ctx::{ctx, log, log_stdio};

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
pub const C_BG_BLUE: &str = "\x1b[44m";

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

// --- Terminal state ----------------------------------------------------------------

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static SAVED_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);

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

pub fn init_terminal() {
    TUI_ACTIVE.store(true, Ordering::SeqCst);
    out("\x1b[?1049h\x1b[?25l");
    let stdin = io::stdin();
    if let Ok(t) = termios::tcgetattr(stdin.as_fd()) {
        SAVED_TERMIOS.lock().unwrap().get_or_insert(t.clone());
        let mut t = t;
        t.local_flags.remove(LocalFlags::ECHO);
        t.input_flags.remove(InputFlags::IXON);
        let _ = termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &t);
    }
    out("\x1b[H\x1b[2J");
}

/// Only undo terminal changes if the TUI made them; otherwise leaving the alternate
/// screen can clobber plain CLI output.
pub fn cleanup_terminal() {
    if !TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    termios_restore_cooked();
    out("\x1b[?25h\x1b[?1049l");
}

pub fn restore_terminal_interactive() {
    if TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        out("\x1b[?1049l");
    }
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

pub enum Key {
    Up,
    Down,
    Enter,
    Char(char),
    Other,
    Eof,
}

/// Read one key press (`read -rsn1` plus arrow-key escape sequences).
pub fn read_key() -> Key {
    let stdin = io::stdin();
    let fd = stdin.as_fd();
    let saved = termios::tcgetattr(fd).ok();
    if let Some(t) = &saved {
        let mut raw = t.clone();
        raw.local_flags.remove(LocalFlags::ICANON | LocalFlags::ECHO);
        raw.control_chars[libc::VMIN] = 1;
        raw.control_chars[libc::VTIME] = 0;
        let _ = termios::tcsetattr(fd, SetArg::TCSANOW, &raw);
    }
    let key = match read_byte() {
        None => Key::Eof,
        Some(b) => match b {
            b'\n' | b'\r' => Key::Enter,
            0x1b => {
                let mut rest = [0u8; 2];
                let mut n = 0;
                while n < 2 && poll_stdin(100) {
                    match read_byte() {
                        Some(b) => {
                            rest[n] = b;
                            n += 1;
                        }
                        None => break,
                    }
                }
                match &rest[..n] {
                    b"[A" => Key::Up,
                    b"[B" => Key::Down,
                    _ => Key::Other,
                }
            }
            b => Key::Char(b as char),
        },
    };
    if let Some(t) = &saved {
        let _ = termios::tcsetattr(fd, SetArg::TCSANOW, t);
    }
    key
}

fn poll_stdin(ms: u16) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let stdin = io::stdin();
    let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    matches!(poll(&mut fds, PollTimeout::from(ms)), Ok(n) if n > 0)
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
    if m >= 60 {
        format!("{:02}:{:02}:{:02}", m / 60, m % 60, sec)
    } else {
        format!("{m:02}:{sec:02}")
    }
}

pub fn calc_eta_countdown(start: i64, current: i64, total: i64, baseline: i64) -> String {
    let elapsed = (now() - start).max(0);
    if total <= 0 || current >= total {
        return "00:00".into();
    }
    let remaining = total - current;
    let est = if current <= 0 {
        (remaining * baseline - elapsed).max(5)
    } else {
        remaining * (elapsed / current).max(1)
    };
    format_seconds(est)
}

pub fn calc_overall_progress(o_step: i64, s_step: i64, s_total: i64) -> i64 {
    const WEIGHTS: [i64; 6] = [25, 5, 5, 50, 10, 5];
    let idx = (o_step.max(1) - 1) as usize;
    let cum: i64 = WEIGHTS.iter().take(idx).sum();
    let cur = WEIGHTS.get(idx).copied().unwrap_or(10);
    let sub = if s_total > 0 { s_step * cur / s_total } else { 0 };
    (cum + sub).min(100)
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
        let spinner = spinner.unwrap_or(SPINNER[SPINNER_IDX.load(Ordering::Relaxed)]);
        let (box_w, inner) = term_dimensions();
        let now = now();
        let s_pct = if s_total > 0 { s_step * 100 / s_total } else { 0 };
        let s_elapsed = format_seconds(now - self.s_start);
        let s_eta = calc_eta_countdown(self.s_start, s_step, s_total, 15);
        let title = &self.title;

        let mut buf = hline(box_w, BOX_TL, BOX_TR);
        if self.auto {
            let o_pct = calc_overall_progress(self.o_step, s_step, s_total);
            let o_elapsed = (now - self.o_start).max(0);
            let o_eta = if o_pct >= 100 {
                0
            } else if o_pct >= 2 {
                (o_elapsed * 100 / o_pct - o_elapsed).max(5)
            } else {
                (420 - o_elapsed).max(10)
            };
            let heading = if inner < 56 { "◆ ARCH LINUX SETUP ◆" } else { "◆ ARCH LINUX / CACHYOS SETUP CONTROLLER ◆" };
            buf += &box_line(&format!("{C_BOLD}{C_WHITE}{heading}{C_RESET}"), inner);
            buf += &hline(box_w, BOX_VL, BOX_VR);
            let (o_step, o_total) = (self.o_step, self.o_total);
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

    /// Run `cmd` with output appended to the log while the dashboard shows its latest
    /// line. stdin is /dev/null, so any prompt fails fast instead of hanging unseen.
    /// Returns the exit code (127 if it could not be started).
    pub fn exec_monitored(&self, s_step: i64, s_total: i64, item: &str, mut cmd: Command) -> i32 {
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
}

fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
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
    let line = text
        .split(['\n', '\r'])
        .rev()
        .find(|l| !l.trim().is_empty())?
        .to_string();
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
        assert_eq!(calc_overall_progress(1, 0, 2), 0);
        assert_eq!(calc_overall_progress(4, 1, 2), 35 + 25);
        assert_eq!(calc_overall_progress(6, 2, 2), 100);
    }

    #[test]
    fn bar_width() {
        assert_eq!(draw_bar(50, 4), "▰▰▱▱");
        assert_eq!(draw_bar(150, 2), "▰▰");
    }
}
