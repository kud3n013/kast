//! Proton Drive flows on top of `proton`: `kast drive …`, and (later in this file) uploading
//! backups, restoring from Drive and retention.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::ctx::ctx;
use crate::proton::{self, Cli, Install, Problem, Status};
use crate::settings;

/// Exit status of a command that needs Proton Drive when it isn't usable (not installed,
/// signed out, keyring locked, offline). Documented in the README.
pub const EXIT_NOT_READY: u8 = 3;

/// "Proton Drive isn't ready": `main` exits with `EXIT_NOT_READY` on it.
#[derive(Debug)]
pub struct NotReady(pub Problem);

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Problem::NotInstalled => write!(f, "proton-drive is not installed (see `kast drive status`)"),
            Problem::Keyring(m) => {
                write!(f, "The keyring holding the Proton Drive session is locked or unavailable ({m}). Unlock it, or switch to pass: kast drive store pass")
            }
            p => write!(f, "{p}"),
        }
    }
}

impl std::error::Error for NotReady {}

/// The hostname as a Drive folder name: `[A-Za-z0-9._-]`, everything else `_`.
pub fn sanitize_host(h: &str) -> String {
    let s: String = h.trim().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() || s == ".." { "unknown-host".into() } else { s }
}

pub fn host_name() -> String {
    sanitize_host(&nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default())
}

/// Find the CLI and make sure it can be used, without contacting Drive.
pub fn installed_cli() -> Result<Cli, Problem> {
    match proton::detect() {
        (Install::Found { compat: proton::Compat::TooOld, version, .. }, _) => {
            Err(Problem::Other(format!("proton-drive {version} is too old; kast needs {} or newer", proton::PROTON_MIN)))
        }
        (Install::Broken { problem, .. }, _) => Err(problem),
        (i, Some(cli)) if i.usable() => Ok(cli),
        _ => Err(Problem::NotInstalled),
    }
}

/// The CLI, signed in (checked now, not from a cache).
#[allow(dead_code)] // used by the upload in the next commit
pub fn ready_cli() -> Result<Cli, NotReady> {
    let cli = installed_cli().map_err(NotReady)?;
    proton::forget_status();
    match proton::cached_status(&cli) {
        Status::SignedIn => Ok(cli),
        s => Err(NotReady(s.problem().unwrap_or(Problem::SignedOut))),
    }
}

/// Short state for lists: "signed in", "not signed in", …
pub fn status_word(s: &Status) -> &'static str {
    match s {
        Status::SignedIn => "signed in",
        Status::SignedOut => "not signed in",
        Status::KeyringUnavailable(_) => "keyring locked",
        Status::Offline(_) => "offline",
        Status::NotAllowed(_) => "not available for this account",
        Status::Other(_) => "error",
    }
}

/// What a state means and what to do about it (details pane, `kast drive status`).
pub fn explain(s: &Status, pass_ready: bool) -> Vec<String> {
    match s {
        Status::SignedIn => vec!["Signed in. kast can back up to and restore from Proton Drive.".into()],
        Status::SignedOut => {
            vec!["There's no Proton Drive session on this computer (or it expired). Sign in through your browser; kast never sees your password.".into()]
        }
        Status::KeyringUnavailable(m) => {
            let mut v = vec![
                "proton-drive keeps its session in your desktop keyring (KWallet or GNOME Keyring), and it can't be reached: it may be locked, \
                 or this is a TTY or SSH session without one."
                    .into(),
                format!("proton-drive said: {m}"),
                "Log in to your desktop (which unlocks the keyring) and re-check.".into(),
            ];
            if pass_ready {
                v.push("Or keep the session in pass (your password store is set up): switching means signing in again.".into());
            }
            v
        }
        Status::Offline(m) => vec!["Proton Drive can't be reached. Check the network and re-check.".into(), format!("proton-drive said: {m}")],
        Status::NotAllowed(m) => {
            vec!["Proton offers the Drive CLI for some plans only (Proton for Business); this account can't use it.".into(), format!("proton-drive said: {m}")]
        }
        Status::Other(m) => vec![format!("proton-drive said: {m}")],
    }
}

/// `pass` is installed and initialised for the user.
pub fn pass_ready() -> bool {
    crate::ctx::have("pass") && ctx().target_home.join(".password-store/.gpg-id").is_file()
}

// --- kast drive -------------------------------------------------------------------------------

pub fn cmd_status(json_out: bool) -> Result<()> {
    let (install, cli) = proton::detect();
    let status = cli.as_ref().filter(|_| install.usable()).map(|c| c.status());
    let s = settings::load();
    let host = host_name();
    let folder = proton::join(&s.folder(), &[&host]);
    if json_out {
        let (path, version, compat) = match &install {
            Install::Found { path, version, compat } => {
                (Some(path.display().to_string()), Some(version.to_string()), Some(format!("{compat:?}").to_lowercase()))
            }
            Install::Broken { path, .. } => (Some(path.display().to_string()), None, None),
            Install::Missing => (None, None, None),
        };
        let v = json!({
            "installed": !matches!(install, Install::Missing),
            "path": path,
            "version": version,
            "compat": compat,
            "problem": match &install { Install::Broken { problem, .. } => Some(problem.to_string()), _ => None },
            "status": status.as_ref().map(Status::key),
            "detail": status.as_ref().and_then(Status::detail),
            "folder": folder,
            "keep": s.keep(),
            "credentials_store": s.store().unwrap_or_else(|| "keychain".into()),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("proton-drive: {}", install.describe());
        if matches!(install, Install::Missing) {
            print_install_help();
        }
        if let Some(st) = &status {
            println!("Status:       {}", status_word(st));
            if *st != Status::SignedIn {
                for l in explain(st, pass_ready()) {
                    println!("              {l}");
                }
            }
        }
        println!("Backups go to: {folder}");
        println!("Keep:          {} per backup type", s.keep());
        println!("Credentials:   {}", if s.store().as_deref() == Some("pass") { "pass" } else { "keychain (Secret Service / KWallet)" });
    }
    match status {
        Some(Status::SignedIn) => Ok(()),
        Some(st) => Err(NotReady(st.problem().unwrap_or(Problem::SignedOut)).into()),
        None => Err(NotReady(Problem::NotInstalled).into()),
    }
}

pub fn print_install_help() {
    let cpu = proton::cpu_build(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default());
    if proton::package_available() {
        println!("Install it with: sudo pacman -S {}", proton::PROTON_PKG);
        return;
    }
    println!("To install it:");
    println!("  1. Download it from {}", proton::DOWNLOAD_PAGE);
    println!("     Pick {cpu} (this CPU {} AVX2)", if cpu == "linux/x64" { "has" } else { "lacks" });
    println!("  2. chmod +x proton-drive");
    println!("  3. Move it to ~/.local/bin (or anywhere on your PATH)");
}

pub fn cmd_login() -> Result<()> {
    let cli = installed_cli().map_err(NotReady)?;
    let mut login = cli.start_login().map_err(NotReady)?;
    let url = login.url(Duration::from_secs(60)).map_err(|p| anyhow::anyhow!("{p}"))?;
    println!("Sign in to Proton Drive: open this link in a browser (it can be on another device, e.g. your phone):");
    println!();
    println!("{}", url.reveal());
    println!();
    if let Some(qr) = crate::qr::ansi(url.reveal()) {
        let width = qr.lines().next().map_or(0, |l| l.chars().count().saturating_sub(16));
        let cols = terminal_columns();
        if cols == 0 || width <= cols {
            print!("{qr}");
            println!();
        } else {
            println!("(The terminal is too narrow for the QR code; widen it, or open the link.)");
        }
    }
    println!("Waiting for you to sign in (up to 10 minutes; Ctrl+C cancels)…");
    let _ = std::io::stdout().flush();
    match login.finish(proton::LOGIN_TIMEOUT + Duration::from_secs(30)) {
        Ok(()) => {
            proton::forget_status();
            match cli.status() {
                Status::SignedIn => println!("Signed in to Proton Drive."),
                s => println!("Sign-in finished, but the check says: {} ({})", status_word(&s), s.detail().unwrap_or("")),
            }
            Ok(())
        }
        Err(p) => anyhow::bail!("Sign-in failed: {p}"),
    }
}

fn terminal_columns() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ only fills the struct we pass
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 { ws.ws_col as usize } else { 0 }
}

pub fn cmd_logout() -> Result<()> {
    let cli = installed_cli().map_err(NotReady)?;
    cli.logout().map_err(|p| anyhow::anyhow!("Sign-out failed: {p}"))?;
    proton::forget_status();
    println!("Signed out of Proton Drive; its session and local caches on this computer are gone.");
    Ok(())
}

pub fn cmd_store(store: &str) -> Result<()> {
    if store == "pass" && !pass_ready() {
        anyhow::bail!("pass isn't set up for {} (no ~/.password-store/.gpg-id); run `pass init <gpg-id>` first", ctx().target_user);
    }
    settings::update(&["proton", "credentials_store"], Some(store.into()))?;
    println!("Proton Drive sessions are now kept in {}.", if store == "pass" { "pass" } else { "the desktop keyring" });
    println!("Sign in again for it to take effect: kast drive login");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names() {
        assert_eq!(sanitize_host("archbox"), "archbox");
        assert_eq!(sanitize_host("my box/../ü"), "my_box_..__");
        assert_eq!(sanitize_host("..."), "unknown-host");
        assert_eq!(sanitize_host(""), "unknown-host");
        assert_eq!(sanitize_host(".hidden."), "hidden");
        assert_eq!(sanitize_host("Host-1.lan"), "Host-1.lan");
    }

    #[test]
    fn explanations() {
        assert_eq!(status_word(&Status::KeyringUnavailable("x".into())), "keyring locked");
        assert!(explain(&Status::KeyringUnavailable("x".into()), true).iter().any(|l| l.contains("pass")));
        assert!(!explain(&Status::KeyringUnavailable("x".into()), false).iter().any(|l| l.contains("in pass")));
        assert!(NotReady(Problem::SignedOut).to_string().contains("kast drive login"));
    }
}
