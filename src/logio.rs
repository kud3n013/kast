//! Getting the log out of kast: view it in a pager or editor, open it with the desktop's
//! default app, show it in the file manager, or put it on the clipboard. The functions here
//! only *build* commands and payloads; `os` runs them.

pub mod os;

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ctx::UserEnv;
use crate::ui::sanitize_log;

/// Most log text put on the clipboard; longer selections keep their tail.
pub const COPY_CAP: usize = 256 * 1024;
/// Largest OSC 52 base64 payload: many terminals drop longer sequences (about 100 KB).
pub const OSC52_CAP: usize = 74 * 1024;
/// Where root runs log.
pub const SYSTEM_LOG: &str = "/var/log/kast.log";

/// How the app points at its own viewer when a desktop route is missing.
pub const TUI_HINT: &str = "press e to view it here";
pub const OSC52_NOTE: &str = "if nothing pasted, your terminal may not support it";

/// What the user is told about an action (a toast in the app, a line on the command line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub ok: bool,
    pub text: String,
}

impl Report {
    pub fn ok(text: impl Into<String>) -> Self {
        Report { ok: true, text: text.into() }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Report { ok: false, text: text.into() }
    }
}

// --- Programs ------------------------------------------------------------------------------

/// A program to start: its name, arguments and extra environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Session variables a program run directly (kast not root) may be missing.
const SESSION_VARS: [&str; 5] = ["XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"];

impl Launch {
    /// `argv` as the target user: through `sudo -u <user> env ...` (like `ctx::run_as_user`)
    /// when kast is root, directly with the session variables otherwise, so it needs no sudo.
    pub fn as_user(root: bool, user: &UserEnv, argv: &[String]) -> Launch {
        if root {
            let mut args = vec!["-u".to_string(), user.user.clone(), "env".into()];
            args.extend(user.vars().into_iter().map(|(k, v)| format!("{k}={v}")));
            args.extend_from_slice(argv);
            return Launch { program: "sudo".into(), args, env: vec![] };
        }
        let env = user.vars().into_iter().filter(|(k, v)| SESSION_VARS.contains(&k.as_str()) && !v.is_empty()).collect();
        Launch { program: argv[0].clone(), args: argv[1..].to_vec(), env }
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args).envs(self.env.iter().map(|(k, v)| (k, v)));
        c
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The program line for `e` (a pager) or `E` (an editor), with `path` appended: the first of
/// `$KAST_PAGER`, `$PAGER`, `less -R +G`, `more` (or `$VISUAL`, `$EDITOR`, `nano`, `vi`) that
/// `have` finds. Variables may carry arguments (`PAGER="less -S"`).
pub fn viewer(editor: bool, path: &Path, env: &dyn Fn(&str) -> Option<String>, have: &dyn Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let (vars, defaults): (&[&str], Vec<Vec<String>>) = if editor {
        (&["VISUAL", "EDITOR"], vec![argv(&["nano"]), argv(&["vi"])])
    } else {
        (&["KAST_PAGER", "PAGER"], vec![argv(&["less", "-R", "+G"]), argv(&["more"])])
    };
    let from_env = vars.iter().filter_map(|v| env(v)).map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>());
    for mut cmd in from_env.chain(defaults.iter().cloned()) {
        if cmd.first().is_some_and(|p| have(p)) {
            cmd.push(path.display().to_string());
            return Ok(cmd);
        }
    }
    let tried: Vec<String> = vars.iter().map(|v| format!("${v}")).chain(defaults.iter().map(|d| d[0].clone())).collect();
    Err(format!("No {} found (tried {})", if editor { "editor" } else { "pager" }, tried.join(", ")))
}

/// The desktop's "open this" program for `target`: xdg-open, gio open, kde-open, kde-open5.
pub fn opener(target: &str, have: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    let cmd = if have("xdg-open") {
        argv(&["xdg-open"])
    } else if have("gio") {
        argv(&["gio", "open"])
    } else {
        ["kde-open", "kde-open5"].into_iter().find(|p| have(p)).map(|p| argv(&[p]))?
    };
    Some([cmd, vec![target.to_string()]].concat())
}

/// Ways to show `path` in the file manager, best first: the FileManager1 D-Bus call (highlights
/// the file in Dolphin, Nautilus, Nemo, Thunar), then opening its folder.
pub fn show_item(path: &Path, have: &dyn Fn(&str) -> bool) -> Vec<Vec<String>> {
    let uri = file_uri(path);
    let mut out = Vec::new();
    if have("gdbus") {
        out.push(argv(&[
            "gdbus",
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("['{uri}']"),
            "",
        ]));
    } else if have("dbus-send") {
        out.push(argv(&[
            "dbus-send",
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ]));
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    out.extend(opener(&dir.display().to_string(), have));
    out
}

/// `file://` URI of an absolute path, percent-encoding everything but unreserved characters.
pub fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

// --- Where we are ----------------------------------------------------------------------------

/// What decides which GUI and clipboard routes can work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// Resolved Wayland socket and X display of the target user ("" if none)
    pub wayland: String,
    pub display: String,
    pub term: String,
    /// Running over SSH (or mosh): a desktop on this machine is not the one the user sees
    pub remote: bool,
}

impl Session {
    /// Whether GUI programs started for the user show up in front of them.
    pub fn gui(&self) -> bool {
        !self.remote && self.term != "linux" && (!self.wayland.is_empty() || !self.display.is_empty())
    }

    /// Clipboard programs worth trying, best first.
    pub fn clipboard_tools(&self, have: &dyn Fn(&str) -> bool) -> Vec<Vec<String>> {
        if !self.gui() {
            return vec![];
        }
        let mut out = Vec::new();
        if !self.wayland.is_empty() && have("wl-copy") {
            out.push(argv(&["wl-copy"]));
        }
        if !self.display.is_empty() {
            if have("xclip") {
                out.push(argv(&["xclip", "-selection", "clipboard", "-in"]));
            }
            if have("xsel") {
                out.push(argv(&["xsel", "--clipboard", "--input"]));
            }
        }
        out
    }
}

/// A process name of an SSH or mosh session.
pub fn is_remote_shell(comm: &str) -> bool {
    matches!(comm, "sshd" | "sshd-session" | "mosh-server")
}

/// Terminal multiplexer between kast and the terminal (OSC 52 must be wrapped for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mux {
    None,
    Tmux,
    Screen,
}

/// From `$TMUX`/`$STY` being set, `$TERM`, and the names of our ancestor processes (sudo drops
/// the variables, the process tree still shows them). tmux is checked first: it sets
/// `TERM=screen*` by default.
pub fn mux(tmux: bool, sty: bool, term: &str, ancestors: &[String]) -> Mux {
    if tmux || term.starts_with("tmux") || ancestors.iter().any(|a| a.starts_with("tmux")) {
        Mux::Tmux
    } else if sty || term.starts_with("screen") || ancestors.iter().any(|a| a.eq_ignore_ascii_case("screen")) {
        Mux::Screen
    } else {
        Mux::None
    }
}

// --- Clipboard payloads ----------------------------------------------------------------------

/// Standard base64 (RFC 4648, with padding).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// At most `max` bytes from the end of `text`, starting on a line when it had to be cut.
pub fn tail(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let rest = &text[start..];
    let rest = rest.find('\n').map_or(rest, |i| &rest[i + 1..]);
    (rest, true)
}

/// The OSC 52 "set clipboard" sequence for `text`, wrapped for tmux or GNU screen, and
/// whether `text` was cut to fit `OSC52_CAP` (its tail is kept).
pub fn osc52(text: &str, mux: Mux) -> (Vec<u8>, bool) {
    let (text, cut) = tail(text, OSC52_CAP / 4 * 3);
    let b64 = base64(text.as_bytes());
    let seq = match mux {
        Mux::None => format!("\x1b]52;c;{b64}\x07"),
        // Passthrough: every ESC of the inner sequence doubled
        Mux::Tmux => format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\"),
        // screen limits the length of one DCS string: send the payload in pieces
        Mux::Screen => {
            let pieces: Vec<&str> = b64.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
            format!("\x1bP\x1b]52;c;{}\x07\x1b\\", pieces.join("\x1b\\\x1bP"))
        }
    };
    (seq.into_bytes(), cut)
}

/// Log text to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub text: String,
    pub lines: usize,
    /// Only the tail of the selection fit in the cap
    pub truncated: bool,
}

/// The log from byte offset `from` on (all of it if the file is now shorter), ANSI codes
/// removed, at most `cap` bytes: a longer selection keeps its tail, with a note on top.
pub fn read_since(path: &Path, from: u64, cap: usize) -> io::Result<Clip> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let from = if from > len { 0 } else { from };
    let start = from.max(len.saturating_sub(cap as u64));
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let truncated = start > from;
    let body = if truncated { bytes.iter().position(|&b| b == b'\n').map_or(&bytes[..], |i| &bytes[i + 1..]) } else { &bytes[..] };
    let lines: Vec<String> = String::from_utf8_lossy(body).lines().map(sanitize_log).collect();
    let mut text = String::new();
    if truncated {
        text.push_str(&format!("[kast: log truncated, full file at {}]\n", path.display()));
    }
    for l in &lines {
        text.push_str(l);
        text.push('\n');
    }
    Ok(Clip { text, lines: lines.len(), truncated })
}

/// Which file `kast log` reads: root's log when root; otherwise the user's own log, unless it
/// is empty (every run creates it) and root runs have written /var/log/kast.log.
pub fn cli_log_path(root: bool, own: &Path, own_len: u64, system_exists: bool) -> PathBuf {
    if !root && own_len == 0 && system_exists { PathBuf::from(SYSTEM_LOG) } else { own.to_path_buf() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        argv(v)
    }

    fn user() -> UserEnv {
        UserEnv {
            user: "p3ter".into(),
            uid: 1000,
            home: "/home/p3ter".into(),
            wayland: "wayland-0".into(),
            dbus: "unix:path=/run/user/1000/bus".into(),
            ..Default::default()
        }
    }

    #[test]
    fn as_user_goes_through_sudo_only_as_root() {
        let l = Launch::as_user(true, &user(), &s(&["xdg-open", "/var/log/kast.log"]));
        assert_eq!(l.program, "sudo");
        assert_eq!(&l.args[..4], &s(&["-u", "p3ter", "env", "HOME=/home/p3ter"]));
        assert!(l.args.contains(&"WAYLAND_DISPLAY=wayland-0".into()));
        assert_eq!(&l.args[l.args.len() - 2..], &s(&["xdg-open", "/var/log/kast.log"]));
        assert!(l.env.is_empty());

        let l = Launch::as_user(false, &user(), &s(&["xdg-open", "/x"]));
        assert_eq!((l.program.as_str(), l.args.clone()), ("xdg-open", s(&["/x"])));
        let keys: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS"]);
    }

    #[test]
    fn viewer_order_and_fallbacks() {
        let p = Path::new("/var/log/kast.log");
        let all = |_: &str| true;
        let none = |_: &str| None;
        assert_eq!(viewer(false, p, &none, &all).unwrap(), s(&["less", "-R", "+G", "/var/log/kast.log"]));
        assert_eq!(viewer(true, p, &none, &all).unwrap(), s(&["nano", "/var/log/kast.log"]));
        let env = |k: &str| match k {
            "PAGER" => Some("bat --paging=always".to_string()),
            "KAST_PAGER" => Some("missing-pager".to_string()),
            "EDITOR" => Some("hx".to_string()),
            _ => None,
        };
        let have = |p: &str| p != "missing-pager";
        assert_eq!(viewer(false, p, &env, &have).unwrap(), s(&["bat", "--paging=always", "/var/log/kast.log"]));
        assert_eq!(viewer(true, p, &env, &have).unwrap(), s(&["hx", "/var/log/kast.log"]));
        let only_more = |p: &str| p == "more";
        assert_eq!(viewer(false, p, &none, &only_more).unwrap()[0], "more");
        let nothing = |_: &str| false;
        assert_eq!(viewer(false, p, &none, &nothing).unwrap_err(), "No pager found (tried $KAST_PAGER, $PAGER, less, more)");
        assert_eq!(viewer(true, p, &none, &nothing).unwrap_err(), "No editor found (tried $VISUAL, $EDITOR, nano, vi)");
    }

    #[test]
    fn openers_in_order() {
        assert_eq!(opener("/x", &|_| true).unwrap(), s(&["xdg-open", "/x"]));
        assert_eq!(opener("/x", &|p| p == "gio").unwrap(), s(&["gio", "open", "/x"]));
        assert_eq!(opener("/x", &|p| p == "kde-open5").unwrap(), s(&["kde-open5", "/x"]));
        assert_eq!(opener("/x", &|_| false), None);
    }

    #[test]
    fn show_item_uses_file_manager1_then_the_folder() {
        let p = Path::new("/var/log/kast.log");
        let cmds = show_item(p, &|_| true);
        assert_eq!(cmds[0][0], "gdbus");
        assert_eq!(cmds[0][9], "['file:///var/log/kast.log']");
        assert_eq!(cmds[1], s(&["xdg-open", "/var/log"]));
        let cmds = show_item(p, &|t| t != "gdbus");
        assert_eq!(cmds[0][6], "array:string:file:///var/log/kast.log");
        assert_eq!(show_item(p, &|t| t == "xdg-open"), vec![s(&["xdg-open", "/var/log"])]);
    }

    #[test]
    fn file_uri_encoding() {
        assert_eq!(file_uri(Path::new("/var/log/kast.log")), "file:///var/log/kast.log");
        assert_eq!(file_uri(Path::new("/home/p/My Logs/kast ö.log")), "file:///home/p/My%20Logs/kast%20%C3%B6.log");
        assert_eq!(file_uri(Path::new("/a/'b'#%")), "file:///a/%27b%27%23%25");
    }

    #[test]
    fn gui_detection() {
        let mut s = Session { wayland: "wayland-0".into(), term: "xterm-kitty".into(), ..Default::default() };
        assert!(s.gui());
        s.remote = true;
        assert!(!s.gui());
        s = Session { display: ":0".into(), term: "linux".into(), ..Default::default() };
        assert!(!s.gui());
        s.term = "xterm-256color".into();
        assert!(s.gui());
        assert!(!Session { term: "xterm".into(), ..Default::default() }.gui());
        assert!(is_remote_shell("sshd-session") && !is_remote_shell("sudo"));
    }

    #[test]
    fn clipboard_tools_follow_the_session() {
        let all = |_: &str| true;
        let s = Session { wayland: "wayland-0".into(), display: ":0".into(), term: "xterm".into(), remote: false };
        assert_eq!(s.clipboard_tools(&all).iter().map(|c| c[0].as_str()).collect::<Vec<_>>(), ["wl-copy", "xclip", "xsel"]);
        let x11 = Session { wayland: "".into(), ..s.clone() };
        assert_eq!(x11.clipboard_tools(&|p| p != "xclip"), vec![argv(&["xsel", "--clipboard", "--input"])]);
        assert!(Session { remote: true, ..s }.clipboard_tools(&all).is_empty());
    }

    #[test]
    fn mux_detection() {
        assert_eq!(mux(false, false, "xterm-256color", &[]), Mux::None);
        assert_eq!(mux(true, false, "screen-256color", &[]), Mux::Tmux);
        assert_eq!(mux(false, false, "screen-256color", &s(&["bash", "tmux: server"])), Mux::Tmux);
        assert_eq!(mux(false, false, "tmux-256color", &[]), Mux::Tmux);
        assert_eq!(mux(false, true, "xterm", &[]), Mux::Screen);
        assert_eq!(mux(false, false, "screen.xterm-256color", &[]), Mux::Screen);
        assert_eq!(mux(false, false, "xterm", &s(&["bash", "SCREEN"])), Mux::Screen);
    }

    #[test]
    fn base64_rfc4648_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn osc52_framing() {
        assert_eq!(osc52("hi", Mux::None), (b"\x1b]52;c;aGk=\x07".to_vec(), false));
        assert_eq!(osc52("hi", Mux::Tmux).0, b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\".to_vec());
        assert_eq!(osc52("hi", Mux::Screen).0, b"\x1bP\x1b]52;c;aGk=\x07\x1b\\".to_vec());
        // Long payloads are split into DCS pieces for screen
        let long = "x".repeat(100);
        let seq = String::from_utf8(osc52(&long, Mux::Screen).0).unwrap();
        assert_eq!(seq.matches("\x1bP").count(), 2);
        assert!(seq.starts_with("\x1bP\x1b]52;c;eHh4") && seq.ends_with("\x07\x1b\\"));
    }

    #[test]
    fn osc52_keeps_the_tail_of_big_payloads() {
        let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let (seq, cut) = osc52(&text, Mux::None);
        assert!(cut);
        assert!(seq.len() <= OSC52_CAP + 16);
        let kept = tail(&text, OSC52_CAP / 4 * 3).0;
        assert!(kept.starts_with("line ") && kept.ends_with("line 19999\n"));
        assert_eq!(seq, format!("\x1b]52;c;{}\x07", base64(kept.as_bytes())).into_bytes());
    }

    #[test]
    fn tail_cuts_on_a_line_and_char_boundary() {
        assert_eq!(tail("abc", 10), ("abc", false));
        assert_eq!(tail("one\ntwo\nthree\n", 8), ("three\n", true));
        // The cut lands inside "ö" and there is no newline after it
        assert_eq!(tail("xöyz", 3), ("yz", true));
    }

    #[test]
    fn read_since_slices_by_offset_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("kast.log");
        std::fs::write(&p, "old run\n").unwrap();
        let session = std::fs::metadata(&p).unwrap().len();
        std::fs::write(&p, "old run\n\x1b[1mbold\x1b[0m\tx\r\nnew\n").unwrap();
        let c = read_since(&p, session, COPY_CAP).unwrap();
        assert_eq!(c, Clip { text: "bold  x\nnew\n".into(), lines: 2, truncated: false });
        // Offset beyond the end (log rotated): everything
        assert_eq!(read_since(&p, 10_000, COPY_CAP).unwrap().lines, 3);
        // Over the cap: the tail, starting on a whole line, with a note
        let c = read_since(&p, 0, 6).unwrap();
        assert!(c.truncated);
        assert_eq!(c.text, format!("[kast: log truncated, full file at {}]\nnew\n", p.display()));
    }

    #[test]
    fn cli_log_path_falls_back_to_root_log() {
        let own = Path::new("/home/p/.local/state/kast.log");
        assert_eq!(cli_log_path(false, own, 0, true), PathBuf::from(SYSTEM_LOG));
        assert_eq!(cli_log_path(false, own, 10, true), own);
        assert_eq!(cli_log_path(false, own, 0, false), own);
        assert_eq!(cli_log_path(true, Path::new(SYSTEM_LOG), 0, true), PathBuf::from(SYSTEM_LOG));
    }
}
