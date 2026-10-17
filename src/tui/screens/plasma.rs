//! KDE Plasma settings (menu `p`): the profile list, save, apply (category checkboxes and a
//! dry-run preview), export, import (with a preview), remove, packages a profile needs, and
//! the config-check view. Every operation is a [`PlasmaReq`] the runtime runs on a worker
//! thread ([`run`]), answered with a [`PlasmaResp`], so the UI never blocks.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::plasma::apply;
use crate::plasma::archive::{self, ExportOpts, ImportOpts};
use crate::plasma::check::{self, Row, Status};
use crate::plasma::config::{Category, Config};
use crate::plasma::deps::{self, Pacman};
use crate::plasma::fsops::human_size;
use crate::plasma::paths::{Roots, valid_profile_name};
use crate::plasma::profile::{self, Ambient, ProfileKind, SaveOpts, Summary};
use crate::plasma::reload;
use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind};
use crate::tui::job::JobKind;
use crate::tui::widgets::{Input, block};

// --- Requests and answers ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PlasmaReq {
    Refresh,
    Save { name: String, assets: bool, force: bool },
    Plan { name: String, only: Vec<Category> },
    Apply { name: String, only: Vec<Category>, restart: bool },
    Remove(String),
    Export { name: String, out: PathBuf, assets: bool, force: bool },
    ImportPreview { file: PathBuf, name: Option<String> },
    Import { file: PathBuf, name: Option<String>, force: bool },
    Check,
    Deps(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DepRow {
    pub kind: String,
    pub name: String,
    pub package: String,
    pub missing: bool,
}

/// Rows, installable packages, packages no repository has
pub type DepsData = (Vec<DepRow>, Vec<String>, Vec<String>);

#[derive(Debug)]
pub enum PlasmaResp {
    Profiles(Result<Vec<Summary>, String>),
    /// An operation ended. `install` lists missing packages worth offering to install.
    Done {
        ok: bool,
        lines: Vec<String>,
        install: Vec<String>,
    },
    Plan {
        name: String,
        result: Result<(String, bool), String>,
    },
    ImportPreview {
        file: PathBuf,
        name: Option<String>,
        result: Result<(String, bool), String>,
    },
    Check(Result<Vec<Row>, String>),
    Deps {
        name: String,
        result: Result<DepsData, String>,
    },
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn done_err(e: String) -> PlasmaResp {
    PlasmaResp::Done { ok: false, lines: vec![e], install: vec![] }
}

/// Run a request. The runtime calls this on a worker thread with the real folders; tests
/// call it with a temporary home.
pub fn run(roots: &Roots, amb: &Ambient, req: PlasmaReq) -> PlasmaResp {
    let cfg = || Config::load(roots).map(|(c, _)| c).map_err(err);
    match req {
        PlasmaReq::Refresh => PlasmaResp::Profiles(profile::list(roots).map_err(err)),
        PlasmaReq::Save { name, assets, force } => {
            match cfg().and_then(|c| profile::save(roots, &c, &name, SaveOpts { force, assets, only: None }, amb).map_err(err)) {
                Ok(r) => {
                    let mut lines = vec![format!(
                        "Saved {:?}: {} files, {}; {} listed entries are not on this machine.",
                        r.name,
                        r.files,
                        human_size(r.bytes),
                        r.missing.len()
                    )];
                    lines.extend(r.warnings.iter().map(|w| format!("Warning: {w}")));
                    if !assets {
                        lines.push("Themes, icons and plasmoids are not stored (Tab on the name adds them); export adds them to the archive.".into());
                    }
                    PlasmaResp::Done { ok: true, lines, install: vec![] }
                }
                Err(e) => done_err(e),
            }
        }
        PlasmaReq::Plan { name, only } => {
            let r = cfg().and_then(|c| {
                let p = profile::load(roots, &name).map_err(err)?;
                let plan = apply::plan(roots, &c, &p, Some(&only)).map_err(err)?;
                Ok((plan.render(), plan.has_changes()))
            });
            PlasmaResp::Plan { name, result: r }
        }
        PlasmaReq::Apply { name, only, restart } => {
            let r = (|| -> Result<Vec<String>, String> {
                let c = cfg()?;
                let p = profile::load(roots, &name).map_err(err)?;
                let plan = apply::plan(roots, &c, &p, Some(&only)).map_err(err)?;
                let tools = restart.then(|| reload::detect_tools(amb.plasma_version.as_deref(), &crate::ctx::have)).flatten();
                let mut lines = Vec::new();
                if restart && tools.is_none() {
                    lines.push("kquitapp/kstart not found: applied without restarting Plasma.".to_string());
                }
                if let Some(t) = &tools {
                    reload::quit_shell(t)?;
                }
                let rep = apply::execute(roots, &plan, true, amb);
                if let Some(t) = &tools
                    && let Err(e) = reload::start_shell(t)
                {
                    lines.push(format!("Warning: {e}"));
                }
                let rep = rep.map_err(err)?;
                lines.push(format!(
                    "Applied {name:?}: {} created, {} overwritten, {} unchanged, {} skipped.",
                    rep.created, rep.overwritten, rep.unchanged, rep.skipped
                ));
                if let Some(s) = &rep.snapshot {
                    lines.push(format!("Undo: apply the profile {s}."));
                }
                lines.extend(rep.failed.iter().map(|f| format!("Failed: {f}")));
                if tools.is_none() {
                    lines.push(reload::WARNING.to_string());
                }
                if rep.failed.is_empty() { Ok(lines) } else { Err(lines.join("\n")) }
            })();
            match r {
                Ok(lines) => PlasmaResp::Done { ok: true, lines, install: vec![] },
                Err(e) => done_err(e),
            }
        }
        PlasmaReq::Remove(name) => match profile::remove(roots, &name) {
            Ok(()) => PlasmaResp::Done { ok: true, lines: vec![format!("Removed {name:?}.")], install: vec![] },
            Err(e) => done_err(err(e)),
        },
        PlasmaReq::Export { name, out, assets, force } => match cfg()
            .and_then(|c| archive::export(roots, &c, &name, &out, ExportOpts { force, assets }, amb).map_err(err))
        {
            Ok(r) => {
                let mut lines =
                    vec![format!("Exported to {} ({}, {} asset files added from the live folders).", r.path.display(), human_size(r.bytes), r.assets_added)];
                lines.extend(r.warnings.iter().map(|w| format!("Warning: {w}")));
                PlasmaResp::Done { ok: true, lines, install: vec![] }
            }
            Err(e) => done_err(e),
        },
        PlasmaReq::ImportPreview { file, name } => {
            let r = archive::import(roots, &file, &ImportOpts { name: name.clone(), force: true, dry_run: true }).map_err(err).map(|r| {
                let s = r.stats;
                let mut t = format!(
                    "Profile {:?}{}\n{} files, {} links, {}\nFrom {} (Plasma {}, kast {}), made {}\n",
                    r.name,
                    if r.replaces { " (replaces the existing profile!)" } else { "" },
                    s.files,
                    s.links,
                    human_size(s.bytes),
                    r.meta.hostname,
                    r.meta.plasma_version.as_deref().unwrap_or("?"),
                    r.meta.kast_version,
                    r.meta.created
                );
                for sec in &r.manifest.sections {
                    t.push_str(&format!("  {:<12} {} entries\n", sec.category.name(), sec.entries.len()));
                }
                t.push_str("\nThe archive passed every safety check. Importing only creates the profile; nothing is applied.");
                (t, r.replaces)
            });
            PlasmaResp::ImportPreview { file, name, result: r }
        }
        PlasmaReq::Import { file, name, force } => match archive::import(roots, &file, &ImportOpts { name, force, dry_run: false }) {
            Ok(r) => {
                let mut lines = vec![format!("Imported profile {:?} ({} files). Apply it with a: nothing was changed yet.", r.name, r.stats.files)];
                let mut install = Vec::new();
                if let Ok(p) = profile::load(roots, &r.name) {
                    let missing = deps::missing(&p.meta.requires, &Pacman);
                    let (ok, no) = deps::split_installable(&missing, &Pacman);
                    if !ok.is_empty() {
                        lines.push(format!("Needs packages that are not installed: {}", ok.join(" ")));
                    }
                    if !no.is_empty() {
                        lines.push(format!("Not in any enabled repository (AUR?): {}", no.join(" ")));
                    }
                    install = ok;
                }
                PlasmaResp::Done { ok: true, lines, install }
            }
            Err(e) => done_err(err(e)),
        },
        PlasmaReq::Check => PlasmaResp::Check(cfg().and_then(|c| check::check(roots, &c).map_err(err))),
        PlasmaReq::Deps(name) => {
            let r = profile::load(roots, &name).map_err(err).map(|p| {
                let missing = deps::missing(&p.meta.requires, &Pacman);
                let (ok, no) = deps::split_installable(&missing, &Pacman);
                let rows = p
                    .meta
                    .requires
                    .iter()
                    .map(|r| DepRow { kind: r.kind.clone(), name: r.name.clone(), package: r.package.clone(), missing: missing.contains(&r.package) })
                    .collect();
                (rows, ok, no)
            });
            PlasmaResp::Deps { name, result: r }
        }
    }
}

// --- State ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Save,
    Export,
    Import,
}

#[derive(Debug)]
pub struct ApplyState {
    pub name: String,
    pub cats: Vec<(Category, bool)>,
    pub restart: bool,
    pub focus: usize,
    pub preview: Option<Result<(String, bool), String>>,
    pub scroll: u16,
}

#[derive(Debug)]
pub struct PreviewState {
    pub file: PathBuf,
    pub name: Option<String>,
    pub text: String,
    pub replaces: bool,
}

#[derive(Debug)]
pub enum Mode {
    List,
    Input { kind: InputKind, input: Input, name_input: Input, field: usize, assets: bool },
    Apply(ApplyState),
    ImportPreview(PreviewState),
    Check { rows: Option<Result<Vec<Row>, String>>, all: bool, scroll: usize },
    Deps { name: String, rows: Option<Result<DepsData, String>> },
}

#[derive(Debug)]
pub struct PlasmaState {
    pub mode: Mode,
    pub profiles: Vec<Summary>,
    pub selected: usize,
    pub loading: bool,
    pub busy: bool,
    /// What the last operation printed (shown under the list)
    pub message: Vec<String>,
    pub message_ok: bool,
}

impl PlasmaState {
    pub fn new() -> Self {
        PlasmaState { mode: Mode::List, profiles: vec![], selected: 0, loading: true, busy: false, message: vec![], message_ok: true }
    }

    pub fn working(&self) -> bool {
        self.loading
            || self.busy
            || matches!(self.mode, Mode::Check { rows: None, .. } | Mode::Deps { rows: None, .. } | Mode::Apply(ApplyState { preview: None, .. }))
    }

    fn current(&self) -> Option<&Summary> {
        self.profiles.get(self.selected)
    }
}

fn state(app: &mut App) -> Option<&mut PlasmaState> {
    match &mut app.screen {
        Screen::Plasma(s) => Some(s),
        _ => None,
    }
}

pub fn open(app: &mut App) -> Option<Command> {
    app.screen = Screen::Plasma(Box::new(PlasmaState::new()));
    Some(Command::Plasma(PlasmaReq::Refresh))
}

/// A text field has the keyboard (letters must reach it, not shortcuts).
pub fn editing(app: &App) -> bool {
    matches!(&app.screen, Screen::Plasma(s) if matches!(s.mode, Mode::Input { .. }))
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(st) = state(app)
        && let Mode::Input { input, name_input, field, .. } = &mut st.mode
    {
        if *field == 0 { input.insert(text) } else { name_input.insert(text) }
    }
}

fn start_busy(app: &mut App, req: PlasmaReq) -> Option<Command> {
    state(app)?.busy = true;
    Some(Command::Plasma(req))
}

fn ask(app: &mut App, title: &str, text: String, yes: &'static str, req: PlasmaReq) {
    app.modal = Some(Modal { kind: ModalKind::Plasma(Box::new(req)), title: title.into(), text, details: vec![], yes, no: "Cancel", yes_selected: false });
}

/// The answer to a [`PlasmaReq`].
pub fn on_resp(app: &mut App, resp: PlasmaResp) -> Option<Command> {
    let mut follow = None;
    let mut toast = None;
    let mut install_offer = None;
    {
        let st = state(app)?;
        match resp {
            PlasmaResp::Profiles(r) => {
                st.loading = false;
                match r {
                    Ok(p) => {
                        st.profiles = p;
                        st.selected = st.selected.min(st.profiles.len().saturating_sub(1));
                    }
                    Err(e) => {
                        st.message = vec![e];
                        st.message_ok = false;
                    }
                }
            }
            PlasmaResp::Done { ok, lines, install } => {
                st.busy = false;
                if !matches!(st.mode, Mode::List) {
                    st.mode = Mode::List;
                }
                toast = Some((if ok { ToastKind::Ok } else { ToastKind::Error }, lines.first().cloned().unwrap_or_default()));
                st.message = lines;
                st.message_ok = ok;
                st.loading = true;
                follow = Some(Command::Plasma(PlasmaReq::Refresh));
                if ok && !install.is_empty() {
                    install_offer = Some(install);
                }
            }
            PlasmaResp::Plan { name, result } => {
                if let Mode::Apply(a) = &mut st.mode
                    && a.name == name
                {
                    a.preview = Some(result);
                    a.scroll = 0;
                }
            }
            PlasmaResp::ImportPreview { file, name, result } => {
                st.busy = false;
                match result {
                    Ok((text, replaces)) => st.mode = Mode::ImportPreview(PreviewState { file, name, text, replaces }),
                    Err(e) => {
                        toast = Some((ToastKind::Error, e.clone()));
                        st.message = vec![e];
                        st.message_ok = false;
                        st.mode = Mode::List;
                    }
                }
            }
            PlasmaResp::Check(r) => {
                if let Mode::Check { rows, .. } = &mut st.mode {
                    *rows = Some(r);
                }
            }
            PlasmaResp::Deps { name, result } => {
                if let Mode::Deps { name: n, rows } = &mut st.mode
                    && *n == name
                {
                    *rows = Some(result);
                }
            }
        }
    }
    if let Some((k, t)) = toast {
        app.toast(k, t);
    }
    if let Some(pkgs) = install_offer {
        offer_install(app, pkgs);
    }
    follow
}

fn offer_install(app: &mut App, pkgs: Vec<String>) {
    app.modal = Some(Modal {
        kind: ModalKind::Confirm(Box::new(JobKind::PlasmaDeps(pkgs.clone()))),
        title: "Install missing packages?".into(),
        text: "This profile uses themes or widgets from packages that are not installed.".into(),
        details: vec![pkgs.join(" "), "pacman -S --needed, from your enabled repositories (including archrepo).".into()],
        yes: "Install",
        no: "Not now",
        yes_selected: false,
    });
}

// --- Keys ----------------------------------------------------------------------------------

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    match &mut st.mode {
        Mode::List => list_key(app, k),
        Mode::Input { .. } => input_key(app, k),
        Mode::Apply(_) => apply_key(app, k),
        Mode::ImportPreview(_) => preview_key(app, k),
        Mode::Check { .. } => check_key(app, k),
        Mode::Deps { .. } => deps_key(app, k),
    }
}

fn open_input(app: &mut App, kind: InputKind) {
    let home = format!("{}/", app.header.home.trim_end_matches('/'));
    let Some(st) = state(app) else { return };
    let initial = match kind {
        InputKind::Save => String::new(),
        InputKind::Export => home,
        InputKind::Import => home,
    };
    st.mode = Mode::Input { kind, input: Input::new(initial), name_input: Input::default(), field: 0, assets: kind == InputKind::Export };
}

fn list_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    if st.busy {
        return None;
    }
    let n = st.profiles.len();
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => st.selected = st.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => st.selected = (st.selected + 1).min(n.saturating_sub(1)),
        KeyCode::Char('r') => {
            st.loading = true;
            return Some(Command::Plasma(PlasmaReq::Refresh));
        }
        KeyCode::Char('s') => open_input(app, InputKind::Save),
        KeyCode::Char('i') => open_input(app, InputKind::Import),
        KeyCode::Char('e') if st.current().is_some() => open_input(app, InputKind::Export),
        KeyCode::Char('c') => {
            st.mode = Mode::Check { rows: None, all: false, scroll: 0 };
            return Some(Command::Plasma(PlasmaReq::Check));
        }
        KeyCode::Char('p') | KeyCode::Char('P') if st.current().is_some() => {
            let name = st.current()?.name.clone();
            st.mode = Mode::Deps { name: name.clone(), rows: None };
            return Some(Command::Plasma(PlasmaReq::Deps(name)));
        }
        KeyCode::Char('d') | KeyCode::Delete if st.current().is_some() => {
            let name = st.current()?.name.clone();
            ask(app, "Remove profile", format!("Remove the profile {name:?}?"), "Remove", PlasmaReq::Remove(name));
        }
        KeyCode::Enter | KeyCode::Char('a') if st.current().is_some() => {
            let cur = st.current()?;
            let name = cur.name.clone();
            let cats: Vec<(Category, bool)> = if cur.categories.is_empty() {
                Category::ALL.iter().map(|c| (*c, true)).collect()
            } else {
                cur.categories.iter().map(|c| (*c, true)).collect()
            };
            let only = cats.iter().map(|(c, _)| *c).collect();
            st.mode = Mode::Apply(ApplyState { name: name.clone(), cats, restart: false, focus: 0, preview: None, scroll: 0 });
            return Some(Command::Plasma(PlasmaReq::Plan { name, only }));
        }
        KeyCode::Esc | KeyCode::Char('q') => app.screen = Screen::Menu,
        _ => {}
    }
    None
}

fn input_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let selected = state(app)?.current().map(|s| s.name.clone());
    let existing: Vec<String> = state(app)?.profiles.iter().map(|p| p.name.clone()).collect();
    let st = state(app)?;
    let Mode::Input { kind, input, name_input, field, assets } = &mut st.mode else { return None };
    match k.code {
        KeyCode::Esc => {
            st.mode = Mode::List;
            return None;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            match kind {
                InputKind::Import => *field = 1 - *field,
                _ => *assets = !*assets,
            }
            return None;
        }
        KeyCode::Enter => {
            let (kind, assets, text, alt) = (*kind, *assets, input.value.trim().to_string(), name_input.value.trim().to_string());
            match kind {
                InputKind::Save => {
                    if !valid_profile_name(&text) {
                        app.toast(ToastKind::Error, "Name: letters, digits, - _ . only; no leading dot; at most 64 characters");
                        return None;
                    }
                    if existing.contains(&text) {
                        ask(
                            app,
                            "Replace profile",
                            format!("A profile named {text:?} exists. Replace it (rebuilt, not merged)?"),
                            "Replace",
                            PlasmaReq::Save { name: text, assets, force: true },
                        );
                        return None;
                    }
                    return start_busy(app, PlasmaReq::Save { name: text, assets, force: false });
                }
                InputKind::Export => {
                    let name = selected?;
                    let out = PathBuf::from(&text);
                    let target = archive::export_path(&out, &name);
                    if target.exists() {
                        ask(
                            app,
                            "Overwrite file",
                            format!("{} exists. Overwrite it?", target.display()),
                            "Overwrite",
                            PlasmaReq::Export { name, out, assets, force: true },
                        );
                        return None;
                    }
                    return start_busy(app, PlasmaReq::Export { name, out, assets, force: false });
                }
                InputKind::Import => {
                    if text.is_empty() {
                        return None;
                    }
                    let name = (!alt.is_empty()).then_some(alt);
                    if name.as_deref().is_some_and(|n| !valid_profile_name(n)) {
                        app.toast(ToastKind::Error, "Profile name: letters, digits, - _ . only");
                        return None;
                    }
                    return start_busy(app, PlasmaReq::ImportPreview { file: PathBuf::from(&text), name });
                }
            }
        }
        _ => {}
    }
    let target = if *field == 0 { input } else { name_input };
    target.key(&k);
    None
}

fn apply_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Mode::Apply(a) = &mut st.mode else { return None };
    let rows = a.cats.len() + 1;
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => st.mode = Mode::List,
        KeyCode::Up | KeyCode::Char('k') => a.focus = a.focus.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => a.focus = (a.focus + 1).min(rows - 1),
        KeyCode::PageUp => a.scroll = a.scroll.saturating_sub(5),
        KeyCode::PageDown => a.scroll = a.scroll.saturating_add(5),
        KeyCode::Char(' ') => {
            if a.focus < a.cats.len() {
                a.cats[a.focus].1 = !a.cats[a.focus].1;
                a.preview = None;
                let only: Vec<Category> = a.cats.iter().filter(|c| c.1).map(|c| c.0).collect();
                return Some(Command::Plasma(PlasmaReq::Plan { name: a.name.clone(), only }));
            }
            a.restart = !a.restart;
        }
        KeyCode::Enter | KeyCode::Char('a') => {
            let only: Vec<Category> = a.cats.iter().filter(|c| c.1).map(|c| c.0).collect();
            let has_changes = matches!(&a.preview, Some(Ok((_, true))));
            if only.is_empty() || !has_changes {
                app.toast(ToastKind::Info, "Nothing to apply for this selection");
                return None;
            }
            let text = format!(
                "Apply {:?} ({} categories)? Files it overwrites are snapshotted first as pre-apply-<time>.{}",
                a.name,
                only.len(),
                if a.restart { " Plasma is quit before and started after." } else { "" }
            );
            let req = PlasmaReq::Apply { name: a.name.clone(), only, restart: a.restart };
            ask(app, "Apply profile", text, "Apply", req);
        }
        _ => {}
    }
    None
}

fn preview_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Mode::ImportPreview(p) = &mut st.mode else { return None };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => st.mode = Mode::List,
        KeyCode::Enter | KeyCode::Char('y') => {
            let (file, name, replaces) = (p.file.clone(), p.name.clone(), p.replaces);
            if replaces {
                ask(app, "Replace profile", "A profile with this name exists. Replace it?".into(), "Replace", PlasmaReq::Import { file, name, force: true });
            } else {
                return start_busy(app, PlasmaReq::Import { file, name, force: false });
            }
        }
        _ => {}
    }
    None
}

fn check_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Mode::Check { all, scroll, .. } = &mut st.mode else { return None };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => st.mode = Mode::List,
        KeyCode::Char('a') => {
            *all = !*all;
            *scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
        KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
        KeyCode::PageDown => *scroll += 10,
        KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
        _ => {}
    }
    None
}

fn deps_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Mode::Deps { rows, .. } = &mut st.mode else { return None };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => st.mode = Mode::List,
        KeyCode::Char('i') | KeyCode::Enter => {
            if let Some(Ok((_, ok, _))) = rows.as_ref()
                && !ok.is_empty()
            {
                let pkgs = ok.clone();
                offer_install(app, pkgs);
            } else {
                app.toast(ToastKind::Info, "Nothing to install");
            }
        }
        _ => {}
    }
    None
}

// --- Drawing -------------------------------------------------------------------------------

pub fn hints(st: &PlasmaState) -> &'static [(&'static str, &'static str)] {
    match st.mode {
        Mode::List => &[
            ("↑/↓", "move"),
            ("a", "apply"),
            ("s", "save"),
            ("e", "export"),
            ("i", "import"),
            ("d", "remove"),
            ("p", "packages"),
            ("c", "check"),
            ("Esc", "back"),
        ],
        Mode::Input { .. } => &[("Enter", "go"), ("Tab", "switch/toggle"), ("Esc", "cancel")],
        Mode::Apply(_) => &[("↑/↓", "move"), ("Space", "toggle"), ("Enter", "apply"), ("PgUp/PgDn", "preview"), ("Esc", "back")],
        Mode::ImportPreview(_) => &[("Enter", "import"), ("Esc", "cancel")],
        Mode::Check { .. } => &[("↑/↓", "scroll"), ("a", "all entries"), ("Esc", "back")],
        Mode::Deps { .. } => &[("i", "install missing"), ("Esc", "back")],
    }
}

pub const HELP: [(&str, &str); 10] = [
    ("↑/↓  j/k", "Choose a profile"),
    ("a  Enter", "Apply: pick categories, read the dry-run preview, then confirm"),
    ("s", "Save the current settings as a profile (Tab adds themes, icons and plasmoids)"),
    ("e", "Export the chosen profile as a .kast-plasma.tar.gz"),
    ("i", "Import an archive: it is checked and previewed first; nothing is applied"),
    ("d", "Remove the chosen profile"),
    ("p", "Packages the profile needs (themes, icon themes, plasmoids), install the missing"),
    ("c", "Check the list against ~/.config and ~/.local/share/plasma"),
    ("r", "Refresh"),
    ("Esc  q", "Back to the menu"),
];

fn spin(app: &App) -> &'static str {
    app.theme.sym.spinner[app.frame % app.theme.sym.spinner.len()]
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &PlasmaState) {
    match &st.mode {
        Mode::List => view_list(f, body, app, st),
        Mode::Input { kind, input, name_input, field, assets } => view_input(f, body, app, st, *kind, input, name_input, *field, *assets),
        Mode::Apply(a) => view_apply(f, body, app, a),
        Mode::ImportPreview(p) => {
            let mut t = Text::from(p.text.clone());
            if p.replaces {
                t.lines.insert(
                    0,
                    Line::styled(format!("{} A profile with this name exists and will be replaced after you confirm.", app.theme.sym.warn), app.theme.warn()),
                );
            }
            f.render_widget(Paragraph::new(t).wrap(Wrap { trim: false }).block(block(&app.theme, " Import preview (nothing written yet) ")), body);
        }
        Mode::Check { rows, all, scroll } => view_check(f, body, app, rows.as_ref(), *all, *scroll),
        Mode::Deps { name, rows } => view_deps(f, body, app, name, rows.as_ref()),
    }
}

fn view_list(f: &mut Frame, body: Rect, app: &App, st: &PlasmaState) {
    let theme = &app.theme;
    let msg_h = (st.message.len() as u16 + 2).clamp(3, 8).min(body.height.saturating_sub(6));
    let [top, msg] = Layout::vertical([Constraint::Min(5), Constraint::Length(msg_h)]).areas(body);
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);
    let items: Vec<ListItem> = if st.profiles.is_empty() {
        vec![ListItem::new(Line::styled(
            if st.loading { format!("{} Reading profiles{}", spin(app), theme.sym.ellipsis) } else { "No profiles yet: press s to save one".to_string() },
            theme.dim(),
        ))]
    } else {
        st.profiles
            .iter()
            .map(|p| {
                let snap = p.meta.as_ref().is_some_and(|m| m.kind == ProfileKind::Snapshot);
                let tag = if snap { " (snapshot)" } else { "" };
                ListItem::new(format!("{}{tag}", p.name))
            })
            .collect()
    };
    let list = List::new(items).block(block(theme, " Plasma profiles ")).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected((!st.profiles.is_empty()).then_some(st.selected));
    f.render_stateful_widget(list, list_area, &mut ls);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(p) = st.current() {
        let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<10}"), theme.bold()), Span::raw(v)]);
        lines.push(kv("Profile", p.name.clone()));
        if let Some(m) = &p.meta {
            lines.push(kv("Made", format!("{} on {}", m.created, m.hostname)));
            lines.push(kv("Plasma", format!("{} (kast {})", m.plasma_version.as_deref().unwrap_or("?"), m.kast_version)));
            if !m.requires.is_empty() {
                lines.push(kv("Needs", format!("{} system package(s): press p", m.requires.len())));
            }
        }
        lines.push(kv("Size", format!("{} files, {}", p.files, human_size(p.bytes))));
        lines.push(kv("Covers", p.categories.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Applying never deletes files, and snapshots what it overwrites first.", theme.dim()));
    } else {
        lines.push(Line::styled("Save, apply, export and import KDE Plasma settings.", theme.dim()));
        lines.push(Line::styled("A Rust port of konsave's logic; see README.", theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Details ")), detail);

    let mut m: Vec<Line> = Vec::new();
    if st.busy {
        m.push(Line::styled(format!("{} Working{}", spin(app), theme.sym.ellipsis), theme.dim()));
    }
    let style = if st.message_ok { theme.ok() } else { theme.error() };
    m.extend(st.message.iter().flat_map(|l| l.lines()).map(|l| Line::styled(l.to_string(), if l.starts_with("Warning") { theme.warn() } else { style })));
    f.render_widget(Paragraph::new(Text::from(m)).wrap(Wrap { trim: false }).block(block(theme, " Last result ")), msg);
}

#[allow(clippy::too_many_arguments)]
fn view_input(f: &mut Frame, body: Rect, app: &App, st: &PlasmaState, kind: InputKind, input: &Input, name_input: &Input, field: usize, assets: bool) {
    let theme = &app.theme;
    let (title, label, hint) = match kind {
        InputKind::Save => {
            (" Save current settings ", "Profile name", "Saves the Plasma settings that exist on this machine. Credentials-like settings are flagged.")
        }
        InputKind::Export => (
            " Export the chosen profile ",
            "Folder or file",
            "A folder gets <profile>.kast-plasma.tar.gz. Large assets (themes, icons, plasmoids) are copied from your live folders into the archive.",
        ),
        InputKind::Import => (" Import an archive ", "Archive file", "Checked and previewed first. Importing only creates a profile; nothing is applied."),
    };
    let [form, rest] = Layout::vertical([Constraint::Length(if kind == InputKind::Import { 8 } else { 7 }), Constraint::Min(0)]).areas(body);
    let inner = block(theme, title).inner(form);
    f.render_widget(block(theme, title), form);
    let mut rows = vec![Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)];
    rows.push(Constraint::Length(1));
    rows.push(Constraint::Min(1));
    let areas = Layout::vertical(rows).split(inner);
    f.render_widget(Paragraph::new(Line::styled(label, theme.bold())), areas[0]);
    input.render(f, areas[1], theme, field == 0);
    match kind {
        InputKind::Import => {
            f.render_widget(Paragraph::new(Line::styled("Profile name (optional, default: from the file name)", theme.bold())), areas[2]);
            name_input.render(f, areas[3], theme, field == 1);
        }
        _ => {
            let on = if assets { theme.sym.on } else { theme.sym.off };
            f.render_widget(Paragraph::new(format!("{on} include themes, icons, plasmoids and other large assets (Tab)")), areas[2]);
        }
    }
    f.render_widget(Paragraph::new(Line::styled(hint, theme.dim())).wrap(Wrap { trim: false }), areas[4]);
    if st.busy {
        f.render_widget(Paragraph::new(Line::styled(format!("{} Working{}", spin(app), theme.sym.ellipsis), theme.dim())), rest);
    }
}

fn view_apply(f: &mut Frame, body: Rect, app: &App, a: &ApplyState) {
    let theme = &app.theme;
    let [left, right] = Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(body);
    let mut items: Vec<ListItem> =
        a.cats.iter().map(|(c, on)| ListItem::new(format!("{} {}", if *on { theme.sym.on } else { theme.sym.off }, c.name()))).collect();
    items.push(ListItem::new(format!("{} restart Plasma around apply", if a.restart { theme.sym.on } else { theme.sym.off })));
    let list = List::new(items)
        .block(block(theme, format!(" Apply {} ", a.name)))
        .highlight_style(theme.selected())
        .highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(a.focus));
    f.render_stateful_widget(list, left, &mut ls);
    let text = match &a.preview {
        None => Text::from(Line::styled(format!("{} Planning{}", spin(app), theme.sym.ellipsis), theme.dim())),
        Some(Err(e)) => Text::from(Line::styled(e.clone(), theme.error())),
        Some(Ok((t, _))) => {
            let mut lines: Vec<Line> = vec![];
            if let Some((c, _)) = a.cats.get(a.focus) {
                lines.push(Line::styled(format!("{}: {}", c.name(), c.describe()), theme.dim()));
            }
            lines.extend(t.lines().map(|l| {
                let style = if l.trim_start().starts_with("overwrite") {
                    theme.warn()
                } else if l.trim_start().starts_with("skip") {
                    theme.dim()
                } else {
                    ratatui::style::Style::new()
                };
                Line::styled(l.to_string(), style)
            }));
            lines.push(Line::raw(""));
            lines.push(Line::styled(crate::plasma::reload::WARNING, theme.dim()));
            Text::from(lines)
        }
    };
    f.render_widget(Paragraph::new(text).scroll((a.scroll, 0)).wrap(Wrap { trim: false }).block(block(theme, " Dry run: what would happen ")), right);
}

fn view_check(f: &mut Frame, body: Rect, app: &App, rows: Option<&Result<Vec<Row>, String>>, all: bool, scroll: usize) {
    let theme = &app.theme;
    let title = " Config check: list vs ~/.config and ~/.local/share/plasma ";
    let lines: Vec<Line> = match rows {
        None => vec![Line::styled(format!("{} Reading{}", spin(app), theme.sym.ellipsis), theme.dim())],
        Some(Err(e)) => vec![Line::styled(e.clone(), theme.error())],
        Some(Ok(rows)) => {
            let mut out = Vec::new();
            let mut dir: Option<&std::path::Path> = None;
            for r in rows {
                let hide = (r.status == Status::Tracked || (r.status == Status::Untracked && !r.kde_like)) && !all;
                if hide {
                    continue;
                }
                if dir != Some(&r.dir) {
                    out.push(Line::styled(r.dir.display().to_string(), theme.title()));
                    dir = Some(&r.dir);
                }
                let (mark, style) = match r.status {
                    Status::Untracked if r.kde_like => ("!", theme.warn()),
                    Status::Missing | Status::Untracked => (" ", theme.dim()),
                    Status::Partial => (" ", theme.info()),
                    Status::Tracked => (" ", theme.ok()),
                };
                let by = if r.sections.is_empty() { String::new() } else { format!("  [{}]", r.sections.join(", ")) };
                out.push(Line::styled(format!(" {mark} {:<42} {}{by}", r.name, r.status.label()), style));
            }
            let n = |s: Status| rows.iter().filter(|r| r.status == s).count();
            out.push(Line::raw(""));
            out.push(Line::raw(format!(
                "{} tracked, {} untracked ({} look like settings), {} listed but not on disk. Add to ~/.config/kast/plasma.toml (kast plasma default-config).",
                n(Status::Tracked),
                n(Status::Untracked),
                rows.iter().filter(|r| r.status == Status::Untracked && r.kde_like).count(),
                n(Status::Missing)
            )));
            out
        }
    };
    let skip = scroll.min(lines.len().saturating_sub(1));
    f.render_widget(Paragraph::new(Text::from(lines.into_iter().skip(skip).collect::<Vec<_>>())).wrap(Wrap { trim: false }).block(block(theme, title)), body);
}

fn view_deps(f: &mut Frame, body: Rect, app: &App, name: &str, rows: Option<&Result<DepsData, String>>) {
    let theme = &app.theme;
    let lines: Vec<Line> = match rows {
        None => vec![Line::styled(format!("{} Asking pacman{}", spin(app), theme.sym.ellipsis), theme.dim())],
        Some(Err(e)) => vec![Line::styled(e.clone(), theme.error())],
        Some(Ok((rows, ok, no))) => {
            let mut out: Vec<Line> = Vec::new();
            if rows.is_empty() {
                out.push(Line::styled("No system themes or plasmoids recorded for this profile.", theme.dim()));
            }
            for r in rows {
                let (state, style) = if r.missing { ("missing", theme.warn()) } else { ("installed", theme.ok()) };
                out.push(Line::styled(format!("  {:<14} {:<28} {} ({state})", r.kind, r.name, r.package), style));
            }
            out.push(Line::raw(""));
            if !ok.is_empty() {
                out.push(Line::raw(format!("Press i to install: {}", ok.join(" "))));
            }
            if !no.is_empty() {
                out.push(Line::styled(format!("Not in any enabled repository (AUR?): {}", no.join(" ")), theme.warn()));
            }
            out
        }
    };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, format!(" Packages {name} needs "))), body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plasma::profile::testutil::{ambient, home};
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;

    fn feed(a: &mut App, resp: PlasmaResp) -> Option<Command> {
        update(a, Event::Loaded(Loaded::Plasma(Box::new(resp))))
    }

    #[test]
    fn worker_save_apply_remove_round_trip_in_a_temp_home() {
        let (_d, r) = home();
        let amb = ambient();
        let PlasmaResp::Done { ok, .. } = run(&r, &amb, PlasmaReq::Save { name: "t".into(), assets: false, force: false }) else { panic!() };
        assert!(ok);
        let PlasmaResp::Done { ok: dup, .. } = run(&r, &amb, PlasmaReq::Save { name: "t".into(), assets: false, force: false }) else { panic!() };
        assert!(!dup, "saving over an existing profile needs force");
        std::fs::write(r.config.join("kwinrc"), "changed").unwrap();
        let PlasmaResp::Plan { result: Ok((text, changes)), .. } = run(&r, &amb, PlasmaReq::Plan { name: "t".into(), only: Category::ALL.to_vec() }) else {
            panic!()
        };
        assert!(changes && text.contains("overwrite"), "{text}");
        let PlasmaResp::Plan { result: Ok((_, none)), .. } = run(&r, &amb, PlasmaReq::Plan { name: "t".into(), only: vec![Category::Input] }) else { panic!() };
        assert!(!none, "only the chosen categories are planned");
        let PlasmaResp::Done { ok, lines, .. } = run(&r, &amb, PlasmaReq::Apply { name: "t".into(), only: Category::ALL.to_vec(), restart: false }) else {
            panic!()
        };
        assert!(ok, "{lines:?}");
        assert_eq!(std::fs::read_to_string(r.config.join("kwinrc")).unwrap(), "[Windows]\nBorderSnapZone=20\n");
        let PlasmaResp::Profiles(Ok(l)) = run(&r, &amb, PlasmaReq::Refresh) else { panic!() };
        assert!(l.iter().any(|s| s.name.starts_with("pre-apply-")), "a snapshot was taken");
        let PlasmaResp::Done { ok, .. } = run(&r, &amb, PlasmaReq::Remove("t".into())) else { panic!() };
        assert!(ok);
        let PlasmaResp::Check(Ok(rows)) = run(&r, &amb, PlasmaReq::Check) else { panic!() };
        assert!(!rows.is_empty());
    }

    #[test]
    fn worker_export_import_with_preview() {
        let (d, r) = home();
        let amb = ambient();
        run(&r, &amb, PlasmaReq::Save { name: "t".into(), assets: false, force: false });
        let out = d.path().join("t.kast-plasma.tar.gz");
        let PlasmaResp::Done { ok, .. } = run(&r, &amb, PlasmaReq::Export { name: "t".into(), out: out.clone(), assets: true, force: false }) else { panic!() };
        assert!(ok && out.is_file());
        let PlasmaResp::ImportPreview { result: Ok((text, replaces)), .. } =
            run(&r, &amb, PlasmaReq::ImportPreview { file: out.clone(), name: Some("copy".into()) })
        else {
            panic!()
        };
        assert!(!replaces && text.contains("passed every safety check"), "{text}");
        assert!(!r.profiles.join("copy").exists(), "the preview writes nothing");
        let PlasmaResp::Done { ok, .. } = run(&r, &amb, PlasmaReq::Import { file: out, name: Some("copy".into()), force: false }) else { panic!() };
        assert!(ok && r.profiles.join("copy/profile.toml").is_file());
    }

    #[test]
    fn screen_flow_apply_checkboxes_and_confirmation() {
        let mut a = app(100, 30);
        let cmd = crate::tui::app::activate(&mut a, crate::tui::screens::menu::Action::Plasma);
        assert!(matches!(cmd, Some(Command::Plasma(PlasmaReq::Refresh))));
        let (_d, r) = home();
        let PlasmaResp::Done { .. } = run(&r, &ambient(), PlasmaReq::Save { name: "t".into(), assets: false, force: false }) else { panic!() };
        let PlasmaResp::Profiles(Ok(list)) = run(&r, &ambient(), PlasmaReq::Refresh) else { panic!() };
        feed(&mut a, PlasmaResp::Profiles(Ok(list)));
        assert!(render(&a).contains("Plasma profiles") && render(&a).contains(" t"));
        let cmd = update(&mut a, key(KeyCode::Char('a')));
        assert!(matches!(&cmd, Some(Command::Plasma(PlasmaReq::Plan { name, only })) if name == "t" && only.len() == 6), "{cmd:?}");
        feed(
            &mut a,
            PlasmaResp::Plan { name: "t".into(), result: Ok(("  overwrite /x/kwinrc\n1 to create, 1 to overwrite, 0 unchanged, 0 skipped".into(), true)) },
        );
        assert!(render(&a).contains("overwrite /x/kwinrc"));
        // untick the first category: a new preview is requested for the other five
        let cmd = update(&mut a, key(KeyCode::Char(' ')));
        assert!(matches!(&cmd, Some(Command::Plasma(PlasmaReq::Plan { only, .. })) if only.len() == 5), "{cmd:?}");
        feed(&mut a, PlasmaResp::Plan { name: "t".into(), result: Ok(("x".into(), true)) });
        update(&mut a, key(KeyCode::Enter));
        let m = a.modal.as_ref().expect("a confirmation");
        assert!(matches!(&m.kind, ModalKind::Plasma(r) if matches!(**r, PlasmaReq::Apply { .. })));
        let cmd = update(&mut a, key(KeyCode::Char('n')));
        assert!(cmd.is_none() && a.modal.is_none(), "No applies nothing");
        let cmd = update(&mut a, key(KeyCode::Esc));
        assert!(cmd.is_none());
        let cmd = update(&mut a, key(KeyCode::Esc));
        assert!(cmd.is_none() && matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn save_input_validates_asks_before_replacing_and_import_goes_through_a_preview() {
        let mut a = app(100, 30);
        crate::tui::app::activate(&mut a, crate::tui::screens::menu::Action::Plasma);
        let (_d, r) = home();
        run(&r, &ambient(), PlasmaReq::Save { name: "t".into(), assets: false, force: false });
        let PlasmaResp::Profiles(Ok(list)) = run(&r, &ambient(), PlasmaReq::Refresh) else { panic!() };
        feed(&mut a, PlasmaResp::Profiles(Ok(list)));
        update(&mut a, key(KeyCode::Char('s')));
        assert!(editing(&a));
        for c in "../x".chars() {
            update(&mut a, key(KeyCode::Char(c)));
        }
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Error);
        for _ in 0..4 {
            update(&mut a, key(KeyCode::Backspace));
        }
        update(&mut a, key(KeyCode::Char('t')));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(matches!(&a.modal.as_ref().unwrap().kind, ModalKind::Plasma(r) if matches!(**r, PlasmaReq::Save { force: true, .. })));
        update(&mut a, key(KeyCode::Char('n')));
        update(&mut a, key(KeyCode::Esc));
        update(&mut a, key(KeyCode::Char('i')));
        for c in "a.tar.gz".chars() {
            update(&mut a, key(KeyCode::Char(c)));
        }
        let cmd = update(&mut a, key(KeyCode::Enter));
        assert!(matches!(cmd, Some(Command::Plasma(PlasmaReq::ImportPreview { .. }))));
        feed(&mut a, PlasmaResp::ImportPreview { file: PathBuf::from("a.tar.gz"), name: None, result: Ok(("Profile \"a\"".into(), false)) });
        assert!(render(&a).contains("Import preview"));
        let cmd = update(&mut a, key(KeyCode::Enter));
        assert!(matches!(cmd, Some(Command::Plasma(PlasmaReq::Import { force: false, .. }))));
    }

    #[test]
    fn errors_show_in_the_panel_and_an_install_offer_follows_an_import() {
        let mut a = app(100, 30);
        crate::tui::app::activate(&mut a, crate::tui::screens::menu::Action::Plasma);
        feed(&mut a, PlasmaResp::Profiles(Ok(vec![])));
        feed(&mut a, PlasmaResp::Done { ok: false, lines: vec!["boom happened".into()], install: vec![] });
        assert!(render(&a).contains("boom happened"));
        feed(&mut a, PlasmaResp::Done { ok: true, lines: vec!["Imported".into()], install: vec!["papirus-icon-theme".into()] });
        let m = a.modal.as_ref().expect("offer");
        assert!(m.details.join(" ").contains("papirus-icon-theme"));
        assert!(matches!(&m.kind, ModalKind::Confirm(k) if matches!(**k, JobKind::PlasmaDeps(_))), "no auto-install: it needs a confirmation");
    }

    fn with_profile(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        crate::tui::app::activate(&mut a, crate::tui::screens::menu::Action::Plasma);
        let (_d, r) = home();
        run(&r, &ambient(), PlasmaReq::Save { name: "work".into(), assets: false, force: false });
        let PlasmaResp::Profiles(Ok(mut list)) = run(&r, &ambient(), PlasmaReq::Refresh) else { panic!() };
        // Fixed values so the snapshot is stable
        list[0].files = 12;
        list[0].bytes = 4096;
        if let Some(m) = list[0].meta.as_mut() {
            m.created = "2026-10-18T12:30:05+02:00".into();
            m.kast_version = "0.0.0".into();
        }
        feed(&mut a, PlasmaResp::Profiles(Ok(list)));
        a
    }

    #[test]
    fn snapshots() {
        let mut a = with_profile(80, 24);
        insta::assert_snapshot!("plasma_list_80x24", render(&a));
        feed(
            &mut a,
            PlasmaResp::Done { ok: true, lines: vec!["Saved \"work\": 12 files.".into(), "Warning: x looks like credentials".into()], install: vec![] },
        );
        insta::assert_snapshot!("plasma_list_result_80x24", render(&a));
        update(&mut a, key(KeyCode::Char('a')));
        feed(&mut a, PlasmaResp::Plan { name: "work".into(), result: Ok(("  create    /home/p3ter/.config/kwinrc\n  overwrite /home/p3ter/.config/kdeglobals\n  skip      /home/p3ter/.config/x  (a folder is in the way)\n1 to create, 1 to overwrite, 4 unchanged, 1 skipped".into(), true)) });
        insta::assert_snapshot!("plasma_apply_100x30", render(&a));
        let mut b = with_profile(100, 30);
        update(&mut b, key(KeyCode::Char('i')));
        insta::assert_snapshot!("plasma_import_input_100x30", render(&b));
    }
}
