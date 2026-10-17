//! The app picker: which packages to install, grouped as in packages.toml, all checked to
//! start with, and from which copy of the list (online, built-in and why, a local file).
//! Menu option 4 goes picker → confirm → run; the automatic run uses it as one stage.
//! The list is fetched in the background when the app opens; until it arrives this screen
//! says so. Which packages are installed already is found off the UI thread too.

use std::collections::HashSet;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::checklist::{self, Checklist, Item, Outcome, Row};
use crate::pkglist::{PkgList, Reason, Source};
use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind};
use crate::tui::job::JobKind;
use crate::tui::theme::Theme;
use crate::tui::widgets::{block, ellipsize};

/// The package list for this session.
#[derive(Debug, Clone)]
pub enum PkgState {
    Fetching,
    Ready(Arc<PkgList>),
}

/// Which packages are installed (`pacman -Q` / `pacman -Sl`, run off the UI thread).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Installed {
    #[default]
    Unknown,
    Loading,
    Known(HashSet<String>),
}

impl Installed {
    pub fn has(&self, name: &str) -> bool {
        matches!(self, Installed::Known(s) if s.contains(name))
    }
}

#[derive(Debug, Clone, Default)]
pub struct PackagesState {
    /// A stage of the automatic run (else menu option 4)
    pub auto: bool,
    /// The checkboxes, built once the list is here
    pub picker: Option<Checklist>,
    /// The list `picker` was built from (rebuilt when a retry brings another one)
    pub for_list: Option<Arc<PkgList>>,
}

impl PackagesState {
    pub fn new(auto: bool) -> Self {
        PackagesState { auto, ..Default::default() }
    }

    /// The chosen package names, None when every package is chosen.
    pub fn selection(&self) -> Option<Vec<String>> {
        let p = self.picker.as_ref()?;
        let (n, m) = p.count();
        (n < m).then(|| p.selected_keys())
    }
}

/// The source line with its marker: colour is never the only signal.
pub fn source_line<'a>(theme: &Theme, state: &PkgState, spinner: &'a str) -> Line<'a> {
    match state {
        PkgState::Fetching => Line::from(vec![Span::styled(format!("{spinner} "), theme.accent()), Span::raw("Fetching package list…")]),
        PkgState::Ready(pl) => source_text(theme, pl),
    }
}

/// "✓ Online · …", "⚠ Built-in · …" or "• Local file · …".
pub fn source_text(theme: &Theme, pl: &PkgList) -> Line<'static> {
    let (sym, style) = match pl.source {
        Source::Online { .. } => (theme.sym.ok, theme.ok()),
        Source::BuiltIn(_) => (theme.sym.warn, theme.warn()),
        Source::Local(_) => (theme.sym.bullet, theme.info()),
    };
    Line::from(vec![Span::styled(format!("{sym} "), style), Span::styled(sep(theme, &pl.source_line()), style)])
}

/// "·" is not in the Linux console font.
pub fn sep(theme: &Theme, s: &str) -> String {
    if theme.sym.ellipsis == "…" { s.to_string() } else { s.replace('·', "-").replace('→', "->").replace('−', "-") }
}

fn bare(name: &str) -> &str {
    name.split_once('/').map_or(name, |(_, n)| n)
}

/// Every package checked, under its group's header.
pub fn build(pl: &PkgList) -> Checklist {
    let mut rows = Vec::new();
    for g in pl.list.groups.iter().filter(|g| !g.packages.is_empty()) {
        rows.push(Row::Header(g.name.clone()));
        rows.extend(g.packages.iter().map(|p| Row::Item(Item::new(p.name.clone(), p.bare(), true))));
    }
    Checklist::new(rows)
}

/// The "installed" tags and the add-on rule: an add-on can only be picked while its parent
/// app is picked or already installed. Otherwise it's unchecked in effect and greyed out
/// ("needs zotero"); checking the parent again brings back its own choice.
pub fn apply(picker: &mut Checklist, pl: &PkgList, installed: &Installed) {
    for p in pl.list.packages() {
        let parent_ok = p.requires.as_ref().is_none_or(|r| installed.has(r) || picker.item(r).is_some_and(Item::selected));
        let Some(it) = picker.item_mut(&p.name) else { continue };
        it.tag = installed.has(&p.name).then(|| "installed".to_string());
        it.enabled = parent_ok;
        it.note = (!parent_ok).then(|| format!("needs {}", bare(p.requires.as_deref().unwrap_or(""))));
    }
    picker.settle();
}

fn state(app: &mut App) -> Option<&mut PackagesState> {
    match &mut app.screen {
        Screen::Packages(s) => Some(s),
        Screen::Auto(w) => w.apps_mut(),
        _ => None,
    }
}

/// Bring the picker up to date with the list and the installed set; asks for the installed
/// set when it isn't known yet. Called when the screen opens and when either arrives.
pub fn sync(app: &mut App) -> Option<Command> {
    let PkgState::Ready(pl) = app.packages.clone() else { return None };
    let installed = app.installed.clone();
    let want_installed = installed == Installed::Unknown;
    let st = state(app)?;
    if !st.for_list.as_ref().is_some_and(|l| Arc::ptr_eq(l, &pl)) {
        st.picker = Some(build(&pl));
        st.for_list = Some(pl.clone());
    }
    if let Some(p) = &mut st.picker {
        apply(p, &pl, &installed);
    }
    if want_installed {
        app.installed = Installed::Loading;
        return Some(Command::LoadInstalled(pl.list.packages().map(|p| p.name.clone()).collect()));
    }
    None
}

/// Lines under the source line: the "+N / −M vs built-in" note and the warnings count.
fn notes(theme: &Theme, state: &PkgState) -> Vec<Line<'static>> {
    let PkgState::Ready(pl) = state else { return Vec::new() };
    let mut lines = Vec::new();
    let d = if pl.is_builtin() { Default::default() } else { pl.diff() };
    if !d.is_empty() {
        let names: Vec<String> = d.added.iter().map(|n| format!("+{n}")).chain(d.removed.iter().map(|n| format!("−{n}"))).collect();
        lines.push(Line::styled(sep(theme, &format!("  {}: {}", d.summary(), names.join(" "))), theme.bold()));
    }
    if !pl.list.warnings.is_empty() {
        lines.push(Line::styled(format!("{} {} warning(s), see Details", theme.sym.warn, pl.list.warnings.len()), theme.warn()));
    }
    lines
}

/// The highlighted package (or group), then where the list came from.
fn details_lines(theme: &Theme, app: &App, st: &PackagesState, pl: &PkgList) -> Vec<Line<'static>> {
    let field = |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<12}"), theme.bold()), Span::raw(v)]);
    let mut lines = Vec::new();
    let picker = st.picker.as_ref();
    match picker.and_then(Checklist::current).and_then(|it| pl.list.packages().find(|p| p.name == it.key).map(|p| (it, p))) {
        Some((it, p)) => {
            lines.push(Line::styled(p.name.clone(), theme.title()));
            if let Some(d) = &p.description {
                lines.push(Line::raw(d.clone()));
            }
            let group = pl.list.groups.iter().find(|g| g.packages.iter().any(|x| x.name == p.name)).map(|g| g.name.clone()).unwrap_or_default();
            lines.push(field("Group:", group));
            let status = match &app.installed {
                Installed::Known(s) if s.contains(&p.name) => "installed (installing again changes nothing)".to_string(),
                Installed::Known(_) => "not installed".to_string(),
                _ => format!("checking{}", theme.sym.ellipsis),
            };
            lines.push(field("Status:", status));
            if let Some(f) = &p.fallback {
                lines.push(field("Fallback:", format!("{f}, when {} isn't in the repositories", p.name)));
            }
            if let Some(x) = &p.replaces {
                lines.push(field("Replaces:", format!("{x}, if installed and {} declares the conflict", p.name)));
            }
            if let Some(r) = &p.requires {
                lines.push(field("Add-on for:", r.clone()));
                if !it.enabled {
                    lines.push(Line::styled(format!("Check {} to pick this add-on.", bare(r)), theme.warn()));
                }
            }
            lines.push(field("Changes:", "installs the package (pacman, else yay as your user)".into()));
            lines.push(field("Needs root:", "yes (kast already runs as root)".into()));
        }
        None => match picker.and_then(|c| match c.rows.get(c.cursor) {
            Some(Row::Header(h)) => Some(h.clone()),
            _ => None,
        }) {
            Some(h) => {
                lines.push(Line::styled(h, theme.title()));
                lines.push(Line::raw("Space checks or unchecks the whole group."));
            }
            None => lines.push(Line::raw("Pick the packages to install.")),
        },
    }
    lines.push(Line::raw(""));
    match &pl.source {
        Source::Online { .. } => lines.push(Line::styled("List downloaded from kud3n013/kast's main branch.", theme.dim())),
        Source::BuiltIn(Reason::Offline) => lines.push(Line::styled("The list built into this kast (--offline).", theme.dim())),
        Source::BuiltIn(r) => {
            lines.push(Line::raw("The list built into this kast, because:"));
            lines.push(Line::styled(format!("  {}", sep(theme, &r.to_string())), theme.warn()));
        }
        Source::Local(p) => lines.push(Line::styled(format!("List read from {} (--packages-file).", p.display()), theme.dim())),
    }
    for w in &pl.list.warnings {
        lines.push(Line::styled(format!("{} {w}", theme.sym.warn), theme.warn()));
    }
    if !pl.is_builtin() {
        let d = pl.diff();
        if !d.is_empty() {
            lines.push(Line::styled(sep(theme, &d.summary()), theme.bold()));
            lines.extend(d.added.iter().map(|n| Line::styled(format!("  + {n}"), theme.ok())));
            lines.extend(d.removed.iter().map(|n| Line::styled(sep(theme, &format!("  − {n}")), theme.error())));
        }
    }
    lines
}

/// (source, list, details) areas: details beside the list from 100 columns, below it otherwise.
fn layout(body: Rect, top_rows: u16) -> (Rect, Rect, Rect) {
    let [top, rest] = Layout::vertical([Constraint::Length(2 + top_rows), Constraint::Min(3)]).areas(body);
    if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Min(50), Constraint::Length(48)]).areas(rest);
        (top, l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(rest);
        (top, l, r)
    }
}

fn areas(app: &App) -> (Rect, Rect, Rect) {
    layout(app.body_area(), 1 + notes(&app.theme, &app.packages).len() as u16)
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &PackagesState) {
    let theme = &app.theme;
    let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
    let notes = notes(theme, &app.packages);
    let (top, list_area, details_area) = layout(body, 1 + notes.len() as u16);
    let mut src = source_line(theme, &app.packages, spinner);
    if matches!(app.installed, Installed::Loading) {
        src.spans.push(Span::styled(format!(" · {spinner} checking what's installed"), theme.dim()));
    }
    let width = top.width.saturating_sub(2) as usize;
    let mut top_lines = vec![Line::from(ellipsize(&src.to_string(), width, theme.sym.ellipsis)).style(src.spans.get(1).map(|s| s.style).unwrap_or_default())];
    top_lines.extend(notes.into_iter().map(|l| Line::styled(ellipsize(&l.to_string(), width, theme.sym.ellipsis), l.style)));
    f.render_widget(Paragraph::new(Text::from(top_lines)).block(block(theme, " Package list ")), top);

    let title = if st.auto { "Apps to install" } else { "Install packages" };
    let (PkgState::Ready(pl), Some(picker)) = (&app.packages, &st.picker) else {
        let wait = "The list is being downloaded from kud3n013/kast. If that fails within 15 seconds, kast uses its built-in list and says why.";
        f.render_widget(Paragraph::new(wait).wrap(Wrap { trim: true }).block(block(theme, format!(" {title} "))), list_area);
        f.render_widget(Paragraph::new("").block(block(theme, " Details ")), details_area);
        return;
    };
    picker.view(f, list_area, theme, title);
    f.render_widget(Paragraph::new(Text::from(details_lines(theme, app, st, pl))).wrap(Wrap { trim: false }).block(block(theme, " Details ")), details_area);
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let page = areas(app).1.height.saturating_sub(2) as usize;
    let retry_ok = matches!(&app.packages, PkgState::Ready(pl) if pl.reason().is_some_and(|r| *r != Reason::Offline));
    let st = state(app)?;
    let auto = st.auto;
    if k.code == KeyCode::Char('r') {
        if retry_ok {
            app.packages = PkgState::Fetching;
            return Some(Command::FetchPackages);
        }
        return None;
    }
    let Some(picker) = &mut st.picker else {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return back(app, auto),
            KeyCode::Enter => app.toast(ToastKind::Info, "Still fetching the package list; it shows here first, before anything is installed"),
            _ => {}
        }
        return None;
    };
    match picker.key(&k, page) {
        Outcome::Confirm => return confirm(app),
        Outcome::Back => return back(app, auto),
        Outcome::Handled => {}
        Outcome::Ignored if k.code == KeyCode::Char('q') => return back(app, auto),
        Outcome::Ignored => return None,
    }
    resync(app);
    None
}

/// Re-apply the add-on rule after a toggle.
fn resync(app: &mut App) {
    let PkgState::Ready(pl) = app.packages.clone() else { return };
    let installed = app.installed.clone();
    if let Some(p) = state(app).and_then(|s| s.picker.as_mut()) {
        apply(p, &pl, &installed);
    }
}

fn back(app: &mut App, auto: bool) -> Option<Command> {
    if auto {
        super::wizard::back(app);
    } else {
        app.screen = Screen::Menu;
    }
    None
}

fn confirm(app: &mut App) -> Option<Command> {
    let PkgState::Ready(pl) = app.packages.clone() else { return None };
    let installed = app.installed.clone();
    let theme = app.theme.clone();
    let st = state(app)?;
    if st.auto {
        return super::wizard::next(app);
    }
    let picker = st.picker.as_ref()?;
    let chosen = picker.selected_keys();
    if chosen.is_empty() {
        app.toast(ToastKind::Error, "No packages selected: check some, or press a to check all");
        return None;
    }
    let (n, m) = picker.count();
    let left_out: Vec<String> = picker.items().filter(|i| !i.selected()).map(|i| i.label.clone()).collect();
    let already = chosen.iter().filter(|c| installed.has(c)).count();
    let mut details = vec![sep(&theme, &format!("List: {}", pl.source_line()))];
    if !left_out.is_empty() {
        details.push(format!("Left out: {}", left_out.join(", ")));
    }
    if already > 0 {
        details.push(format!("{already} of them are installed already (nothing changes for those)."));
    }
    details.push("Anything pacman can't install is retried with yay as your user.".into());
    let selection = st.selection();
    app.modal = Some(Modal {
        kind: ModalKind::Confirm(Box::new(JobKind::Packages(pl, selection))),
        title: "Install packages".into(),
        text: format!("Install {n} of {m} packages?"),
        details,
        yes: "Install",
        no: "Back",
        yes_selected: true,
    });
    None
}

pub fn mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    let list_area = areas(app).1;
    let st = state(app)?;
    let picker = st.picker.as_mut()?;
    match m.kind {
        MouseEventKind::ScrollUp => picker.step(false),
        MouseEventKind::ScrollDown => picker.step(true),
        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) if picker.click(list_area, m.row) => resync(app),
        _ => {}
    }
    None
}

pub fn hints(app: &App) -> &'static [(&'static str, &'static str)] {
    match &app.packages {
        PkgState::Ready(pl) if pl.reason().is_some_and(|r| *r != Reason::Offline) => &HINTS_RETRY,
        _ => &HINTS,
    }
}

pub const HINTS: [(&str, &str); 7] = [("Space", "toggle"), ("Enter", "continue"), ("Esc", "back"), ("?", "help"), ("a", "all"), ("n", "none"), ("i", "invert")];
pub const HINTS_RETRY: [(&str, &str); 8] =
    [("Space", "toggle"), ("Enter", "continue"), ("Esc", "back"), ("?", "help"), ("r", "retry download"), ("a", "all"), ("n", "none"), ("i", "invert")];

pub const HELP: [(&str, &str); 9] = [
    checklist::HELP[0],
    checklist::HELP[1],
    checklist::HELP[2],
    checklist::HELP[3],
    checklist::HELP[4],
    ("Enter", "Continue with the checked packages"),
    ("Esc  q", "Back (your choices are kept)"),
    ("r", "Download the list again (after a fallback)"),
    ("Add-ons", "Need their app checked or already installed"),
];
