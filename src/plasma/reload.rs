// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! Restarting the Plasma shell around an apply. The fork's `--reload-kde` kills and starts
//! plasmashell *after* copying, but Plasma writes its in-memory state to
//! `plasma-org.kde.plasma.desktop-appletsrc` and `plasmashellrc` when it quits, which undoes
//! the apply. So the order here is: quit, apply, start.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `plasmashell 6.8.0` -> `6.8.0`
pub fn parse_plasma_version(out: &str) -> Option<String> {
    let v = out.split_whitespace().last()?;
    v.chars().next().filter(char::is_ascii_digit).map(|_| v.to_string())
}

/// `plasmashell --version`, run as the target user (under sudo root has no session for Qt) and
/// asked only once per run.
pub fn plasma_version() -> Option<String> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            let out = user_command("plasmashell", &["--version"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            out.status.success().then(|| parse_plasma_version(&String::from_utf8_lossy(&out.stdout))).flatten()
        })
        .clone()
}

pub fn major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tools {
    pub quit: &'static str,
    pub start: &'static str,
}

/// The programs for this Plasma: `kquitapp6` + `kstart` on Plasma 6, `kquitapp5` +
/// `kstart5` on Plasma 5. When the version is unknown, whichever exists (6 first).
pub fn detect_tools(version: Option<&str>, have: &dyn Fn(&str) -> bool) -> Option<Tools> {
    let six = Tools { quit: "kquitapp6", start: "kstart" };
    let five = Tools { quit: "kquitapp5", start: "kstart5" };
    let usable = |t: &Tools| have(t.quit) && have(t.start);
    match version.and_then(major) {
        Some(5) => usable(&five).then_some(five),
        Some(_) => usable(&six).then_some(six),
        None => [six, five].into_iter().find(|t| usable(t)),
    }
}

pub const WARNING: &str = "Plasma rewrites its settings when it quits, so files restored while it runs can be overwritten again. \
Restarting the shell around the apply avoids that; a full log out and in is the most reliable. KWin (window manager) settings, including KWin's own shortcuts, need a log out or a KWin reconfigure.";

fn user_command(program: &str, args: &[&str]) -> Command {
    let c = crate::ctx::ctx();
    if c.is_root {
        let mut all = vec![program];
        all.extend_from_slice(args);
        crate::ctx::run_as_user(&all)
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
}

fn running() -> bool {
    Command::new("pgrep").args(["-x", "plasmashell"]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Ask plasmashell to quit and wait (up to 15 s) until it has written its state and gone.
pub fn quit_shell(t: &Tools) -> Result<(), String> {
    if !running() {
        return Ok(());
    }
    let ok = user_command(t.quit, &["plasmashell"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !ok {
        return Err(format!("{} plasmashell failed", t.quit));
    }
    let start = Instant::now();
    while running() {
        if start.elapsed() > Duration::from_secs(15) {
            return Err("plasmashell did not quit within 15 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Start plasmashell again, detached.
pub fn start_shell(t: &Tools) -> Result<(), String> {
    user_command(t.start, &["plasmashell"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{} plasmashell: {e}", t.start))
}

const SHORTCUT_UNIT: &str = "plasma-kglobalaccel.service";

fn shortcut_daemon_running() -> bool {
    Command::new("pgrep").args(["-x", "kglobalacceld"]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn user_ok(program: &str, args: &[&str]) -> bool {
    user_command(program, args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Quit `kglobalacceld`, which owns the global shortcuts and rewrites `kglobalshortcutsrc`
/// from memory when it exits or syncs. Restoring that file while it runs leaves the old
/// shortcuts (KDE's defaults, e.g. Meta+L) in charge. Returns true when it was stopped and
/// must be started again with [`start_shortcut_daemon`].
pub fn stop_shortcut_daemon() -> Result<bool, String> {
    if !shortcut_daemon_running() {
        return Ok(false);
    }
    if !user_ok("systemctl", &["--user", "stop", SHORTCUT_UNIT]) {
        let have = |n: &str| crate::ctx::have(n);
        let quit = ["kquitapp6", "kquitapp5"].into_iter().find(|n| have(n)).ok_or("could not stop kglobalacceld")?;
        if !user_ok(quit, &["kglobalaccel"]) {
            return Err(format!("{quit} kglobalaccel failed"));
        }
    }
    let start = Instant::now();
    while shortcut_daemon_running() {
        if start.elapsed() > Duration::from_secs(10) {
            return Err("kglobalacceld did not quit within 10 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(true)
}

/// Start `kglobalacceld` again so it reads the restored `kglobalshortcutsrc`.
pub fn start_shortcut_daemon() -> Result<(), String> {
    if shortcut_daemon_running() || user_ok("systemctl", &["--user", "start", SHORTCUT_UNIT]) {
        return Ok(());
    }
    let bin = ["/usr/lib/kf6/kglobalacceld", "/usr/lib/kglobalacceld", "/usr/libexec/kglobalacceld"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .ok_or("could not start kglobalacceld; log out and in to load the shortcuts")?;
    user_command(bin, &[]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ()).map_err(|e| format!("{bin}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_plasma_version("plasmashell 6.8.0\n").as_deref(), Some("6.8.0"));
        assert_eq!(parse_plasma_version("plasmashell 5.27.11").as_deref(), Some("5.27.11"));
        assert_eq!(parse_plasma_version(""), None);
        assert_eq!(parse_plasma_version("plasmashell"), None);
        assert_eq!(major("6.8.0"), Some(6));
    }

    #[test]
    fn picks_tools_by_version_and_availability() {
        let all = |_: &str| true;
        assert_eq!(detect_tools(Some("6.8.0"), &all), Some(Tools { quit: "kquitapp6", start: "kstart" }));
        assert_eq!(detect_tools(Some("5.27.1"), &all), Some(Tools { quit: "kquitapp5", start: "kstart5" }));
        let only5 = |n: &str| n.ends_with('5');
        assert_eq!(detect_tools(Some("6.1"), &only5), None, "never mixes a Plasma 6 shell with Plasma 5 tools");
        assert_eq!(detect_tools(None, &only5), Some(Tools { quit: "kquitapp5", start: "kstart5" }));
        assert_eq!(detect_tools(None, &|_| false), None);
    }
}
