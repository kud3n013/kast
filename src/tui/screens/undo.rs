//! Undo changes (menu `u`) and uninstall apps (menu `x`). Undo: the runs kast recorded,
//! newest first → the changes of one, on a checklist (everything undoable checked, except
//! deleting your files and reinstalling old packages) → a preview of exactly what runs, in
//! order → a job. Uninstall: the list's installed apps (none checked) → preview → job.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::checklist::{self, Checklist, Item as Check, Outcome, Row};
use crate::journal::{Run, Status};
use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind, start_job};
use crate::tui::job::JobKind;
use crate::tui::theme::Theme;
use crate::tui::widgets::{block, ellipsize};
use crate::undo::{self, InstalledApp, Item, View};

/// What the loader found for the undo screen.
#[derive(Debug, Clone, Default)]
pub struct UndoData {
    pub runs: Vec<Run>,
    pub view: View,
    /// Journal lines that couldn't be read, unsafe files
    pub problems: Vec<String>,
    /// Bytes of file copies kept
    pub stored: u64,
    /// Short diffs of files changed since kast, by (run, label)
    pub diffs: HashMap<(String, String), Vec<String>>,
    /// The journal couldn't be read at all
    pub error: Option<String>,
    pub keep: usize,
}

/// What the loader found for the uninstall screen.
#[derive(Debug, Clone, Default)]
pub struct UninstallData {
    pub apps: Vec<InstalledApp>,
    pub view: View,
}

/// Read the journal and look at the system (off the UI thread).
pub fn load_undo() -> UndoData {
    let dir = crate::journal::state_dir();
    let (runs, problems) = match undo::load_runs(&dir) {
        Ok(x) => x,
        Err(e) => return UndoData { error: Some(e.to_string()), ..Default::default() },
    };
    let listed = undo::listed(&runs);
    let view = undo::gather(&listed);
    let mut diffs = HashMap::new();
    for r in &listed {
        for it in undo::plan(r, &view) {
            if it.warning.as_deref().is_some_and(|w| w.contains("changed since")) {
                diffs.insert((it.run.clone(), it.label.clone()), undo::diff_lines(&dir, &it, 8));
            }
        }
    }
    UndoData { stored: undo::stored_size(&dir), keep: undo::keep_runs(&dir), runs, view, problems, diffs, error: None }
}

/// The list's installed apps and what needs them (off the UI thread).
pub fn load_uninstall(list: &crate::pkglist::Parsed) -> UninstallData {
    let installed: HashSet<String> = crate::ctx::output("pacman", &["-Qq"]).map(|o| o.lines().map(String::from).collect()).unwrap_or_default();
    let runs = undo::load_runs(&crate::journal::state_dir()).map(|(r, _)| r).unwrap_or_default();
    let apps = undo::installed_apps(list, &installed, &runs);
    let names: Vec<String> = apps.iter().map(|a| a.name.clone()).collect();
    UninstallData { view: undo::uninstall_view(&names), apps }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Undo,
    Uninstall,
}

#[derive(Debug, Clone)]
pub enum Stage {
    Loading,
    /// Undo: the runs
    Runs,
    /// Undo: the changes of runs[i]; uninstall: the apps
    Pick,
    /// What will run, with Start / Back
    Preview {
        items: Vec<Item>,
        start: bool,
    },
}

#[derive(Debug, Clone)]
pub struct UndoState {
    pub mode: Mode,
    pub stage: Stage,
    pub data: UndoData,
    pub apps: UninstallData,
    pub run: usize,
    pub picker: Checklist,
    /// The plan behind `picker` (undo), by checklist key
    pub items: Vec<Item>,
}

impl UndoState {
    pub fn new(mode: Mode) -> Self {
        UndoState {
            mode,
            stage: Stage::Loading,
            data: UndoData::default(),
            apps: UninstallData::default(),
            run: 0,
            picker: Checklist::default(),
            items: Vec::new(),
        }
    }

    pub fn loading(&self) -> bool {
        matches!(self.stage, Stage::Loading)
    }

    fn listed(&self) -> Vec<&Run> {
        undo::listed(&self.data.runs)
    }

    /// The checklist of a run's changes, grouped by step.
    fn pick_run(&mut self, i: usize) {
        let Some(run) = self.listed().get(i).map(|r| (*r).clone()) else { return };
        self.run = i;
        self.items = undo::plan(&run, &self.data.view);
        let mut rows = Vec::new();
        let mut step = None;
        for (k, it) in self.items.iter().enumerate() {
            if step.as_deref() != Some(it.step.as_str()) {
                step = Some(it.step.clone());
                rows.push(Row::Header(step_title(&it.step)));
            }
            let mut c = Check::new(k.to_string(), it.label.clone(), it.default_on && it.undoable);
            c.enabled = it.undoable;
            if it.undoable {
                c.tag = it.warning.clone();
            } else {
                c.note = it.warning.clone();
            }
            rows.push(Row::Item(c));
        }
        self.picker = Checklist::new(rows);
        self.stage = Stage::Pick;
    }

    /// The uninstall checklist: the installed apps, none checked.
    fn pick_apps(&mut self) {
        let mut rows = Vec::new();
        let mut group = None;
        for a in &self.apps.apps {
            if group.as_deref() != Some(a.group.as_str()) {
                group = Some(a.group.clone());
                rows.push(Row::Header(a.group.clone()));
            }
            let mut c = Check::new(a.name.clone(), a.name.clone(), false);
            c.tag = Some(match &a.by_kast {
                Some(d) => format!("installed by kast ({d})"),
                None => "installed before kast / outside kast".into(),
            });
            rows.push(Row::Item(c));
        }
        self.picker = Checklist::new(rows);
        self.stage = Stage::Pick;
    }

    /// The checked items, in plan order.
    fn chosen(&self) -> Vec<Item> {
        match self.mode {
            Mode::Undo => self.picker.selected_keys().iter().filter_map(|k| k.parse::<usize>().ok()).filter_map(|k| self.items.get(k).cloned()).collect(),
            Mode::Uninstall => undo::uninstall_plan(&self.picker.selected_keys(), &self.apps.view),
        }
    }
}

fn step_title(step: &str) -> String {
    match step {
        "" => "Other".into(),
        "repo" => "kud3n's repository".into(),
        s => {
            let mut c = s.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

fn state(app: &mut App) -> Option<&mut UndoState> {
    match &mut app.screen {
        Screen::Undo(s) => Some(s),
        _ => None,
    }
}

pub fn on_undo(app: &mut App, data: UndoData) {
    if let Some(st) = state(app) {
        st.data = data;
        st.stage = Stage::Runs;
        st.run = st.run.min(st.listed().len().saturating_sub(1));
    }
}

pub fn on_uninstall(app: &mut App, data: UninstallData) {
    if let Some(st) = state(app) {
        st.apps = data;
        st.pick_apps();
    }
}

fn page(app: &App) -> usize {
    app.body_area().height.saturating_sub(2) as usize
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let page = page(app);
    let st = state(app)?;
    match &mut st.stage {
        Stage::Loading => {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                app.screen = Screen::Menu;
            }
            None
        }
        Stage::Runs => {
            let n = st.listed().len();
            match k.code {
                KeyCode::Up | KeyCode::Char('k') if n > 0 => st.run = (st.run + n - 1) % n,
                KeyCode::Down | KeyCode::Char('j') if n > 0 => st.run = (st.run + 1) % n,
                KeyCode::Home | KeyCode::Char('g') => st.run = 0,
                KeyCode::End | KeyCode::Char('G') => st.run = n.saturating_sub(1),
                KeyCode::Enter if n > 0 => {
                    let i = st.run;
                    st.pick_run(i);
                }
                KeyCode::Char('p') => return ask_prune(app),
                KeyCode::Esc | KeyCode::Char('q') => app.screen = Screen::Menu,
                _ => {}
            }
            None
        }
        Stage::Pick => match st.picker.key(&k, page) {
            Outcome::Confirm => {
                let items = st.chosen();
                if !items.iter().any(|i| i.undoable) {
                    let what = if st.mode == Mode::Undo { "Nothing checked to undo" } else { "No app checked (or each is needed by something else)" };
                    app.toast(ToastKind::Error, what);
                    return None;
                }
                st.stage = Stage::Preview { items, start: true };
                None
            }
            Outcome::Back => {
                back_from_pick(app);
                None
            }
            Outcome::Ignored if k.code == KeyCode::Char('q') => {
                back_from_pick(app);
                None
            }
            _ => None,
        },
        Stage::Preview { items, start } => match k.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h' | 'l') => {
                *start = !*start;
                None
            }
            KeyCode::Enter if *start => run(app),
            KeyCode::Char('s') => run(app),
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                let _ = items;
                st.stage = Stage::Pick;
                None
            }
            _ => None,
        },
    }
}

fn back_from_pick(app: &mut App) {
    if let Some(st) = state(app) {
        match st.mode {
            Mode::Undo => st.stage = Stage::Runs,
            Mode::Uninstall => app.screen = Screen::Menu,
        }
    }
}

fn run(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    let Stage::Preview { items, .. } = &st.stage else { return None };
    let items: Vec<Item> = items.iter().filter(|i| i.undoable).cloned().collect();
    let kind = match st.mode {
        Mode::Undo => JobKind::Undo(items),
        Mode::Uninstall => JobKind::Uninstall(items),
    };
    Some(start_job(app, kind))
}

/// `p`: delete the file copies of old or fully undone runs, after asking.
fn ask_prune(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    let ids = undo::prunable(&st.data.runs, st.data.keep);
    let keep = st.data.keep;
    if ids.is_empty() {
        app.toast(ToastKind::Info, format!("Nothing to prune (keeping the last {keep} runs)"));
        return None;
    }
    app.modal = Some(Modal {
        kind: ModalKind::Prune(ids.clone()),
        title: "Prune".into(),
        text: format!("Delete the stored file copies of {} run(s)?", ids.len()),
        details: vec![
            format!("Runs older than the last {keep} and fully undone runs: {}", ids.join(", ")),
            "Their file changes can't be undone afterwards.".into(),
        ],
        yes: "Delete",
        no: "Keep",
        yes_selected: false,
    });
    None
}

pub fn mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    let body = app.body_area();
    let st = state(app)?;
    if let Stage::Pick = st.stage {
        let (list, _) = layout(body);
        match m.kind {
            MouseEventKind::ScrollUp => st.picker.step(false),
            MouseEventKind::ScrollDown => st.picker.step(true),
            MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                st.picker.click(list, m.row);
            }
            _ => {}
        }
    }
    None
}

// --- Drawing -------------------------------------------------------------------------------

fn layout(body: Rect) -> (Rect, Rect) {
    if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Min(50), Constraint::Length(52)]).areas(body);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(body);
        (l, r)
    }
}

fn run_line(r: &Run, items: &[Item], theme: &Theme) -> Line<'static> {
    let (n, m) = undo::counts(r, items);
    let when = r.ts.get(..16).unwrap_or(&r.ts).replace('T', " ");
    let mut spans =
        vec![Span::raw(format!("{when}  kast {}  {}", r.version, r.steps.join(", "))), Span::styled(format!("  {n} changes, {m} undoable"), theme.dim())];
    if r.changes.iter().any(|c| c.status == Status::Incomplete) {
        spans.push(Span::styled(format!("  {} incomplete", theme.sym.warn), theme.warn()));
    }
    if r.changes.iter().all(|c| c.status == Status::Undone) {
        spans.push(Span::styled("  undone", theme.dim()));
    }
    Line::from(spans)
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &UndoState) {
    let theme = &app.theme;
    let (list_area, details) = layout(body);
    let title = if st.mode == Mode::Undo { "Undo changes" } else { "Uninstall apps" };
    match &st.stage {
        Stage::Loading => {
            let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
            let what = if st.mode == Mode::Undo { "Reading what kast changed" } else { "Looking for installed apps" };
            f.render_widget(Paragraph::new(format!("{spinner} {what}{}", theme.sym.ellipsis)).block(block(theme, format!(" {title} "))), body);
        }
        Stage::Runs => {
            let runs = st.listed();
            let mut lines = Vec::new();
            if let Some(e) = &st.data.error {
                lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error()));
            }
            let items: Vec<ListItem> = runs.iter().map(|r| ListItem::new(run_line(r, &undo::plan(r, &st.data.view), theme))).collect();
            if runs.is_empty() && st.data.error.is_none() {
                lines.push(Line::raw("kast hasn't recorded any changes yet."));
            }
            if lines.is_empty() {
                let list = List::new(items)
                    .block(block(theme, format!(" {title}: runs, newest first ")))
                    .highlight_style(theme.selected())
                    .highlight_symbol(format!("{} ", theme.sym.pointer));
                let mut ls = ListState::default().with_selected(Some(st.run));
                f.render_stateful_widget(list, list_area, &mut ls);
            } else {
                f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, format!(" {title} "))), list_area);
            }
            let mut d = Vec::new();
            if let Some(r) = runs.get(st.run) {
                d.push(Line::styled(r.id.clone(), theme.title()));
                d.push(Line::raw(format!("On {} for {}, kast {}", r.host, r.user, r.version)));
                d.push(Line::raw(format!("Steps: {}", r.steps.join(", "))));
                let incomplete = r.changes.iter().filter(|c| c.status == Status::Incomplete).count();
                if incomplete > 0 {
                    d.push(Line::styled(
                        format!("{} {incomplete} change(s) were interrupted; undo reverts them from their before-state.", theme.sym.warn),
                        theme.warn(),
                    ));
                }
                if r.changes.iter().any(|c| c.newer) {
                    d.push(Line::styled("Some changes come from a newer kast and can't be undone here.", theme.warn()));
                }
                d.push(Line::raw(""));
            }
            d.push(Line::styled(format!("File copies kept: {} (last {} runs; p prunes)", undo::human(st.data.stored), st.data.keep), theme.dim()));
            for p in &st.data.problems {
                d.push(Line::styled(format!("{} {p}", theme.sym.warn), theme.warn()));
            }
            f.render_widget(Paragraph::new(Text::from(d)).wrap(Wrap { trim: false }).block(block(theme, " Details ")), details);
        }
        Stage::Pick => {
            let t = if st.mode == Mode::Undo { "Changes to undo" } else { "Apps to remove" };
            st.picker.view(f, list_area, theme, t);
            f.render_widget(Paragraph::new(Text::from(pick_details(st, theme))).wrap(Wrap { trim: false }).block(block(theme, " Details ")), details);
        }
        Stage::Preview { items, start } => preview_view(f, body, theme, st, items, *start),
    }
}

fn pick_details(st: &UndoState, theme: &Theme) -> Vec<Line<'static>> {
    let mut d = Vec::new();
    let Some(cur) = st.picker.current() else {
        d.push(Line::raw("Space on a header checks or unchecks all of it."));
        return d;
    };
    match st.mode {
        Mode::Undo => {
            let Some(it) = cur.key.parse::<usize>().ok().and_then(|k| st.items.get(k)) else { return d };
            d.push(Line::styled(it.label.clone(), theme.title()));
            if let Some(w) = &it.warning {
                d.push(Line::styled(format!("{} {w}", theme.sym.warn), theme.warn()));
            }
            if let Some(diff) = st.data.diffs.get(&(it.run.clone(), it.label.clone())) {
                d.push(Line::raw("Since kast wrote it (- kast's before, + now):"));
                d.extend(diff.iter().map(|l| Line::styled(format!("  {l}"), theme.dim())));
                d.push(Line::raw("Checked: restore anyway. Unchecked: keep the current file."));
            }
        }
        Mode::Uninstall => {
            d.push(Line::styled(cur.label.clone(), theme.title()));
            if let Some(t) = &cur.tag {
                d.push(Line::raw(t.clone()));
            }
            if let Some(p) = st.apps.view.installed.get(&cur.key)
                && !p.required_by.is_empty()
            {
                d.push(Line::styled(format!("Needed by: {} (kept unless those go too)", p.required_by.join(", ")), theme.warn()));
            }
            d.push(Line::raw("Removed with pacman -Rns (its unneeded dependencies too)."));
        }
    }
    d
}

fn preview_view(f: &mut Frame, body: Rect, theme: &Theme, st: &UndoState, items: &[Item], start: bool) {
    let [text, buttons] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(body);
    let mut lines = vec![Line::styled("In this order:", theme.bold())];
    for (i, it) in items.iter().enumerate() {
        let style = if it.undoable { ratatui::style::Style::new() } else { theme.dim() };
        let mut spans = vec![Span::styled(format!("  {:>2}. ", i + 1), theme.bold()), Span::styled(it.label.clone(), style)];
        if let Some(w) = &it.warning {
            spans.push(Span::styled(format!("  {} {w}", theme.sym.warn), theme.warn()));
        }
        lines.push(Line::from(spans));
    }
    if st.mode == Mode::Undo && items.iter().any(|i| matches!(&i.act, undo::Act::RestoreFile { path, .. } if path.starts_with("/etc/pam.d"))) {
        lines.push(Line::raw(""));
        lines.push(Line::styled("PAM files are checked after they're restored; if sudo would break, they're put back.", theme.dim()));
    }
    let title = if st.mode == Mode::Undo { " Undo: preview " } else { " Uninstall: preview " };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, title)), text);
    let button = |label: &str, on: bool, style| {
        Span::styled(format!(" {} {label} ", if on { theme.sym.pointer } else { " " }), if on { theme.selected() } else { style })
    };
    let line = Line::from(vec![button("Start", start, theme.ok()), Span::raw("   "), button("Back", !start, ratatui::style::Style::new())]);
    f.render_widget(Paragraph::new(line).block(block(theme, "")), buttons);
    let _ = ellipsize;
}

pub fn hints(st: &UndoState) -> &'static [(&'static str, &'static str)] {
    match st.stage {
        Stage::Loading => &[("Esc", "back")],
        Stage::Runs => &[("↑/↓", "move"), ("Enter", "open"), ("p", "prune"), ("Esc", "back"), ("?", "help")],
        Stage::Pick => &[("Space", "toggle"), ("Enter", "preview"), ("Esc", "back"), ("?", "help"), ("a", "all"), ("n", "none"), ("i", "invert")],
        Stage::Preview { .. } => &[("Enter", "choose"), ("←/→", "Start/Back"), ("s", "start"), ("Esc", "back")],
    }
}

pub const HELP: [(&str, &str); 9] = [
    ("Runs", "↑/↓ move, Enter opens a run, p prunes old file copies (asks)"),
    checklist::HELP[0],
    checklist::HELP[2],
    checklist::HELP[3],
    ("Enter", "Preview exactly what will run, in order"),
    ("Preview", "←/→ Start / Back, s starts the job"),
    ("Greyed out", "Can't be undone (no snapshot, newer kast, still needed)"),
    ("Esc  q", "Back one stage"),
    ("?", "This help"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Change;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;
    use crate::undo::PkgNow;
    use serde_json::json;

    fn change(seq: u64, kind: &str, step: &str, status: Status, data: serde_json::Value) -> Change {
        Change { seq, kind: kind.into(), step: step.into(), status, data, newer: false }
    }

    fn data() -> UndoData {
        let pam = json!({ "path": "/etc/pam.d/sudo", "before": { "sha256": "p0", "mode": 420, "uid": 0, "gid": 0 }, "after_sha256": "p1" });
        let run = Run {
            id: "20261017-101500-a1b2c3".into(),
            ts: "2026-10-17T10:15:00+02:00".into(),
            version: "0.4.0".into(),
            host: "archbox".into(),
            user: "p3ter".into(),
            steps: vec!["upgrade".into(), "packages".into(), "services".into(), "import".into()],
            changes: vec![
                change(1, "upgrade", "upgrade", Status::Done, json!({ "snapshot": { "tool": "snapper", "pre": 41, "post": 42 } })),
                change(2, "pkg_install", "packages", Status::Done, json!({ "name": "zotero", "version": "7.0", "reason": "explicit" })),
                change(3, "pkg_install", "packages", Status::Done, json!({ "name": "libdep", "version": "1", "reason": "dependency" })),
                change(4, "file_write", "services", Status::Incomplete, pam),
                change(5, "service_enable", "services", Status::Done, json!({ "unit": "valkey", "was_enabled": false, "was_active": false })),
                change(6, "import", "import", Status::Done, json!({ "archive": "/b/all.zip", "apps": ["zen"], "snapshot": null })),
            ],
        };
        let older = Run {
            id: "20261010-090000-ffeedd".into(),
            ts: "2026-10-10T09:00:00+02:00".into(),
            version: "0.3.0".into(),
            host: "archbox".into(),
            user: "p3ter".into(),
            steps: vec!["export".into()],
            changes: vec![change(1, "export", "export", Status::Done, json!({ "file": "/media/data/backups/all.zip" }))],
        };
        let mut view = View::default();
        view.installed.insert("zotero".into(), PkgNow { explicit: true, required_by: vec!["zotero-ocr".into()], repo: String::new() });
        view.installed.insert("libdep".into(), PkgNow::default());
        view.files.insert("/etc/pam.d/sudo".into(), Some("p1".into()));
        UndoData { runs: vec![older, run], view, problems: vec![], stored: 48_128, diffs: HashMap::new(), error: None, keep: 10 }
    }

    fn opened(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        assert!(matches!(update(&mut a, key(KeyCode::Char('u'))), Some(Command::LoadUndo)));
        update(&mut a, Event::Loaded(Loaded::Undo(Box::new(data()))));
        a
    }

    fn st(a: &App) -> &UndoState {
        match &a.screen {
            Screen::Undo(s) => s,
            _ => panic!("not on the undo screen"),
        }
    }

    #[test]
    fn runs_changes_preview_job() {
        let mut a = opened(80, 24);
        assert_eq!(st(&a).listed()[0].id, "20261017-101500-a1b2c3", "newest first");
        update(&mut a, key(KeyCode::Enter));
        let p = &st(&a).picker;
        // zotero kept (needed), libdep removed, PAM (incomplete) restored, valkey disabled; import and upgrade greyed
        assert_eq!(p.count(), (3, 6));
        let import = p.items().find(|i| i.label.starts_with("Restore of all.zip")).unwrap();
        assert!(!import.enabled && import.note.as_deref() == Some("not undoable (no snapshot was taken)"));
        update(&mut a, key(KeyCode::Enter));
        let Stage::Preview { items, .. } = &st(&a).stage else { panic!("preview") };
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Remove libdep", "Disable and stop valkey", "Restore /etc/pam.d/sudo"]);
        let Some(Command::Start(JobKind::Undo(items))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the job") };
        assert_eq!(items.len(), 3);
        // Esc goes back stage by stage
        let mut a = opened(80, 24);
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Char('n')));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Error);
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(st(&a).stage, Stage::Runs));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn prune_asks_first() {
        let mut a = opened(80, 24);
        if let Screen::Undo(s) = &mut a.screen {
            s.data.keep = 1;
        }
        update(&mut a, key(KeyCode::Char('p')));
        let m = a.modal.as_ref().unwrap();
        assert!(matches!(&m.kind, ModalKind::Prune(ids) if ids == &["20261010-090000-ffeedd".to_string()]));
        assert!(!m.yes_selected);
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::PruneUndo(_))));
    }

    #[test]
    fn uninstall_lists_installed_apps_unchecked() {
        let mut a = app(80, 24);
        assert!(matches!(update(&mut a, key(KeyCode::Char('x'))), Some(Command::LoadUninstall)));
        let apps = vec![
            InstalledApp { name: "zotero".into(), group: "Official repositories".into(), by_kast: None },
            InstalledApp { name: "steam".into(), group: "Official repositories".into(), by_kast: Some("2026-10-17".into()) },
        ];
        let mut view = View::default();
        view.installed.insert("zotero".into(), PkgNow { explicit: true, required_by: vec!["zotero-ocr".into()], repo: String::new() });
        view.installed.insert("steam".into(), PkgNow::default());
        update(&mut a, Event::Loaded(Loaded::Uninstall(Box::new(UninstallData { apps, view }))));
        assert_eq!(st(&a).picker.count(), (0, 2), "nothing checked to start with");
        assert_eq!(st(&a).picker.item("steam").unwrap().tag.as_deref(), Some("installed by kast (2026-10-17)"));
        update(&mut a, key(KeyCode::Char('a')));
        update(&mut a, key(KeyCode::Enter));
        let Stage::Preview { items, .. } = &st(&a).stage else { panic!() };
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Keep zotero", "Remove steam"]);
        assert!(matches!(update(&mut a, key(KeyCode::Char('s'))), Some(Command::Start(JobKind::Uninstall(i))) if i.len() == 1));
    }

    #[test]
    fn snapshots() {
        for (w, h) in [(80, 24), (140, 40)] {
            let mut a = opened(w, h);
            insta::assert_snapshot!(format!("undo_runs_{w}x{h}"), render(&a));
            update(&mut a, key(KeyCode::Enter));
            update(&mut a, key(KeyCode::End));
            insta::assert_snapshot!(format!("undo_changes_{w}x{h}"), render(&a));
            update(&mut a, key(KeyCode::Enter));
            insta::assert_snapshot!(format!("undo_preview_{w}x{h}"), render(&a));

            let mut a = app(w, h);
            update(&mut a, key(KeyCode::Char('x')));
            let apps = vec![
                InstalledApp { name: "zotero".into(), group: "Official repositories".into(), by_kast: None },
                InstalledApp { name: "steam".into(), group: "Official repositories".into(), by_kast: Some("2026-10-17".into()) },
                InstalledApp { name: "beeper".into(), group: "kud3n's repository".into(), by_kast: Some("2026-10-17".into()) },
            ];
            update(&mut a, Event::Loaded(Loaded::Uninstall(Box::new(UninstallData { apps, view: View::default() }))));
            insta::assert_snapshot!(format!("uninstall_{w}x{h}"), render(&a));
        }
    }
}
