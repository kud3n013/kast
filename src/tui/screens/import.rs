//! Restore (and inspect) backups: a table of the archives and face models kas finds, a
//! manifest panel, a custom path field, then the restore options and a confirmation that
//! names exactly what gets overwritten.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};

use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind};
use crate::tui::theme::Theme;
use crate::tui::widgets::{Input, block, ellipsize, popup};
use crate::vault::import::ImportOpts;
use crate::vault::info::InfoReport;

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    pub tag: &'static str,
    pub size: String,
    pub modified: String,
}

impl Candidate {
    pub fn is_dat(&self) -> bool {
        crate::vault::is_dat(&self.path)
    }
}

/// Backups in the usual places, newest first (runs `du`, so off the UI thread).
pub fn load_candidates(zips_only: bool) -> Vec<Candidate> {
    let (zips, models) = crate::interactive::find_candidates();
    let models = if zips_only { Vec::new() } else { models };
    zips.into_iter()
        .chain(models)
        .map(|p| {
            let modified = p
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            Candidate {
                name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                tag: crate::interactive::candidate_tag(&p),
                size: crate::vault::format_bytes(&p),
                modified,
                path: p,
            }
        })
        .collect()
}

/// A picked file, as the runtime found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspected {
    pub info: InfoReport,
    /// What restoring each app of the archive would overwrite
    pub destinations: Vec<(String, Vec<String>)>,
}

/// Read a file the user pointed at: `Ok(None)` for a face model, the manifest otherwise.
pub fn inspect(path: &Path) -> Result<Option<Inspected>, String> {
    if !path.is_file() {
        return Err(format!("File not found: {}", path.display()));
    }
    if crate::vault::is_dat(path) {
        return Ok(None);
    }
    let info = crate::vault::info::read_info(path)?;
    let destinations = info.apps.iter().map(|a| (a.key.clone(), crate::vault::import::restore_destinations(&a.key))).collect();
    Ok(Some(Inspected { info, destinations }))
}

/// Tab completion of a path typed by the user: the completed text and the names that match.
pub fn complete_path(value: &str, home: &Path) -> (String, Vec<String>) {
    let expanded = match value.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.display()),
        None if value == "~" => format!("{}/", home.display()),
        None => value.to_string(),
    };
    let (dir, prefix) = match expanded.rfind('/') {
        Some(i) => (&expanded[..=i], &expanded[i + 1..]),
        None => ("./", expanded.as_str()),
    };
    let mut names: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let visible = !n.starts_with('.') || prefix.starts_with('.');
                    (visible && n.starts_with(prefix)).then(|| (n, e.path().is_dir()))
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let typed_dir = &value[..value.len() - prefix.len().min(value.len())];
    let completed = match names.as_slice() {
        [] => return (value.to_string(), Vec::new()),
        [(n, is_dir)] => format!("{typed_dir}{n}{}", if *is_dir { "/" } else { "" }),
        many => {
            let first = &many[0].0;
            let common = many.iter().fold(first.len(), |len, (n, _)| first.bytes().zip(n.bytes()).take(len).take_while(|(a, b)| a == b).count());
            format!("{typed_dir}{}", &first[..common])
        }
    };
    let shown = if names.len() > 1 { names.into_iter().map(|(n, d)| if d { n + "/" } else { n }).collect() } else { Vec::new() };
    (completed, shown)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Menu 6: pick something and restore it
    Restore,
    /// Menu 9: look inside archives
    Inspect,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub file: PathBuf,
    pub inspected: Inspected,
    /// (label, app to restore; None = everything)
    pub choices: Vec<(String, Option<String>)>,
    pub choice: usize,
    pub kill: bool,
    pub safety: bool,
    pub dry_run: bool,
    /// Highlighted row: choices, then the 3 toggles, then Start
    pub focus: usize,
}

impl Options {
    fn new(file: PathBuf, inspected: Inspected) -> Self {
        let keys: Vec<&str> = inspected.info.apps.iter().map(|a| a.key.as_str()).collect();
        let mut choices = vec![("All applications & desktop settings (recommended)".to_string(), None)];
        if keys.contains(&"kde") {
            choices.push(("KDE Plasma settings only".into(), Some("kde".to_string())));
        }
        if keys.contains(&"hypr") {
            choices.push(("Hyprland settings only".into(), Some("hypr".to_string())));
        }
        for k in keys.iter().filter(|k| !matches!(**k, "kde" | "hypr")) {
            choices.push((format!("Only {k}"), Some(k.to_string())));
        }
        Options { file, inspected, choices, choice: 0, kill: true, safety: true, dry_run: false, focus: 0 }
    }

    fn rows(&self) -> usize {
        self.choices.len() + 4
    }

    pub fn opts(&self) -> ImportOpts {
        ImportOpts {
            file: self.file.clone(),
            app: self.choices[self.choice].1.clone(),
            safety_backup: self.safety,
            kill: self.kill,
            // With kill set, running apps are closed and this dialog already confirmed the
            // overwrite; without it the engine asks about running apps itself
            force: self.kill,
            dry_run: self.dry_run,
        }
    }

    fn apps(&self) -> Vec<String> {
        match &self.choices[self.choice].1 {
            Some(a) => vec![a.clone()],
            None => self.inspected.info.apps.iter().map(|a| a.key.clone()).collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stage {
    Pick,
    Options(Box<Options>),
}

#[derive(Debug, Clone)]
pub struct ImportState {
    pub mode: Mode,
    pub loading: bool,
    pub all: Vec<Candidate>,
    pub filter: Input,
    pub filtering: bool,
    /// Row in the visible list; the last row is "Custom path…"
    pub selected: usize,
    pub path: Option<Input>,
    pub completions: Vec<String>,
    pub preview: Option<(PathBuf, Result<Option<Inspected>, String>)>,
    pub stage: Stage,
    last_click: Option<(usize, u64)>,
}

impl ImportState {
    pub fn new(mode: Mode) -> Self {
        ImportState {
            mode,
            loading: true,
            all: Vec::new(),
            filter: Input::default(),
            filtering: false,
            selected: 0,
            path: None,
            completions: Vec::new(),
            preview: None,
            stage: Stage::Pick,
            last_click: None,
        }
    }

    pub fn visible(&self) -> Vec<&Candidate> {
        let f = self.filter.value.to_lowercase();
        self.all.iter().filter(|c| f.is_empty() || c.name.to_lowercase().contains(&f) || c.path.to_string_lossy().to_lowercase().contains(&f)).collect()
    }

    fn row_count(&self) -> usize {
        self.visible().len() + 1
    }

    fn selected_candidate(&self) -> Option<&Candidate> {
        self.visible().get(self.selected).copied()
    }

    /// Ask for the manifest of the highlighted archive if the panel shows another file.
    fn preview_cmd(&self) -> Option<Command> {
        let c = self.selected_candidate()?;
        if c.is_dat() || self.preview.as_ref().is_some_and(|(p, _)| *p == c.path) {
            return None;
        }
        Some(Command::Inspect { path: c.path.clone(), pick: false })
    }

    fn move_to(&mut self, row: usize) -> Option<Command> {
        self.selected = row.min(self.row_count() - 1);
        self.preview_cmd()
    }
}

fn state(app: &mut App) -> Option<&mut ImportState> {
    match &mut app.screen {
        Screen::Import(s) => Some(s),
        _ => None,
    }
}

// --- Messages from the runtime -------------------------------------------------------------

pub fn on_candidates(app: &mut App, list: Vec<Candidate>) -> Option<Command> {
    let st = state(app)?;
    st.all = list;
    st.loading = false;
    st.selected = 0;
    st.preview_cmd()
}

pub fn on_inspected(app: &mut App, path: PathBuf, pick: bool, result: Result<Option<Inspected>, String>) -> Option<Command> {
    let user = app.header.user.clone();
    let st = state(app)?;
    if !pick {
        st.preview = Some((path, result));
        return None;
    }
    match result {
        Err(e) => app.toast(ToastKind::Error, e),
        Ok(None) if st.mode == Mode::Inspect => app.toast(ToastKind::Info, "A .dat face model has no manifest to show"),
        Ok(None) => {
            let opts = ImportOpts { file: path.clone(), app: None, safety_backup: true, kill: false, force: true, dry_run: false };
            app.modal = Some(Modal {
                kind: ModalKind::Confirm(Box::new(crate::tui::job::JobKind::Import(opts))),
                title: "Restore face model".into(),
                text: format!("Install this Howdy face model for {user}?"),
                details: vec![
                    format!("File:        {}", path.display()),
                    format!("Destination: /etc/howdy/models/{user}.dat (root:root, 0600)"),
                    "An existing model is copied to the safety snapshots first.".into(),
                ],
                yes: "Restore",
                no: "Back",
                yes_selected: false,
            });
        }
        Ok(Some(ins)) => {
            st.path = None;
            if st.mode == Mode::Inspect {
                st.preview = Some((path, Ok(Some(ins))));
            } else {
                st.stage = Stage::Options(Box::new(Options::new(path, ins)));
            }
        }
    }
    None
}

pub fn on_completion(app: &mut App, value: String, matches: Vec<String>) {
    if let Some(st) = state(app)
        && let Some(input) = &mut st.path
    {
        input.set(value);
        st.completions = matches;
    }
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(st) = state(app) {
        if let Some(p) = &mut st.path {
            p.insert(text.trim());
        } else if st.filtering {
            st.filter.insert(text);
        }
    }
}

// --- Keys ----------------------------------------------------------------------------------

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    if let Some(input) = &mut st.path {
        return match k.code {
            KeyCode::Esc => {
                st.path = None;
                st.completions.clear();
                None
            }
            KeyCode::Tab => Some(Command::Complete(input.value.clone())),
            KeyCode::Enter if !input.value.trim().is_empty() => {
                let path = PathBuf::from(input.value.trim());
                Some(Command::Inspect { path, pick: true })
            }
            _ => {
                input.key(&k);
                st.completions.clear();
                None
            }
        };
    }
    if st.filtering {
        match k.code {
            KeyCode::Esc => {
                st.filter = Input::default();
                st.filtering = false;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => st.filtering = false,
            _ => {
                st.filter.key(&k);
            }
        }
        return st.move_to(0);
    }
    if matches!(st.stage, Stage::Options(_)) {
        return options_key(app, k);
    }
    let last = st.row_count() - 1;
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => st.move_to(st.selected.checked_sub(1).unwrap_or(last)),
        KeyCode::Down | KeyCode::Char('j') => st.move_to(if st.selected >= last { 0 } else { st.selected + 1 }),
        KeyCode::Home | KeyCode::Char('g') => st.move_to(0),
        KeyCode::End | KeyCode::Char('G') => st.move_to(last),
        KeyCode::PageUp => st.move_to(st.selected.saturating_sub(10)),
        KeyCode::PageDown => st.move_to(st.selected + 10),
        KeyCode::Char('/') => {
            st.filtering = true;
            None
        }
        KeyCode::Char('c') => {
            st.path = Some(Input::default());
            None
        }
        KeyCode::Enter => open_row(st),
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Menu;
            None
        }
        _ => None,
    }
}

fn open_row(st: &mut ImportState) -> Option<Command> {
    match st.selected_candidate() {
        Some(c) => Some(Command::Inspect { path: c.path.clone(), pick: true }),
        None => {
            st.path = Some(Input::default());
            None
        }
    }
}

fn options_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Stage::Options(o) = &mut st.stage else { return None };
    let n = o.rows();
    let n_choices = o.choices.len();
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => o.focus = o.focus.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => o.focus = (o.focus + 1) % n,
        KeyCode::Home => o.focus = 0,
        KeyCode::End => o.focus = n - 1,
        KeyCode::Esc | KeyCode::Char('q') => st.stage = Stage::Pick,
        KeyCode::Char('s') => return confirm(app),
        KeyCode::Char(' ') | KeyCode::Enter => {
            let f = o.focus;
            if f < n_choices {
                o.choice = f;
                if k.code == KeyCode::Enter {
                    return confirm(app);
                }
            } else if f == n_choices {
                o.kill = !o.kill;
            } else if f == n_choices + 1 {
                o.safety = !o.safety;
            } else if f == n_choices + 2 {
                o.dry_run = !o.dry_run;
            } else {
                return confirm(app);
            }
        }
        _ => {}
    }
    None
}

/// The dialog before a restore, listing exactly what gets overwritten.
fn confirm(app: &mut App) -> Option<Command> {
    let warn = app.theme.sym.warn;
    let st = state(app)?;
    let Stage::Options(o) = &st.stage else { return None };
    let apps = o.apps();
    let mut details = vec![format!("Archive:  {}", o.file.display()), format!("Restores: {}", apps.join(" ")), String::new()];
    if o.dry_run {
        details.push("Dry run: only shows what would happen, nothing is changed.".into());
    } else {
        details.push("Overwrites:".into());
        for (app_key, dests) in &o.inspected.destinations {
            if apps.contains(app_key) {
                details.extend(dests.iter().map(|d| format!("  {d}")));
            }
        }
        details.push(String::new());
        details.push(if o.safety {
            "A safety snapshot of the current files is saved first.".into()
        } else {
            format!("{warn} No safety snapshot: the current files cannot be rolled back.")
        });
        details.push(if o.kill {
            "Running apps among these are closed first.".into()
        } else {
            "If any of these apps run, you will be asked what to do.".into()
        });
    }
    let opts = o.opts();
    let dry = o.dry_run;
    app.modal = Some(Modal {
        kind: ModalKind::Confirm(Box::new(crate::tui::job::JobKind::Import(opts))),
        title: "Restore".into(),
        text: if dry { "Run a dry run of this restore?".into() } else { "Overwrite these settings with the backup?".into() },
        details,
        yes: if dry { "Dry run" } else { "Restore" },
        no: "Back",
        yes_selected: false,
    });
    None
}

// --- Mouse ---------------------------------------------------------------------------------

pub fn mouse(app: &mut App, m: MouseEvent, body: Rect) -> Option<Command> {
    let now = app.now_ms;
    let st = state(app)?;
    if st.path.is_some() {
        return None;
    }
    if let Stage::Options(o) = &mut st.stage {
        match m.kind {
            MouseEventKind::ScrollUp => o.focus = o.focus.saturating_sub(1),
            MouseEventKind::ScrollDown => o.focus = (o.focus + 1).min(o.rows() - 1),
            _ => {}
        }
        return None;
    }
    match m.kind {
        MouseEventKind::ScrollUp => st.move_to(st.selected.saturating_sub(1)),
        MouseEventKind::ScrollDown => st.move_to(st.selected + 1),
        MouseEventKind::Down(MouseButton::Left) => {
            let (table, _) = layout(body);
            // Border, then the header row
            let first = table.y + 2;
            if m.row < first || m.row >= table.bottom().saturating_sub(1) || m.column <= table.x || m.column >= table.right() {
                return None;
            }
            let row = (m.row - first) as usize + table_offset(st, table);
            if row >= st.row_count() {
                return None;
            }
            let double = st.last_click.is_some_and(|(r, t)| r == row && now.saturating_sub(t) <= 400);
            st.last_click = Some((row, now));
            let cmd = st.move_to(row);
            if double {
                st.last_click = None;
                return open_row(st);
            }
            cmd
        }
        _ => None,
    }
}

/// First visible row of the table (it scrolls to keep the selection in view).
fn table_offset(st: &ImportState, table: Rect) -> usize {
    let rows = table.height.saturating_sub(3) as usize;
    (st.selected + 1).saturating_sub(rows.max(1))
}

// --- Drawing -------------------------------------------------------------------------------

/// (table, side panel); side by side from 100 columns, stacked below.
fn layout(body: Rect) -> (Rect, Rect) {
    if body.width >= 100 {
        let [t, p] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
        (t, p)
    } else {
        let [t, p] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        (t, p)
    }
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &ImportState) {
    let theme = &app.theme;
    if let Stage::Options(o) = &st.stage {
        options_view(f, body, theme, o);
        return;
    }
    let (table_area, panel) = layout(body);
    let wide = body.width >= 100;
    let visible = st.visible();
    let title = match (st.mode, st.filtering || !st.filter.value.is_empty()) {
        (_, true) => format!(" Filter: {}{} ", st.filter.value, if st.filtering { theme.sym.caret } else { "" }),
        (Mode::Restore, false) => " Backups & face models (newest first) ".into(),
        (Mode::Inspect, false) => " Backup archives (newest first) ".into(),
    };
    let header = if wide { vec!["Name", "Type", "Size", "Modified"] } else { vec!["Name", "Type", "Size"] };
    let mut rows: Vec<Row> = visible
        .iter()
        .map(|c| {
            let mut cells = vec![Cell::from(c.name.clone()), Cell::from(c.tag.trim_matches(['[', ']']).replace(" Backup", "")), Cell::from(c.size.clone())];
            if wide {
                cells.push(Cell::from(c.modified.clone()));
            }
            Row::new(cells)
        })
        .collect();
    rows.push(Row::new(vec![Cell::from(Span::styled(format!("Custom path{}", theme.sym.ellipsis), theme.accent()))]));
    let widths: Vec<Constraint> = if wide {
        vec![Constraint::Min(20), Constraint::Length(16), Constraint::Length(6), Constraint::Length(16)]
    } else {
        vec![Constraint::Min(16), Constraint::Length(16), Constraint::Length(6)]
    };
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme.bold()))
        .block(block(theme, title))
        .row_highlight_style(theme.selected())
        .highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ts = TableState::default().with_selected(Some(st.selected)).with_offset(table_offset(st, table_area));
    f.render_stateful_widget(table, table_area, &mut ts);

    // Side panel: what the highlighted entry is
    let mut lines: Vec<Line> = Vec::new();
    if st.loading {
        lines.push(Line::styled(format!("{} Looking for backups{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis), theme.dim()));
    } else if visible.is_empty() && st.all.is_empty() {
        lines.push(Line::raw("No backups found in /media/data/backups, /media/data,"));
        lines.push(Line::raw("~/.local/share/app-vault-sync/backups or ~/Downloads."));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Press c to enter a path.", theme.dim()));
    } else if let Some(c) = st.selected_candidate() {
        lines.push(Line::from(vec![Span::styled("Path: ", theme.bold()), Span::raw(c.path.display().to_string())]));
        lines.push(Line::from(vec![Span::styled("Size: ", theme.bold()), Span::raw(format!("{}   modified {}", c.size, c.modified))]));
        lines.push(Line::raw(""));
        if c.is_dat() {
            lines.push(Line::raw(format!("Howdy face model. Restoring installs it as /etc/howdy/models/{}.dat.", app.header.user)));
        } else {
            match &st.preview {
                Some((p, Ok(Some(ins)))) if *p == c.path => lines.extend(manifest_lines(theme, &ins.info)),
                Some((p, Err(e))) if *p == c.path => lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error())),
                _ => lines.push(Line::styled(format!("Reading manifest{}", theme.sym.ellipsis), theme.dim())),
            }
        }
    } else {
        lines.push(Line::raw("Enter the path of a .zip archive or a .dat face model."));
        if let Some((p, Ok(Some(ins)))) = &st.preview
            && st.mode == Mode::Inspect
            && !st.all.iter().any(|c| c.path == *p)
        {
            lines.push(Line::raw(""));
            lines.extend(manifest_lines(theme, &ins.info));
        }
    }
    let panel_title = if st.mode == Mode::Inspect { " Manifest " } else { " Details " };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, panel_title)), panel);

    if let Some(input) = &st.path {
        path_popup(f, body, theme, input, &st.completions);
    }
}

pub fn manifest_lines<'a>(theme: &Theme, info: &'a InfoReport) -> Vec<Line<'a>> {
    let field = |k: &'static str, v: String| Line::from(vec![Span::styled(format!("{k:<10}"), theme.bold()), Span::raw(v)]);
    let mut lines = vec![
        field("Created", info.created.clone()),
        field("Source", format!("{} ({})", info.host, info.user)),
        field("Target", info.target.clone()),
        field("Media", info.media.clone()),
        field("Size", info.size.clone()),
        Line::raw(""),
        Line::styled(format!("Contains {} item(s):", info.apps.len()), theme.bold()),
    ];
    for a in &info.apps {
        lines.push(Line::styled(format!("{} {}", theme.sym.bullet, a.name), theme.accent()));
        for (label, value) in &a.fields {
            lines.push(Line::raw(format!("    {}{value}", label)));
        }
    }
    lines
}

fn path_popup(f: &mut Frame, body: Rect, theme: &Theme, input: &Input, completions: &[String]) {
    let shown = completions.len().min(6) as u16;
    let r = popup(f, body, body.width.saturating_sub(8).min(90), 4 + shown + u16::from(shown > 0));
    let blk = block(theme, " Path to a .zip archive or .dat face model ");
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let [field, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    input.render(f, field, theme, true);
    let mut lines = vec![Line::styled("Tab completes, Enter opens, Esc closes", theme.dim())];
    if shown > 0 {
        lines.push(Line::raw(""));
        lines.extend(completions.iter().take(6).map(|c| Line::raw(format!("  {c}"))));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), rest);
}

fn options_view(f: &mut Frame, body: Rect, theme: &Theme, o: &Options) {
    let wide = body.width >= 100;
    let (list_area, panel) = if wide {
        let [l, p] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        (l, Some(p))
    } else {
        (body, None)
    };
    let mut items: Vec<ListItem> = o
        .choices
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            let mark = if i == o.choice { theme.sym.radio_on } else { "( )" };
            ListItem::new(format!("{mark} {label}"))
        })
        .collect();
    let toggle = |on: bool, label: &str| ListItem::new(format!("{} {label}", if on { theme.sym.on } else { theme.sym.off }));
    items.push(toggle(o.kill, "Close running apps first"));
    items.push(toggle(o.safety, "Keep a safety snapshot of the current files"));
    items.push(toggle(o.dry_run, "Dry run (change nothing)"));
    let start = if o.dry_run { "Start dry run" } else { "Start restore" };
    items.push(ListItem::new(Line::styled(format!("{} {start}", theme.sym.start), theme.ok())));
    let name = o.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = format!(" Restore {} ", ellipsize(&name, list_area.width.saturating_sub(12) as usize, theme.sym.ellipsis));
    let list = List::new(items).block(block(theme, title)).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(o.focus));
    f.render_stateful_widget(list, list_area, &mut ls);
    if let Some(p) = panel {
        f.render_widget(Paragraph::new(Text::from(manifest_lines(theme, &o.inspected.info))).wrap(Wrap { trim: false }).block(block(theme, " Manifest ")), p);
    }
}

pub fn hints(st: &ImportState) -> &'static [(&'static str, &'static str)] {
    if st.path.is_some() {
        &[("Tab", "complete"), ("Enter", "open"), ("Esc", "close")]
    } else if st.filtering {
        &[("type", "filter"), ("Enter", "done"), ("Esc", "clear")]
    } else if matches!(st.stage, Stage::Options(_)) {
        &[("↑/↓", "move"), ("Space", "toggle"), ("Enter", "choose"), ("s", "start"), ("Esc", "back"), ("?", "help")]
    } else if st.mode == Mode::Inspect {
        &[("↑/↓", "move"), ("/", "filter"), ("c", "custom path"), ("Esc", "back"), ("?", "help")]
    } else {
        &[("↑/↓", "move"), ("Enter", "choose"), ("/", "filter"), ("c", "custom path"), ("Esc", "back"), ("?", "help")]
    }
}

pub const HELP: [(&str, &str); 10] = [
    ("↑/↓  j/k", "Move"),
    ("Enter", "Choose the highlighted backup (or option)"),
    ("/", "Filter the list by name or path"),
    ("c", "Type a path (Tab completes, paste works)"),
    ("Space", "Options: select / toggle"),
    ("s", "Options: start"),
    ("Mouse", "Click selects, double-click chooses, wheel moves"),
    ("Esc  q", "Back"),
    ("Ctrl+U", "Clear the text field"),
    ("?", "This help"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;
    use crate::tui::job::JobKind;
    use crate::vault::info::InfoApp;

    fn cand(name: &str, tag: &'static str, size: &str) -> Candidate {
        Candidate { path: PathBuf::from(format!("/media/data/backups/{name}")), name: name.into(), tag, size: size.into(), modified: "2026-10-16 21:04".into() }
    }

    fn inspected() -> Inspected {
        let app = |key: &str, fields: Vec<(&str, &str)>| InfoApp {
            key: key.into(),
            name: key.to_uppercase(),
            fields: fields.into_iter().map(|(l, v)| (l.to_string(), v.to_string())).collect(),
        };
        Inspected {
            info: InfoReport {
                file: "/media/data/backups/all_backup_2026-10-16_210400.zip".into(),
                size: "1.2G".into(),
                created: "2026-10-16T21:04:00+02:00".into(),
                host: "archbox".into(),
                user: "p3ter".into(),
                target: "all".into(),
                media: "true".into(),
                apps: vec![
                    app("beeper", vec![("Folder:      ", "~/.config/BeeperTexts")]),
                    app("zen", vec![("Folder:      ", "~/.config/zen")]),
                    app("kde", vec![("Config:      ", "~/.config (kdeglobals, kwinrc, Plasma applets, shortcuts)")]),
                ],
            },
            destinations: vec![
                ("beeper".into(), vec!["/home/p3ter/.config/BeeperTexts".into()]),
                ("zen".into(), vec!["/home/p3ter/.config/zen".into()]),
                ("kde".into(), vec!["KDE Plasma settings in /home/p3ter/.config (kdeglobals, kwinrc, ...)".into()]),
            ],
        }
    }

    /// The import screen with three candidates loaded and the first one's manifest shown.
    fn loaded(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        assert!(matches!(update(&mut a, key(KeyCode::Char('6'))), Some(Command::LoadCandidates { zips_only: false })));
        let list = vec![
            cand("all_backup_2026-10-16_210400.zip", "[Full System Backup]", "1.2G"),
            cand("kde_backup_2026-10-01_090000.zip", "[KDE Plasma Backup]", "48M"),
            cand("p3ter_face_model_2026-09-30.dat", "[Howdy Face Model]", "8.0K"),
        ];
        let cmd = update(&mut a, Event::Loaded(Loaded::Candidates(list)));
        let Some(Command::Inspect { path, pick: false }) = cmd else { panic!("expected a preview load") };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: false, result: Ok(Some(inspected())) }));
        a
    }

    fn st(a: &App) -> &ImportState {
        match &a.screen {
            Screen::Import(s) => s,
            _ => panic!("not on the import screen"),
        }
    }

    #[test]
    fn pick_options_and_confirm() {
        let mut a = loaded(80, 24);
        // Wraps to the custom path row
        update(&mut a, key(KeyCode::Up));
        assert_eq!(st(&a).selected, 3);
        update(&mut a, key(KeyCode::Home));
        let Some(Command::Inspect { path, pick: true }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(inspected())) }));
        let Stage::Options(o) = &st(&a).stage else { panic!("no options") };
        let labels: Vec<&str> = o.choices.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["All applications & desktop settings (recommended)", "KDE Plasma settings only", "Only beeper", "Only zen"]);
        // Choose "KDE only", turn off the safety snapshot, start
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        for _ in 0..4 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Char('s')));
        let m = a.modal.as_ref().unwrap();
        assert!(!m.yes_selected);
        assert!(m.details.iter().any(|d| d.contains("KDE Plasma settings in")));
        assert!(!m.details.iter().any(|d| d.contains("BeeperTexts")));
        assert!(m.details.iter().any(|d| d.contains("No safety snapshot")));
        let ModalKind::Confirm(kind) = &m.kind else { panic!() };
        let JobKind::Import(o) = kind.as_ref() else { panic!() };
        assert_eq!((o.app.as_deref(), o.safety_backup, o.kill, o.force, o.dry_run), (Some("kde"), false, true, true, false));
        // Enter = Back (the safe default); y starts the job
        update(&mut a, key(KeyCode::Enter));
        assert!(a.modal.is_none());
        update(&mut a, key(KeyCode::Char('s')));
        assert!(matches!(update(&mut a, key(KeyCode::Char('y'))), Some(Command::Start(JobKind::Import(_)))));
        assert!(matches!(a.screen, Screen::Run(_)));
    }

    #[test]
    fn face_model_confirm_names_destination() {
        let mut a = loaded(80, 24);
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Up));
        let Some(Command::Inspect { path, pick: true }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(None) }));
        let m = a.modal.as_ref().unwrap();
        assert!(m.details.iter().any(|d| d.contains("/etc/howdy/models/p3ter.dat")));
    }

    #[test]
    fn filter_and_custom_path() {
        let mut a = loaded(80, 24);
        update(&mut a, key(KeyCode::Char('/')));
        for c in "kde".chars() {
            update(&mut a, key(KeyCode::Char(c)));
        }
        assert_eq!(st(&a).visible().len(), 1);
        // '?' and 'q' are text while typing
        update(&mut a, key(KeyCode::Char('q')));
        assert!(!a.quit);
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(st(&a).visible().len(), 3);
        update(&mut a, key(KeyCode::Char('c')));
        update(&mut a, Event::Paste("/tmp/x.zip\n".into()));
        assert!(matches!(update(&mut a, key(KeyCode::Tab)), Some(Command::Complete(v)) if v == "/tmp/x.zip"));
        let cmd = update(&mut a, key(KeyCode::Enter));
        assert!(matches!(cmd, Some(Command::Inspect { pick: true, .. })));
        update(&mut a, Event::Loaded(Loaded::Inspected { path: "/tmp/x.zip".into(), pick: true, result: Err("File not found: /tmp/x.zip".into()) }));
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Error);
        update(&mut a, key(KeyCode::Esc));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn snapshot_import_table_80x24() {
        insta::assert_snapshot!(render(&loaded(80, 24)));
    }

    #[test]
    fn snapshot_import_table_140x40() {
        insta::assert_snapshot!(render(&loaded(140, 40)));
    }

    #[test]
    fn snapshot_import_confirm_long_80x24() {
        let mut a = loaded(80, 24);
        let mut ins = inspected();
        ins.info.apps.clear();
        ins.destinations.clear();
        for k in crate::vault::ALL_APPS {
            ins.info.apps.push(InfoApp { key: k.into(), name: k.to_uppercase(), fields: vec![] });
            ins.destinations.push((k.into(), vec![format!("/home/p3ter/.config/{k}")]));
        }
        let Some(Command::Inspect { path, .. }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(ins)) }));
        update(&mut a, key(KeyCode::Char('s')));
        let out = render(&a);
        // The buttons stay visible even when the list doesn't fit
        assert!(out.contains("Restore") && out.contains("Back"));
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_import_confirm_140x40() {
        let mut a = loaded(140, 40);
        let Some(Command::Inspect { path, .. }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(inspected())) }));
        update(&mut a, key(KeyCode::Char('s')));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn path_completion() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("backups")).unwrap();
        std::fs::write(t.path().join("backups/all_backup_1.zip"), "").unwrap();
        std::fs::write(t.path().join("backups/all_backup_2.zip"), "").unwrap();
        std::fs::write(t.path().join(".hidden"), "").unwrap();
        let home = t.path();
        assert_eq!(complete_path("~/ba", home), ("~/backups/".to_string(), vec![]));
        let (v, m) = complete_path("~/backups/a", home);
        assert_eq!(v, "~/backups/all_backup_");
        assert_eq!(m, vec!["all_backup_1.zip", "all_backup_2.zip"]);
        let abs = format!("{}/backups/all_backup_2", home.display());
        assert_eq!(complete_path(&abs, home).0, format!("{abs}.zip"));
        assert_eq!(complete_path("~/nothing", home), ("~/nothing".to_string(), vec![]));
        assert_eq!(complete_path("~/.hi", home).0, "~/.hidden");
    }
}
