//! The side-effecting half of `logio`: finding out where we run and starting programs.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{
    COPY_CAP, Launch, Mux, OSC52_NOTE, OSC52_TEXT_CAP, Report, SYSTEM_LOG, Session, cli_log_path, desktop_env, env_value, is_remote_shell, mux, opener, osc52,
    read_since, show_item, tail,
};
use crate::ctx::{self, UserEnv, ctx};
use crate::ui::sanitize_log;

/// How long a launcher (xdg-open, gdbus, wl-copy...) may take before we stop waiting for it.
const LAUNCH_WAIT: Duration = Duration::from_secs(3);

/// Whether `prog` (a name on PATH or a path) can be run.
pub fn have(prog: &str) -> bool {
    if prog.contains('/') { Path::new(prog).is_file() } else { ctx::have(prog) }
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Names (`comm`) of our ancestor processes, nearest first.
pub fn ancestors() -> Vec<String> {
    let mut out = Vec::new();
    let mut pid = std::os::unix::process::parent_id();
    while pid > 1 && out.len() < 32 {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { break };
        // "pid (comm) state ppid ...": comm may itself contain spaces and parentheses
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { break };
        out.push(stat[open + 1..close].to_string());
        pid = stat[close + 1..].split_whitespace().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    }
    out
}

/// No controlling terminal for `cmd`: neither it nor sudo (with `use_pty`) can touch ours.
fn detach(cmd: &mut std::process::Command) {
    // SAFETY: setsid is async-signal-safe and touches no memory of ours
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

/// The user's session for desktop programs: `ctx::user_env`, gaps in the display variables
/// filled from their systemd user manager, plus the desktop variables (see `DESKTOP_KEYS`).
fn desktop_session() -> (UserEnv, Vec<(String, String)>) {
    let mut u = ctx::user_env();
    let argv: Vec<String> = ["systemctl", "--user", "show-environment"].map(String::from).to_vec();
    let mut cmd = Launch::as_user(ctx().is_root, &u, &[], &argv).command();
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    detach(&mut cmd);
    let systemd = cmd.output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    for (field, key) in [(&mut u.wayland, "WAYLAND_DISPLAY"), (&mut u.display, "DISPLAY"), (&mut u.xauthority, "XAUTHORITY")] {
        if field.is_empty() {
            *field = env_value(&systemd, key).unwrap_or_default().to_string();
        }
    }
    let extra = desktop_env(&env, &systemd);
    (u, extra)
}

pub fn session() -> Session {
    let remote = env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some() || ancestors().iter().any(|a| is_remote_shell(a));
    let (u, _) = desktop_session();
    Session { wayland: u.wayland, display: u.display, term: env("TERM").unwrap_or_default(), remote }
}

pub fn current_mux() -> Mux {
    mux(env("TMUX").is_some(), env("STY").is_some(), &env("TERM").unwrap_or_default(), &ancestors())
}

enum Waited {
    Exited(Option<i32>),
    StillRunning,
}

/// Start `argv` as the target user in its own session (no controlling terminal, so neither it
/// nor sudo can draw on ours), with `input` on stdin, and wait up to `LAUNCH_WAIT` for it.
fn launch(argv: &[String], input: Option<Vec<u8>>) -> io::Result<Waited> {
    let (user, desktop) = desktop_session();
    let mut cmd = Launch::as_user(ctx().is_root, &user, &desktop, argv).command();
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::null()).stderr(Stdio::null());
    detach(&mut cmd);
    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        // On its own thread: a program that stops reading must not hang us
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(Waited::Exited(st.code()));
        }
        if Instant::now() >= deadline {
            // A file manager or viewer that stays in the foreground: leave it, reap it later
            std::thread::spawn(move || child.wait());
            return Ok(Waited::StillRunning);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn failure(prog: &str, result: io::Result<Waited>, hint: &str) -> Report {
    match result {
        Ok(Waited::Exited(Some(code))) => Report::err(format!("{prog} failed (exit {code}); {hint}")),
        Ok(_) => Report::err(format!("{prog} was killed; {hint}")),
        Err(e) => Report::err(format!("Could not run {prog}: {e}")),
    }
}

fn launched(r: &io::Result<Waited>) -> bool {
    matches!(r, Ok(Waited::Exited(Some(0)) | Waited::StillRunning))
}

/// `o`: open the log with the desktop's default app. `hint` says how else to view it.
pub fn open_file(path: &Path, hint: &str) -> Report {
    if !session().gui() {
        return Report::err(format!("No graphical session; {hint}"));
    }
    let Some(argv) = opener(&path.display().to_string(), &have) else {
        return Report::err(format!("No xdg-open, gio or kde-open found; {hint}"));
    };
    let r = launch(&argv, None);
    match r {
        Ok(Waited::Exited(Some(0))) => Report::ok(format!("Opened the log ({})", argv[0])),
        // Some openers run the app in the foreground; one without a window never returns
        Ok(Waited::StillRunning) => Report::ok(format!("Opening the log ({} still running); if nothing appears, {hint}", argv[0])),
        r => failure(&argv[0], r, hint),
    }
}

/// Open a web address in the user's browser (the sign-in link).
pub fn open_url(url: &str, hint: &str) -> Report {
    if !session().gui() {
        return Report::err(format!("No graphical session; {hint}"));
    }
    let Some(argv) = opener(url, &have) else {
        return Report::err(format!("No xdg-open, gio or kde-open found; {hint}"));
    };
    match launch(&argv, None) {
        Ok(Waited::Exited(Some(0)) | Waited::StillRunning) => Report::ok("Opened the link in your browser"),
        r => failure(&argv[0], r, hint),
    }
}

/// `f`: show the log in the file manager (the file highlighted when it supports FileManager1).
pub fn show_in_folder(path: &Path, hint: &str) -> Report {
    if !session().gui() {
        return Report::err(format!("No graphical session; {hint}"));
    }
    let mut last = Report::err(format!("No gdbus, dbus-send or xdg-open found; {hint}"));
    for argv in show_item(path, &have) {
        let r = launch(&argv, None);
        // A gdbus call still waiting means the file manager is starting: don't open a second one
        if launched(&r) {
            return Report::ok(format!("Shown in the file manager ({})", argv[0]));
        }
        last = failure(&argv[0], r, hint);
    }
    last
}

/// Result of a copy: done by a clipboard program, or OSC 52 bytes the caller must write to
/// the terminal itself (in the app: between frames).
pub enum Copied {
    Done(Report),
    Osc52 { bytes: Vec<u8>, report: Report },
}

/// Put `text` on the clipboard with wl-copy, xclip or xsel as the user; failing those, hand
/// back an OSC 52 sequence. `what` names the text in the message ("412 lines"), `hint` how
/// else to see the log.
pub fn copy(text: &str, what: &str, hint: &str) -> Copied {
    let s = session();
    let tools = s.clipboard_tools(&have);
    for argv in &tools {
        // These fork to keep serving the selection: only the parent's exit is waited for
        if let Ok(Waited::Exited(Some(0))) = launch(argv, Some(text.as_bytes().to_vec())) {
            return Copied::Done(Report::ok(format!("Copied {what} ({})", argv[0])));
        }
    }
    if s.term == "linux" {
        return Copied::Done(Report::err(format!("The Linux console has no clipboard; {hint}")));
    }
    let note = format!("[kast: log truncated, full file at {}]", ctx().log_file.display());
    let (bytes, cut) = osc52(text, current_mux(), &note);
    let what = if cut { format!("the last {} lines", tail(text, OSC52_TEXT_CAP - note.len() - 1).0.lines().count()) } else { what.to_string() };
    let mut msg = format!("Copied {what} via terminal (OSC 52); {OSC52_NOTE}");
    if s.gui() && tools.is_empty() {
        msg.push_str("; install wl-clipboard (Wayland) or xclip (X11) for a reliable copy");
    }
    Copied::Osc52 { bytes, report: Report::ok(msg) }
}

/// When even writing to the terminal failed.
pub fn copy_failed(e: &io::Error, hint: &str) -> Report {
    Report::err(format!("Could not copy ({e}); install wl-clipboard (Wayland) or xclip (X11), or {hint}"))
}

/// `y`: the log from byte `from` on.
pub fn copy_log(path: &Path, from: u64, hint: &str) -> Copied {
    match read_since(path, from, COPY_CAP) {
        Ok(c) if c.lines == 0 => Copied::Done(Report::ok("Nothing logged yet")),
        Ok(c) => {
            let what = if c.truncated { format!("the last {} lines (over 256 KiB)", c.lines) } else { format!("{} lines", c.lines) };
            copy(&c.text, &what, hint)
        }
        Err(e) => Copied::Done(Report::err(format!("Could not read {}: {e}", path.display()))),
    }
}

// --- kast log ------------------------------------------------------------------------------

const CLI_HINT: &str = "view it with: kast log --tail 100";

fn print_report(r: Report) -> anyhow::Result<()> {
    if r.ok {
        println!("{}", r.text);
        Ok(())
    } else {
        anyhow::bail!("{}", r.text)
    }
}

pub fn cmd_log(tail_lines: Option<usize>, open: bool, copy_it: bool) -> anyhow::Result<()> {
    let c = ctx();
    let own_len = fs::metadata(&c.log_file).map(|m| m.len()).unwrap_or(0);
    let path = cli_log_path(c.is_root, &c.log_file, own_len, Path::new(SYSTEM_LOG).exists());
    if open {
        return print_report(open_file(&path, CLI_HINT));
    }
    if copy_it {
        // Outside the app there is no session start to go by: the whole file, capped
        return match copy_log(&path, 0, CLI_HINT) {
            Copied::Done(r) => print_report(r),
            Copied::Osc52 { bytes, report } => {
                let mut out = io::stdout();
                if !out.is_terminal() {
                    anyhow::bail!("No clipboard program and no terminal to copy through; install wl-clipboard (Wayland) or xclip (X11)");
                }
                out.write_all(&bytes).and_then(|_| out.flush()).map_err(|e| anyhow::anyhow!(copy_failed(&e, CLI_HINT).text))?;
                print_report(report)
            }
        };
    }
    if let Some(n) = tail_lines {
        let bytes = fs::read(&path).map_err(|e| anyhow::anyhow!("Could not read {}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        for l in &lines[lines.len().saturating_sub(n)..] {
            println!("{}", sanitize_log(l));
        }
        return Ok(());
    }
    println!("{}", path.display());
    Ok(())
}
