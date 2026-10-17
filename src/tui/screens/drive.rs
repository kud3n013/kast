//! Proton Drive in the TUI: the source-list row and its details (shared by the import and
//! export screens), the dialogs for installing, the keyring and signing out, and the sign-in
//! window with its QR code.
//!
//! Nothing here runs `proton-drive`: the checks, the sign-in process and the sign-out are
//! `Command`s the runtime carries out off the UI thread.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::drive::{explain, status_word};
use crate::proton::{self, Compat, Install, Problem, SignInUrl, Status};
use crate::tui::app::{App, Command, Modal, ModalKind, ToastKind, is_ctrl_c};
use crate::tui::widgets::{block, popup, wrapped_rows};

/// What kast found out about Proton Drive (`Command::DriveCheck`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveInfo {
    pub install: Install,
    /// Only checked when the CLI is usable
    pub status: Option<Status>,
    /// pacman knows `proton::PROTON_PKG`
    pub package: bool,
    pub pass_ready: bool,
    /// `linux/x64` or `linux/x64-baseline`
    pub cpu: &'static str,
    /// The remote base folder and this machine's folder name in it
    pub folder: String,
    pub host: String,
    pub keep: u32,
    pub store: Option<String>,
}

impl DriveInfo {
    pub fn signed_in(&self) -> bool {
        self.status == Some(Status::SignedIn)
    }

    /// `/my-files/kast-backups/<host>`
    pub fn host_folder(&self) -> String {
        proton::join(&self.folder, &[&self.host])
    }
}

/// Gather `DriveInfo` (runs `proton-drive`: off the UI thread). `refresh` checks the sign-in
/// again instead of using this session's answer.
pub fn load(refresh: bool) -> DriveInfo {
    if refresh {
        proton::forget_status();
    }
    let (install, cli) = proton::detect();
    let status = cli.as_ref().filter(|_| install.usable()).map(proton::cached_status);
    let s = crate::settings::load();
    DriveInfo {
        package: matches!(install, Install::Missing) && proton::package_available(),
        install,
        status,
        pass_ready: crate::drive::pass_ready(),
        cpu: proton::cpu_build(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default()),
        folder: s.folder(),
        host: crate::drive::host_name(),
        keep: s.keep(),
        store: s.store(),
    }
}

/// The sign-in window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    /// Set once proton-drive printed it
    pub url: Option<SignInUrl>,
    /// When the URL arrived (the CLI's 10 minutes start then)
    pub since_ms: u64,
    pub error: Option<String>,
    /// "Cancel sign-in?" is showing
    pub confirm_cancel: bool,
}

/// Start checking (the row shows "checking…" until the answer).
pub fn check(app: &mut App, refresh: bool) -> Command {
    app.drive_checking = true;
    Command::DriveCheck { refresh }
}

/// Check once per session when a screen with the source list opens.
pub fn check_if_unknown(app: &mut App) -> Option<Command> {
    (app.drive.is_none() && !app.drive_checking).then(|| check(app, false))
}

pub fn on_info(app: &mut App, info: DriveInfo) {
    app.drive_checking = false;
    app.drive = Some(Box::new(info));
}

/// The source-list row text, and whether Proton Drive can be picked as it is.
pub fn row(app: &App) -> (String, bool) {
    let state = match (&app.drive, app.drive_checking) {
        (_, true) | (None, false) => "checking…".to_string(),
        (Some(d), _) => match (&d.install, &d.status) {
            (Install::Missing, _) => "not installed — Enter to install".into(),
            (Install::Found { compat: Compat::TooOld, .. }, _) => "update needed — see details".into(),
            (Install::Broken { .. }, _) => "doesn't run — see details".into(),
            (_, Some(Status::SignedIn)) => return ("Proton Drive · signed in".into(), true),
            (_, Some(Status::SignedOut)) => "not signed in — Enter to sign in".into(),
            (_, Some(Status::KeyringUnavailable(_))) => "keyring locked — see details".into(),
            (_, Some(Status::Offline(_))) => "offline".into(),
            (_, Some(Status::NotAllowed(_))) => "not available for this account".into(),
            (_, Some(Status::Other(_)) | None) => "error — see details".into(),
        },
    };
    (format!("Proton Drive · {state}"), false)
}

/// The details pane while the Proton Drive row is highlighted.
pub fn details(app: &App, upload: bool) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let Some(d) = app.drive.as_deref().filter(|_| !app.drive_checking) else {
        return vec![Line::raw("Checking for proton-drive and whether it's signed in…")];
    };
    let mut v: Vec<Line> = Vec::new();
    let what = if upload {
        format!("Backs up to {} on Proton Drive, with proton-drive (Proton's own command-line tool) running as you.", d.host_folder())
    } else {
        format!("Restores backups from {} on Proton Drive, this machine's or another's.", d.folder)
    };
    v.push(Line::raw(what));
    v.push(Line::raw(""));
    match &d.install {
        Install::Missing => {
            v.push(Line::styled("proton-drive is not installed.", theme.warn()));
            if d.package {
                v.push(Line::raw(format!("Enter installs it ({}, from kud3n's repository).", proton::PROTON_PKG)));
            } else {
                v.push(Line::raw(format!("Enter shows how to install it (download it from Proton; this CPU needs {}).", d.cpu)));
            }
        }
        Install::Broken { problem, .. } => {
            v.push(Line::styled(d.install.describe(), theme.error()));
            if *problem == Problem::NeedsBaseline {
                v.push(Line::raw(format!("Download the linux/x64-baseline build from {}.", proton::DOWNLOAD_PAGE)));
            }
        }
        Install::Found { compat, .. } => {
            let style = if *compat == Compat::Ok { theme.dim() } else { theme.warn() };
            v.push(Line::styled(d.install.describe(), style));
            if let Some(s) = &d.status {
                v.push(Line::from(vec![Span::raw("Status: "), Span::styled(status_word(s), if *s == Status::SignedIn { theme.ok() } else { theme.warn() })]));
                for l in explain(s, d.pass_ready) {
                    if *s != Status::SignedIn {
                        v.push(Line::raw(l));
                    }
                }
            }
        }
    }
    v.push(Line::raw(""));
    let store = if d.store.as_deref() == Some("pass") { "pass" } else { "desktop keyring" };
    v.push(Line::styled(format!("Keeps {} backups per type and machine · session in the {store}", d.keep), theme.dim()));
    let mut keys = vec!["r re-check"];
    if d.signed_in() {
        keys.push("L sign out");
    }
    v.push(Line::styled(keys.join(" · "), theme.dim()));
    v
}

/// Enter on the Proton Drive row when it can't be picked: whatever gets it there.
pub fn enter(app: &mut App) -> Option<Command> {
    let Some(d) = app.drive.as_deref().cloned().filter(|_| !app.drive_checking) else {
        app.toast(ToastKind::Info, "Still checking Proton Drive…");
        return None;
    };
    let modal =
        |kind, title: &str, text: String, details: Vec<String>, yes, no| Modal { kind, title: title.into(), text, details, yes, no, yes_selected: true };
    match (&d.install, &d.status) {
        (Install::Missing, _) if d.package => {
            app.modal = Some(modal(
                ModalKind::DriveInstall,
                "Proton Drive",
                format!("Install {}?", proton::PROTON_PKG),
                vec![
                    "Proton's command-line tool for Drive, from kud3n's repository.".into(),
                    "Recorded like any package install (kast undo can remove it).".into(),
                ],
                "Install",
                "Cancel",
            ));
        }
        (Install::Missing, _) => {
            app.modal = Some(modal(
                ModalKind::DriveRecheck,
                "Install proton-drive",
                "proton-drive isn't installed. To install it:".into(),
                vec![
                    format!("1. Download it from {}", proton::DOWNLOAD_PAGE),
                    format!("   Pick {} (this CPU {} AVX2)", d.cpu, if d.cpu == "linux/x64" { "has" } else { "lacks" }),
                    "2. chmod +x proton-drive".into(),
                    "3. Move it to ~/.local/bin".into(),
                    "Then press r to re-check.".into(),
                ],
                "Re-check (r)",
                "Close",
            ));
        }
        (Install::Broken { .. } | Install::Found { compat: Compat::TooOld, .. }, _) => {
            let mut details = vec![d.install.describe()];
            details.push(format!("Get the current version from {} and re-check.", proton::DOWNLOAD_PAGE));
            app.modal = Some(modal(ModalKind::DriveRecheck, "Proton Drive", "proton-drive can't be used as it is.".into(), details, "Re-check (r)", "Close"));
        }
        (_, Some(Status::SignedOut)) => return start_sign_in(app),
        (_, Some(s @ Status::KeyringUnavailable(_))) if d.pass_ready && d.store.as_deref() != Some("pass") => {
            app.modal = Some(modal(
                ModalKind::DriveUsePass,
                "Keyring locked",
                "The keyring that holds the Proton Drive session can't be used.".into(),
                explain(s, true),
                "Use pass",
                "Close",
            ));
        }
        (_, Some(s)) => {
            app.modal = Some(modal(
                ModalKind::DriveRecheck,
                "Proton Drive",
                format!("Proton Drive: {}", status_word(s)),
                explain(s, d.pass_ready),
                "Re-check (r)",
                "Close",
            ));
        }
        (_, None) => return Some(check(app, true)),
    }
    None
}

/// `r` / `L` while the Proton Drive row is highlighted. None when the key isn't one of these.
pub fn row_key(app: &mut App, k: &KeyEvent) -> Option<Option<Command>> {
    match k.code {
        KeyCode::Char('r') => Some(Some(check(app, true))),
        KeyCode::Char('L') if app.drive.as_deref().is_some_and(DriveInfo::signed_in) => {
            app.modal = Some(Modal {
                kind: ModalKind::DriveSignOut,
                title: "Sign out".into(),
                text: "Sign out of Proton Drive?".into(),
                details: vec!["This removes the Proton Drive session and its local caches on this computer.".into()],
                yes: "Sign out",
                no: "Cancel",
                yes_selected: false,
            });
            Some(None)
        }
        _ => None,
    }
}

pub fn start_sign_in(app: &mut App) -> Option<Command> {
    app.signin = Some(SignIn { url: None, since_ms: app.now_ms, error: None, confirm_cancel: false });
    Some(Command::DriveLogin)
}

pub fn on_url(app: &mut App, url: SignInUrl) -> Option<Command> {
    match &mut app.signin {
        Some(s) => {
            s.url = Some(url);
            s.since_ms = app.now_ms;
            None
        }
        // Closed before the address came: stop the process
        None => Some(Command::DriveLoginCancel),
    }
}

pub fn on_done(app: &mut App, result: Result<(), String>) -> Option<Command> {
    let s = app.signin.as_mut()?;
    match result {
        Ok(()) => {
            app.signin = None;
            app.toast(ToastKind::Ok, "Signed in to Proton Drive");
            Some(check(app, true))
        }
        Err(e) => {
            s.error = Some(e);
            s.confirm_cancel = false;
            None
        }
    }
}

pub fn on_logged_out(app: &mut App, result: Result<(), String>) -> Option<Command> {
    match result {
        Ok(()) => app.toast(ToastKind::Ok, "Signed out of Proton Drive"),
        Err(e) => app.toast(ToastKind::Error, format!("Sign-out failed: {e}")),
    }
    Some(check(app, true))
}

pub fn on_store_set(app: &mut App, result: Result<(), String>) -> Option<Command> {
    match result {
        Ok(()) => app.toast(ToastKind::Ok, "Proton Drive now keeps its session in pass: sign in again"),
        Err(e) => app.toast(ToastKind::Error, e),
    }
    Some(check(app, true))
}

/// Keys while the sign-in window is open (it takes them all).
pub fn signin_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let s = app.signin.as_mut()?;
    if s.confirm_cancel {
        return match k.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                app.signin = None;
                Some(Command::DriveLoginCancel)
            }
            _ => {
                s.confirm_cancel = false;
                None
            }
        };
    }
    if s.error.is_some() {
        return match k.code {
            KeyCode::Enter | KeyCode::Char('r') => start_sign_in(app),
            _ if is_ctrl_c(&k) || matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) => {
                app.signin = None;
                None
            }
            _ => None,
        };
    }
    match k.code {
        KeyCode::Char('y') => s.url.clone().map(Command::CopySignIn),
        KeyCode::Char('o') => s.url.clone().map(Command::OpenSignIn),
        KeyCode::Esc | KeyCode::Char('q') => {
            s.confirm_cancel = true;
            None
        }
        _ if is_ctrl_c(&k) => {
            s.confirm_cancel = true;
            None
        }
        _ => None,
    }
}

/// `m:ss` left of the CLI's ten minutes.
pub fn countdown(since_ms: u64, now_ms: u64) -> String {
    let left = proton::LOGIN_TIMEOUT.as_secs().saturating_sub(now_ms.saturating_sub(since_ms) / 1000);
    format!("{}:{:02}", left / 60, left % 60)
}

pub fn signin_view(f: &mut Frame, area: Rect, app: &App, s: &SignIn) {
    let theme = &app.theme;
    let mut lines: Vec<Line> = Vec::new();
    let mut qr: Option<Vec<String>> = None;
    let (mut w, title) = (64u16, " Sign in to Proton Drive ");
    if let Some(e) = &s.error {
        lines.push(Line::styled("Sign-in didn't complete.", theme.error()));
        lines.push(Line::raw(e.clone()));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter try again · Esc close", theme.dim()));
    } else if s.confirm_cancel {
        lines.push(Line::styled("Cancel sign-in?", theme.bold()));
        lines.push(Line::raw("The sign-in link stops working."));
        lines.push(Line::raw(""));
        lines.push(Line::styled("y cancel sign-in · any other key keeps waiting", theme.dim()));
    } else if let Some(url) = &s.url {
        let text = url.reveal();
        lines.push(Line::raw("Open this link to sign in. You can also sign in on your phone: scan the code."));
        lines.push(Line::raw(""));
        lines.push(Line::styled(text.to_string(), theme.accent()));
        lines.push(Line::raw(""));
        let code = crate::qr::half_blocks(text).unwrap_or_default();
        let (qw, qh) = (code.first().map_or(0, |l| l.chars().count()) as u16, code.len() as u16);
        w = w.max(qw + 4);
        let inner_w = (w.min(area.width).saturating_sub(2)).max(1) as usize;
        let text_rows: u16 = lines.iter().map(|l| wrapped_rows(&l.to_string(), inner_w) as u16).sum::<u16>() + 3;
        if !code.is_empty() && qw + 4 <= area.width && text_rows + qh + 2 <= area.height {
            qr = Some(code);
        } else {
            lines.push(Line::styled(
                format!("Terminal too small for the QR code (it needs {}×{}): copy or open the link instead.", qw + 4, text_rows + qh + 2),
                theme.warn(),
            ));
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::raw("Waiting for you to sign in… "),
            Span::styled(format!("{} left", countdown(s.since_ms, app.now_ms)), theme.bold()),
        ]));
        lines.push(Line::styled("y copy link · o open in browser · Esc cancel", theme.dim()));
    } else {
        lines.push(Line::raw(format!("{} Starting proton-drive…", app.theme.sym.spinner[app.frame % app.theme.sym.spinner.len()])));
        lines.push(Line::styled("Esc cancel", theme.dim()));
    }
    let w = w.min(area.width);
    let inner_w = w.saturating_sub(2).max(1) as usize;
    let text_rows: u16 = lines.iter().map(|l| wrapped_rows(&l.to_string(), inner_w).max(1) as u16).sum();
    let qh = qr.as_ref().map_or(0, |q| q.len() as u16 + 1);
    let r = popup(f, area, w, text_rows + qh + 2);
    let blk = block(theme, title);
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    match qr {
        Some(code) => {
            // Text above the code (everything up to the URL's blank line), the rest below
            let split = 4.min(lines.len());
            let below: Vec<Line> = lines.split_off(split);
            let above_rows: u16 = lines.iter().map(|l| wrapped_rows(&l.to_string(), inner_w).max(1) as u16).sum();
            let [a, q, b] = Layout::vertical([Constraint::Length(above_rows), Constraint::Length(code.len() as u16 + 1), Constraint::Min(1)]).areas(inner);
            f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), a);
            // Always black on white, whatever the theme: an inverted code often won't scan
            let style = Style::new().fg(Color::Black).bg(Color::White);
            let qw = code[0].chars().count() as u16;
            let [qa] = Layout::horizontal([Constraint::Length(qw)]).flex(ratatui::layout::Flex::Center).areas(q);
            let code_lines: Vec<Line> = code.into_iter().map(|l| Line::styled(l, style)).collect();
            f.render_widget(Paragraph::new(Text::from(code_lines)), qa);
            f.render_widget(Paragraph::new(Text::from(below)).wrap(Wrap { trim: false }), b);
        }
        None => f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), inner),
    }
}

pub fn signin_hints(s: &SignIn) -> &'static [(&'static str, &'static str)] {
    if s.error.is_some() {
        &[("Enter", "try again"), ("Esc", "close")]
    } else if s.confirm_cancel {
        &[("y", "cancel sign-in"), ("n", "keep waiting")]
    } else if s.url.is_some() {
        &[("y", "copy link"), ("o", "open"), ("Esc", "cancel")]
    } else {
        &[("Esc", "cancel")]
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::proton::Version;
    use std::path::PathBuf;

    pub fn info(status: Option<Status>) -> DriveInfo {
        DriveInfo {
            install: Install::Found { path: PathBuf::from("/home/p3ter/.local/bin/proton-drive"), version: Version(0, 9, 0), compat: Compat::Ok },
            status,
            package: false,
            pass_ready: false,
            cpu: "linux/x64",
            folder: crate::settings::DEFAULT_FOLDER.into(),
            host: "archbox".into(),
            keep: 5,
            store: None,
        }
    }

    pub fn missing(package: bool) -> DriveInfo {
        DriveInfo { install: Install::Missing, status: None, package, ..info(None) }
    }

    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, Screen, update};
    use crate::tui::event::Loaded;
    use ratatui::crossterm::event::KeyCode;

    /// The restore source list with the Proton Drive row highlighted.
    fn on_proton_row(w: u16, h: u16, d: DriveInfo) -> App {
        let mut a = app(w, h);
        a.drive = Some(Box::new(d));
        update(&mut a, key(KeyCode::Char('6')));
        update(&mut a, key(KeyCode::Down));
        a
    }

    #[test]
    fn snapshot_source_states() {
        let keyring = Status::KeyringUnavailable("Failed to load session from secrets: Cannot autolaunch D-Bus without X11 $DISPLAY".into());
        let states = [
            ("signed_in", info(Some(Status::SignedIn))),
            ("signed_out", info(Some(Status::SignedOut))),
            ("keyring", DriveInfo { pass_ready: true, ..info(Some(keyring)) }),
            ("offline", info(Some(Status::Offline("ConnectionError: fetch failed".into())))),
            ("not_installed", missing(true)),
        ];
        for (name, d) in states {
            let a = on_proton_row(80, 24, d.clone());
            insta::assert_snapshot!(format!("drive_source_{name}_80x24"), render(&a));
        }
        let a = on_proton_row(140, 40, info(Some(Status::SignedIn)));
        insta::assert_snapshot!("drive_source_signed_in_140x40", render(&a));
    }

    #[test]
    fn dialogs() {
        // Installable from the repository: a job
        let mut a = on_proton_row(80, 24, missing(true));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::DriveInstall));
        insta::assert_snapshot!("drive_install_80x24", render(&a));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(crate::tui::job::JobKind::InstallProton))));
        // Not in a repository: the manual steps
        let mut a = on_proton_row(80, 24, DriveInfo { cpu: "linux/x64-baseline", ..missing(false) });
        update(&mut a, key(KeyCode::Enter));
        insta::assert_snapshot!("drive_manual_install_80x24", render(&a));
        // Keyring locked with pass set up: offer pass
        let mut a = on_proton_row(80, 24, DriveInfo { pass_ready: true, ..info(Some(Status::KeyringUnavailable("x".into()))) });
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::DriveSetStore(s)) if s == "pass"));
        // Signed in: L asks before signing out
        let mut a = on_proton_row(80, 24, info(Some(Status::SignedIn)));
        assert!(update(&mut a, key(KeyCode::Char('L'))).is_none());
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::DriveSignOut));
        assert!(a.modal.as_ref().unwrap().details[0].contains("local caches"));
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::DriveLogout)));
        assert!(matches!(update(&mut a, Event::Loaded(Loaded::DriveLoggedOut(Ok(())))), Some(Command::DriveCheck { refresh: true })));
    }

    fn url() -> SignInUrl {
        crate::proton::parse_sign_in(r#"{"signInUrl":"https://account.proton.me/sso/drive-cli#selector=Zm9vYmFy&key=c2VjcmV0a2V5bWF0ZXJpYWw"}"#)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn sign_in_window() {
        for (w, h) in [(140, 40), (80, 24)] {
            let mut a = on_proton_row(w, h, info(Some(Status::SignedOut)));
            assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DriveLogin)));
            assert!(a.animating());
            if w == 80 {
                insta::assert_snapshot!("drive_signin_starting_80x24", render(&a));
            }
            a.now_ms += 1000;
            assert!(update(&mut a, Event::Loaded(Loaded::SignInUrl(url()))).is_none());
            a.now_ms += 75_000;
            let out = render(&a);
            assert!(out.contains("8:45 left"));
            if w == 80 {
                assert!(out.contains("Terminal too small for the QR code"));
            } else {
                assert!(out.contains("█") && !out.contains("too small"));
            }
            insta::assert_snapshot!(format!("drive_signin_{w}x{h}"), out);
            // y and o hand the URL to the runtime
            assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::CopySignIn(_))));
            assert!(matches!(update(&mut a, key(KeyCode::Char('o'))), Some(Command::OpenSignIn(_))));
        }
        // Esc asks, then y kills it
        let mut a = on_proton_row(80, 24, info(Some(Status::SignedOut)));
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, Event::Loaded(Loaded::SignInUrl(url())));
        assert!(update(&mut a, key(KeyCode::Esc)).is_none());
        assert!(render(&a).contains("Cancel sign-in?"));
        assert!(update(&mut a, key(KeyCode::Char('n'))).is_none());
        assert!(a.signin.as_ref().is_some_and(|s| !s.confirm_cancel));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::DriveLoginCancel)));
        assert!(a.signin.is_none());
        // A late "done" for a closed window is ignored; a late address stops the process
        assert!(update(&mut a, Event::Loaded(Loaded::SignInDone(Ok(())))).is_none());
        assert!(matches!(update(&mut a, Event::Loaded(Loaded::SignInUrl(url()))), Some(Command::DriveLoginCancel)));
        // Failure: shown, Enter tries again
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, Event::Loaded(Loaded::SignInDone(Err("Authentication timed out".into()))));
        insta::assert_snapshot!("drive_signin_failed_80x24", render(&a));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DriveLogin)));
        // Success: toast, check again, back on the source list
        let cmd = update(&mut a, Event::Loaded(Loaded::SignInDone(Ok(()))));
        assert!(matches!(cmd, Some(Command::DriveCheck { refresh: true })));
        assert!(a.signin.is_none() && matches!(a.screen, Screen::Import(_)));
        assert_eq!(a.toast.as_ref().unwrap().text, "Signed in to Proton Drive");
    }

    #[test]
    fn sign_in_url_never_shows_in_debug_output() {
        let mut a = app(80, 24);
        update(&mut a, Event::Loaded(Loaded::Drive(Box::new(info(Some(Status::SignedOut))))));
        let ev = Event::Loaded(Loaded::SignInUrl(url()));
        let cmd = Command::CopySignIn(url());
        for text in [format!("{ev:?}"), format!("{cmd:?}")] {
            assert!(!text.contains("c2VjcmV0") && text.contains(proton::URL_HIDDEN), "{text}");
        }
    }

    #[test]
    fn countdown_from_ten_minutes() {
        assert_eq!(countdown(1000, 1000), "10:00");
        assert_eq!(countdown(0, 61_500), "8:59");
        assert_eq!(countdown(0, 700_000), "0:00");
    }

    #[test]
    fn rows_by_state() {
        let mut a = crate::tui::app::tests::app(80, 24);
        a.drive = None;
        assert_eq!(row(&a), ("Proton Drive · checking…".into(), false));
        let cases = [
            (missing(false), "Proton Drive · not installed — Enter to install", false),
            (info(Some(Status::SignedIn)), "Proton Drive · signed in", true),
            (info(Some(Status::SignedOut)), "Proton Drive · not signed in — Enter to sign in", false),
            (info(Some(Status::KeyringUnavailable("x".into()))), "Proton Drive · keyring locked — see details", false),
            (info(Some(Status::Offline("x".into()))), "Proton Drive · offline", false),
        ];
        for (i, text, ok) in cases {
            on_info(&mut a, i);
            assert_eq!(row(&a), (text.to_string(), ok));
        }
    }
}
