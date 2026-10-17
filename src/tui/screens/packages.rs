//! What the packages step will install, and from which copy of the list, shown before
//! menu option 1 (automatic run) or 4 (packages) starts. The list is fetched in the
//! background when the app opens; until it arrives this screen says so.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::pkglist::{PkgList, Reason, Source, rules};
use crate::tui::app::{App, Command, Screen, ToastKind, start_job};
use crate::tui::job::JobKind;
use crate::tui::theme::Theme;
use crate::tui::widgets::{block, ellipsize};

/// The package list for this session.
#[derive(Debug, Clone)]
pub enum PkgState {
    Fetching,
    Ready(Arc<PkgList>),
}

#[derive(Debug, Clone, Default)]
pub struct PackagesState {
    /// Confirming the automatic run (else menu option 4)
    pub auto: bool,
    pub scroll: usize,
}

impl PackagesState {
    pub fn title(&self) -> &'static str {
        if self.auto { "Automatic setup" } else { "Install packages" }
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

/// The grouped list, as lines.
fn list_lines(theme: &Theme, pl: &PkgList) -> Vec<Line<'static>> {
    let width = pl.list.packages().map(|p| p.name.chars().count()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for g in pl.list.groups.iter().filter(|g| !g.packages.is_empty()) {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(format!("{} ({})", g.name, g.packages.len()), theme.bold()));
        for p in &g.packages {
            let r = rules(p);
            let mut spans = vec![Span::raw(format!("  {:width$}", p.name))];
            if !r.is_empty() {
                spans.push(Span::styled(format!("  {}", sep(theme, &r)), theme.dim()));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn details_lines(theme: &Theme, st: &PackagesState, pl: &PkgList) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let what = if st.auto {
        "The automatic run upgrades the system, adds kud3n's repository, installs these packages, then sets up services."
    } else {
        "Installs these packages with pacman. Anything pacman can't install is retried with yay as your user, if kast's built-in list has it."
    };
    lines.push(Line::raw(what));
    lines.push(Line::raw(""));
    match &pl.source {
        Source::Online { .. } => lines.push(Line::raw("Downloaded from kud3n013/kast's main branch for this session.")),
        Source::BuiltIn(Reason::Offline) => lines.push(Line::raw("The list built into this kast (--offline).")),
        Source::BuiltIn(r) => {
            lines.push(Line::raw("The list built into this kast, because:"));
            lines.push(Line::styled(format!("  {}", sep(theme, &r.to_string())), theme.warn()));
        }
        Source::Local(p) => lines.push(Line::raw(format!("Read from {} (--packages-file).", p.display()))),
    }
    for w in &pl.list.warnings {
        lines.push(Line::styled(format!("{} {w}", theme.sym.warn), theme.warn()));
    }
    if !pl.is_builtin() {
        let d = pl.diff();
        lines.push(Line::raw(""));
        if d.is_empty() {
            lines.push(Line::styled("Same packages as the built-in list.", theme.dim()));
        } else {
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
        let [l, r] = Layout::horizontal([Constraint::Min(50), Constraint::Length(44)]).areas(rest);
        (top, l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(rest);
        (top, l, r)
    }
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

fn list_height(app: &App, body: Rect) -> usize {
    layout(body, 1 + notes(&app.theme, &app.packages).len() as u16).1.height.saturating_sub(2) as usize
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &PackagesState) {
    let theme = &app.theme;
    let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
    let notes = notes(theme, &app.packages);
    let (top, list_area, details_area) = layout(body, 1 + notes.len() as u16);
    let mut src = source_line(theme, &app.packages, spinner);
    if let Some(Span { content, .. }) = src.spans.get_mut(1) {
        *content = ellipsize(content, top.width.saturating_sub(6) as usize, theme.sym.ellipsis).into();
    }
    let width = top.width.saturating_sub(2) as usize;
    let mut top_lines = vec![src];
    top_lines.extend(notes.into_iter().map(|l| Line::styled(ellipsize(&l.to_string(), width, theme.sym.ellipsis), l.style)));
    f.render_widget(Paragraph::new(Text::from(top_lines)).block(block(theme, " Package list ")), top);

    let PkgState::Ready(pl) = &app.packages else {
        let wait = "The list is being downloaded from kud3n013/kast. If that fails within 15 seconds, kast uses its built-in list and says why.";
        f.render_widget(Paragraph::new(wait).wrap(Wrap { trim: true }).block(block(theme, format!(" {} ", st.title()))), list_area);
        f.render_widget(Paragraph::new("").block(block(theme, " Details ")), details_area);
        return;
    };
    let lines = list_lines(theme, pl);
    let max = lines.len().saturating_sub(list_height(app, body));
    let title = format!(" {} · {} packages ", st.title(), pl.list.count());
    f.render_widget(Paragraph::new(Text::from(lines)).scroll((st.scroll.min(max) as u16, 0)).block(block(theme, sep(theme, &title))), list_area);
    f.render_widget(Paragraph::new(Text::from(details_lines(theme, st, pl))).wrap(Wrap { trim: false }).block(block(theme, " Details ")), details_area);
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let page = list_height(app, app.body_area()).max(1);
    let total = match &app.packages {
        PkgState::Ready(pl) => list_lines(&app.theme, pl).len(),
        PkgState::Fetching => 0,
    };
    let Screen::Packages(st) = &mut app.screen else { return None };
    let max = total.saturating_sub(page);
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.screen = Screen::Menu,
        KeyCode::Up | KeyCode::Char('k') => st.scroll = st.scroll.min(max).saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => st.scroll = (st.scroll + 1).min(max),
        KeyCode::PageUp => st.scroll = st.scroll.min(max).saturating_sub(page),
        KeyCode::PageDown | KeyCode::Char(' ') => st.scroll = (st.scroll + page).min(max),
        KeyCode::Home | KeyCode::Char('g') => st.scroll = 0,
        KeyCode::End | KeyCode::Char('G') => st.scroll = max,
        KeyCode::Char('r') => {
            if matches!(&app.packages, PkgState::Ready(pl) if pl.reason().is_some_and(|r| *r != Reason::Offline)) {
                app.packages = PkgState::Fetching;
                return Some(Command::FetchPackages);
            }
        }
        KeyCode::Enter => {
            let auto = st.auto;
            let PkgState::Ready(pl) = &app.packages else {
                app.toast(ToastKind::Info, "Still fetching the package list; it shows here first, before anything is installed");
                return None;
            };
            let pl = pl.clone();
            return Some(start_job(app, if auto { JobKind::Auto(pl) } else { JobKind::Packages(pl) }));
        }
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

pub const HINTS: [(&str, &str); 4] = [("Enter", "start"), ("↑/↓", "scroll"), ("?", "help"), ("Esc", "back")];
pub const HINTS_RETRY: [(&str, &str); 5] = [("Enter", "start"), ("↑/↓", "scroll"), ("?", "help"), ("Esc", "back"), ("r", "retry download")];

pub const HELP: [(&str, &str); 7] = [
    ("Enter", "Start: install this list"),
    ("↑/↓  j/k", "Scroll the list"),
    ("PgUp/PgDn  Space", "Scroll a page"),
    ("Home/End  g/G", "Top / bottom"),
    ("r", "Download the list again (after a fallback)"),
    ("?", "This help"),
    ("Esc  q", "Back to the menu, nothing installed"),
];
