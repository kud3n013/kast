//! Data partition (menu `2`, and a stage of the automatic run): pick a partition from the
//! detected ones (rows that can't be used are dim, with the reason), a mount point, the
//! options with the exact fstab line shown live, which home folders to move, and a review.
//! Detection, the mount point check and the subvolume listing are commands for the runtime;
//! nothing here touches the machine.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::checklist::{Checklist, Item, Outcome, Row as CheckRow};
use crate::partition::apply::{self, Plan, Who};
use crate::partition::detect::{self, Row};
use crate::partition::fstab::{self, Choices, Mode, Preset, Spot};
use crate::partition::{Driver, Fs};
use crate::setup::HomeState;
use crate::tui::app::{App, Command, Screen, ToastKind, start_job};
use crate::tui::job::JobKind;
use crate::tui::widgets::{Input, block, ellipsize};

/// What the machine looked like when the screen opened (`Command::LoadPartitions`).
#[derive(Debug, Clone, PartialEq)]
pub struct Scan {
    pub rows: Vec<Row>,
    pub fstab: String,
    /// The home folders kast can move, and what each is now
    pub homes: Vec<(String, HomeState)>,
}

pub fn load() -> Result<Box<Scan>, String> {
    let fstab = std::fs::read_to_string(crate::journal::sys_path("/etc/fstab")).unwrap_or_default();
    let (_, rows) = detect::scan(&crate::partition::SysRunner, &fstab).map_err(|e| format!("{e:#}"))?;
    let home = crate::ctx::ctx().target_home.clone();
    let homes = crate::setup::USER_DIRS.iter().map(|d| (d.to_string(), crate::setup::home_state(&home.join(d)))).collect();
    Ok(Box::new(Scan { rows, fstab, homes }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Pick,
    Mount,
    Options,
    Folders,
    Review,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Subvols {
    NotLoaded,
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

/// A row of the options stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    Mode,
    Preset,
    ReadOnly,
    NoAtime,
    Prealloc,
    Compress,
    Subvol,
    OwnTop,
    Replace,
    Unmount,
    Manual,
    Continue,
}

#[derive(Debug, Clone)]
pub struct PartState {
    /// A stage of the automatic run: ends at the folders, the wizard's review shows the rest
    pub embedded: bool,
    pub stage: Stage,
    pub loading: bool,
    pub error: Option<String>,
    pub scan: Option<Box<Scan>>,
    pub cursor: usize,
    pub picked: Option<usize>,
    pub mount: Input,
    pub mount_error: Option<String>,
    /// The mount point check is running
    pub checking: bool,
    pub completions: Vec<String>,
    pub spot: Option<Spot>,
    pub choices: Option<Choices>,
    pub focus: usize,
    pub edit: Option<Input>,
    pub edit_error: Option<String>,
    pub subvols: Subvols,
    pub replace: bool,
    pub unmount_first: bool,
    pub folders: Checklist,
    pub start_focused: bool,
}

impl PartState {
    pub fn new(embedded: bool) -> Self {
        PartState {
            embedded,
            stage: Stage::Pick,
            loading: true,
            error: None,
            scan: None,
            cursor: 0,
            picked: None,
            mount: Input::default(),
            mount_error: None,
            checking: false,
            completions: Vec::new(),
            spot: None,
            choices: None,
            focus: 0,
            edit: None,
            edit_error: None,
            subvols: Subvols::NotLoaded,
            replace: true,
            unmount_first: true,
            folders: Checklist::default(),
            start_focused: true,
        }
    }

    pub fn rows(&self) -> &[Row] {
        self.scan.as_ref().map_or(&[], |s| &s.rows)
    }

    pub fn row(&self) -> Option<&Row> {
        self.picked.and_then(|i| self.rows().get(i))
    }

    pub fn busy(&self) -> bool {
        self.loading || self.checking || self.subvols == Subvols::Loading
    }

    /// The fstab lines that name this partition or this mount point.
    pub fn conflicts(&self) -> Vec<fstab::Conflict> {
        match (&self.scan, self.row(), &self.choices) {
            (Some(s), Some(r), Some(c)) => fstab::conflicts(&s.fstab, &r.part, &c.mountpoint),
            _ => Vec::new(),
        }
    }

    /// Where the partition is mounted now.
    pub fn mounted_at(&self) -> Vec<String> {
        self.row().map(|r| r.part.mountpoints.iter().filter(|m| !m.starts_with('[')).cloned().collect()).unwrap_or_default()
    }

    pub fn opts(&self) -> Vec<Opt> {
        let Some(c) = &self.choices else { return Vec::new() };
        let mut v = vec![Opt::Mode];
        if c.driver.fs().foreign() {
            v.push(Opt::Preset);
        }
        v.extend([Opt::ReadOnly, Opt::NoAtime]);
        match c.driver {
            Driver::Ntfs3 => v.push(Opt::Prealloc),
            Driver::Btrfs => v.extend([Opt::Compress, Opt::Subvol]),
            _ => {}
        }
        if !c.driver.fs().foreign() {
            v.push(Opt::OwnTop);
        }
        if !self.conflicts().is_empty() {
            v.push(Opt::Replace);
        }
        if !self.mounted_at().is_empty() {
            v.push(Opt::Unmount);
        }
        v.extend([Opt::Manual, Opt::Continue]);
        v
    }

    /// Why the plan can't be made yet.
    pub fn blocker(&self) -> Option<String> {
        if self.choices.is_none() {
            return Some("pick a partition first".into());
        }
        if !self.conflicts().is_empty() && !self.replace {
            return Some("an /etc/fstab line is in the way: allow replacing it, or pick another mount point".into());
        }
        if !self.mounted_at().is_empty() && !self.unmount_first {
            return Some("it is mounted elsewhere: allow kast to unmount it first".into());
        }
        None
    }

    pub fn plan(&self, who: Who, date: String) -> Result<Plan, String> {
        if let Some(b) = self.blocker() {
            return Err(b);
        }
        let (Some(row), Some(choices)) = (self.row(), &self.choices) else { return Err("pick a partition first".into()) };
        let replace = if self.replace { self.conflicts().iter().map(|c| c.line.no).collect() } else { Vec::new() };
        Ok(Plan {
            part: row.part.clone(),
            choices: choices.clone(),
            replace,
            unmount_first: self.unmount_first && !self.mounted_at().is_empty(),
            folders: self.folders.selected_keys(),
            who,
            date,
        })
    }
}

/// `YYYY-MM-DD` of a Unix time (UTC; only for the comment line).
pub fn ymd(secs: i64) -> String {
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn who(app: &App) -> Who {
    Who { user: app.header.user.clone(), uid: app.header.uid, gid: app.header.gid, home: std::path::PathBuf::from(&app.header.home) }
}

/// The state being edited: the screen's own, or the automatic run's stage.
pub fn state(app: &mut App) -> Option<&mut PartState> {
    match &mut app.screen {
        Screen::Partition(s) => Some(s),
        Screen::Auto(w) if w.stage == super::wizard::Stage::Partition => w.partition.as_deref_mut(),
        _ => None,
    }
}

pub fn state_ref(app: &App) -> Option<&PartState> {
    match &app.screen {
        Screen::Partition(s) => Some(s),
        Screen::Auto(w) if w.stage == super::wizard::Stage::Partition => w.partition.as_deref(),
        _ => None,
    }
}

pub fn editing(app: &App) -> bool {
    state_ref(app).is_some_and(|s| s.stage == Stage::Mount || s.edit.is_some())
}

pub fn open(app: &mut App) -> Option<Command> {
    app.screen = Screen::Partition(Box::new(PartState::new(false)));
    Some(Command::LoadPartitions)
}

pub fn on_scan(app: &mut App, r: Result<Box<Scan>, String>) {
    let Some(st) = state(app) else { return };
    st.loading = false;
    match r {
        Ok(scan) => {
            st.error = None;
            // Keep the choice when the same partition is still there
            let keep = st.row().map(|r| r.part.path.clone());
            st.scan = Some(scan);
            st.cursor = st.cursor.min(st.rows().len().saturating_sub(1));
            st.picked = keep.and_then(|p| st.rows().iter().position(|r| r.part.path == p));
            if st.picked.is_none() && st.stage != Stage::Pick {
                st.stage = Stage::Pick;
                st.choices = None;
            }
        }
        Err(e) => st.error = Some(e),
    }
}

pub fn on_mountpoint(app: &mut App, raw: String, result: Result<(String, Spot), String>) -> Option<Command> {
    let st = state(app)?;
    if st.mount.value != raw {
        return None;
    }
    st.checking = false;
    match result {
        Err(e) => {
            st.mount_error = Some(e);
            None
        }
        Ok((path, spot)) => {
            st.mount_error = None;
            st.spot = Some(spot);
            if let Some(c) = &mut st.choices {
                c.mountpoint = path;
            }
            enter(app, Stage::Options)
        }
    }
}

pub fn on_subvolumes(app: &mut App, r: Result<Vec<String>, String>) {
    if let Some(st) = state(app) {
        st.subvols = match r {
            Ok(v) => Subvols::Loaded(v),
            Err(e) => Subvols::Failed(e),
        };
    }
}

pub fn on_completion(app: &mut App, value: String, matches: Vec<String>) {
    if let Some(st) = state(app).filter(|s| s.stage == Stage::Mount && s.mount.value == value) {
        // A single answer extends the path; several are listed
        if let [one] = matches.as_slice() {
            st.mount.set(one.clone());
            st.completions.clear();
        } else {
            st.completions = matches;
        }
    } else if let Some(st) = state(app).filter(|s| s.stage == Stage::Mount) {
        st.mount.set(value);
        st.completions = matches;
    }
}

/// Go to `stage`, setting up what it needs.
fn enter(app: &mut App, stage: Stage) -> Option<Command> {
    let (uid, gid) = (app.header.uid, app.header.gid);
    let st = state(app)?;
    st.stage = stage;
    st.edit = None;
    st.focus = 0;
    match stage {
        Stage::Mount if st.choices.is_none() => {
            let row = st.row()?.clone();
            let driver = row.driver()?;
            let mp = crate::partition::default_mountpoint(&row.part);
            let mut c = Choices::new(&row.part, driver, &mp, uid, gid);
            c.own_top_folder = !driver.fs().foreign();
            st.mount.set(mp);
            st.choices = Some(c);
            st.subvols = Subvols::NotLoaded;
            st.replace = true;
            st.unmount_first = true;
            None
        }
        Stage::Options => {
            let (btrfs, part) = (st.choices.as_ref().is_some_and(|c| c.driver == Driver::Btrfs), st.row().map(|r| r.part.clone()));
            if btrfs && st.subvols == Subvols::NotLoaded {
                st.subvols = Subvols::Loading;
                return part.map(|p| Command::LoadSubvolumes(Box::new(p)));
            }
            None
        }
        Stage::Folders => {
            let mode_note = st.choices.as_ref().is_some_and(|c| c.mode == Mode::Automount || c.external);
            let rows = st
                .scan
                .as_ref()
                .map(|s| s.homes.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|(name, state)| {
                    let mut it = Item::new(name.clone(), format!("~/{name}"), false);
                    match state {
                        HomeState::Dir => it.tag = Some("folder".into()),
                        HomeState::Absent => it.tag = Some("not there yet: created on the partition".into()),
                        HomeState::Link(t) => {
                            it.enabled = false;
                            it.note = Some(format!("already a link to {}", t.display()));
                        }
                        HomeState::File => {
                            it.enabled = false;
                            it.note = Some("a file, not a folder".into());
                        }
                    }
                    CheckRow::Item(it)
                })
                .collect();
            let old: Vec<String> = st.folders.selected_keys();
            st.folders = Checklist::new(rows);
            for k in old {
                if let Some(it) = st.folders.item_mut(&k) {
                    it.checked = true;
                }
            }
            let _ = mode_note;
            None
        }
        _ => None,
    }
}

fn back(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    let embedded = st.embedded;
    let to = match st.stage {
        Stage::Pick => {
            if embedded {
                super::wizard::back(app);
            } else {
                app.screen = Screen::Menu;
            }
            return None;
        }
        Stage::Mount => Stage::Pick,
        Stage::Options => Stage::Mount,
        Stage::Folders => Stage::Options,
        Stage::Review => Stage::Folders,
    };
    enter(app, to)
}

/// Enter on the last row of the options.
fn forward(app: &mut App, from: Stage) -> Option<Command> {
    let st = state(app)?;
    if let Some(b) = st.blocker().filter(|_| from == Stage::Options) {
        app.toast(ToastKind::Error, b);
        return None;
    }
    let to = match from {
        Stage::Options => Stage::Folders,
        Stage::Folders if st.embedded => return finish_embedded(app),
        Stage::Folders => Stage::Review,
        other => other,
    };
    if to == Stage::Review {
        st.start_focused = true;
    }
    enter(app, to)
}

fn finish_embedded(app: &mut App) -> Option<Command> {
    let (w, date) = (who(app), ymd(app.now));
    let plan = state(app)?.plan(w, date);
    match plan {
        Ok(p) => super::wizard::partition_done(app, p),
        Err(e) => {
            app.toast(ToastKind::Error, e);
            None
        }
    }
}

fn start(app: &mut App) -> Option<Command> {
    let (w, date) = (who(app), ymd(app.now));
    match state(app)?.plan(w, date) {
        Ok(p) => Some(start_job(app, JobKind::Partition(Box::new(p)))),
        Err(e) => {
            app.toast(ToastKind::Error, e);
            None
        }
    }
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let stage = state(app)?.stage;
    if state(app)?.edit.is_some() {
        return edit_key(app, k);
    }
    match stage {
        Stage::Pick => pick_key(app, k),
        Stage::Mount => mount_key(app, k),
        Stage::Options => options_key(app, k),
        Stage::Folders => {
            let st = state(app)?;
            match st.folders.key(&k, 10) {
                Outcome::Confirm => forward(app, Stage::Folders),
                Outcome::Back => back(app),
                Outcome::Ignored if k.code == KeyCode::Char('q') => back(app),
                _ => None,
            }
        }
        Stage::Review => {
            let st = state(app)?;
            match k.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h' | 'l') => {
                    st.start_focused = !st.start_focused;
                    None
                }
                KeyCode::Enter if st.start_focused => start(app),
                KeyCode::Char('s') => start(app),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => back(app),
                _ => None,
            }
        }
    }
}

fn pick_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let n = st.rows().len();
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return back(app),
        KeyCode::Char('r') if !st.loading => {
            st.loading = true;
            return Some(Command::LoadPartitions);
        }
        _ if st.loading || n == 0 => {}
        KeyCode::Up | KeyCode::Char('k') => st.cursor = st.cursor.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.cursor = (st.cursor + 1) % n,
        KeyCode::Home => st.cursor = 0,
        KeyCode::End => st.cursor = n - 1,
        KeyCode::Enter | KeyCode::Char(' ') => {
            let row = st.rows()[st.cursor].clone();
            if let Some(why) = row.reason() {
                let why = format!("{} can't be used: {why}", row.part.path);
                app.toast(ToastKind::Info, why);
                return None;
            }
            // A different partition starts over; the same one keeps what was chosen
            if st.picked != Some(st.cursor) {
                st.choices = None;
                st.folders = Checklist::default();
            }
            st.picked = Some(st.cursor);
            return enter(app, Stage::Mount);
        }
        _ => {}
    }
    None
}

fn mount_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    match k.code {
        KeyCode::Esc => return back(app),
        KeyCode::Tab => {
            return Some(Command::Complete(st.mount.value.clone()));
        }
        KeyCode::Enter if !st.checking => {
            let raw = st.mount.value.clone();
            match fstab::normalise_mountpoint(&raw) {
                Err(e) => st.mount_error = Some(e),
                Ok(path) => {
                    st.mount.set(path.clone());
                    st.mount_error = None;
                    st.checking = true;
                    return Some(Command::CheckMountpoint(path));
                }
            }
        }
        _ => {
            if st.mount.key(&k) {
                st.mount_error = None;
                st.completions.clear();
            }
        }
    }
    None
}

fn edit_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let c = st.choices.as_mut()?;
    let input = st.edit.as_mut()?;
    match k.code {
        KeyCode::Esc => st.edit = None,
        KeyCode::Enter => {
            if input.value.trim().is_empty() {
                c.manual = None;
                st.edit = None;
                st.edit_error = None;
            } else {
                match fstab::validate_manual(c.driver, &input.value) {
                    Ok(v) => {
                        c.manual = Some(v);
                        st.edit = None;
                        st.edit_error = None;
                    }
                    Err(e) => st.edit_error = Some(e),
                }
            }
        }
        _ => {
            input.key(&k);
        }
    }
    None
}

fn options_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let opts = st.opts();
    let n = opts.len().max(1);
    st.focus = st.focus.min(n - 1);
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return back(app),
        KeyCode::Up | KeyCode::Char('k') => st.focus = st.focus.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.focus = (st.focus + 1) % n,
        KeyCode::Char('c') => return forward(app, Stage::Options),
        KeyCode::Char(' ' | 'h' | 'l') | KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
            let back_dir = matches!(k.code, KeyCode::Left | KeyCode::Char('h'));
            match opts[st.focus] {
                Opt::Continue => return forward(app, Stage::Options),
                row => toggle(st, row, back_dir),
            }
        }
        _ => {}
    }
    None
}

fn toggle(st: &mut PartState, row: Opt, back: bool) {
    let subvols = match &st.subvols {
        Subvols::Loaded(v) => v.clone(),
        _ => Vec::new(),
    };
    let manual_start = st.choices.as_ref().map(|c| c.manual.clone().unwrap_or_else(|| fstab::driver_options(c)).join(","));
    let Some(c) = st.choices.as_mut() else { return };
    match row {
        Opt::Mode => c.mode = if c.mode == Mode::Boot { Mode::Automount } else { Mode::Boot },
        Opt::Preset => {
            let i = Preset::ALL.iter().position(|p| *p == c.preset).unwrap_or(0);
            c.preset = Preset::ALL[(i + if back { Preset::ALL.len() - 1 } else { 1 }) % Preset::ALL.len()];
        }
        Opt::ReadOnly => c.read_only = !c.read_only,
        Opt::NoAtime => c.noatime = !c.noatime,
        Opt::Prealloc => c.prealloc = !c.prealloc,
        Opt::Compress => c.compress = !c.compress,
        Opt::Subvol => {
            // "default" then each subvolume
            let n = subvols.len() + 1;
            let i = c.subvol.as_ref().and_then(|s| subvols.iter().position(|v| v == s)).map_or(0, |p| p + 1);
            let j = (i + if back { n - 1 } else { 1 }) % n;
            c.subvol = if j == 0 { None } else { Some(subvols[j - 1].clone()) };
        }
        Opt::OwnTop => c.own_top_folder = !c.own_top_folder,
        Opt::Replace => st.replace = !st.replace,
        Opt::Unmount => st.unmount_first = !st.unmount_first,
        Opt::Manual => {
            st.edit = Some(Input::new(manual_start.unwrap_or_default()));
            st.edit_error = None;
        }
        Opt::Continue => {}
    }
}

// --- Drawing -------------------------------------------------------------------------------

fn field<'a>(theme: &crate::tui::theme::Theme, k: &'static str, v: impl Into<String>) -> Line<'a> {
    Line::from(vec![Span::styled(k, theme.bold()), Span::raw(v.into())])
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    match st.stage {
        Stage::Pick => pick_view(f, body, app, st),
        Stage::Mount => mount_view(f, body, app, st),
        Stage::Options => options_view(f, body, app, st),
        Stage::Folders => folders_view(f, body, app, st),
        Stage::Review => review_view(f, body, app, st),
    }
}

fn split(body: Rect, list_h: u16) -> (Rect, Rect) {
    if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Length(62), Constraint::Min(30)]).areas(body);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Length(list_h), Constraint::Min(3)]).areas(body);
        (l, r)
    }
}

fn row_text(r: &Row, width: usize) -> String {
    let p = &r.part;
    let label = p.label.as_deref().filter(|l| !l.is_empty()).unwrap_or("-");
    let fs = p.fs().map_or_else(|| p.fstype.clone().unwrap_or_else(|| "none".into()), |f| f.name().to_string());
    let kind = if p.external { "external" } else { "internal" };
    let text = format!("{:<15} {:<10} {:<11} {:>10}  {kind}", p.path, ellipsize(label, 10, "…"), fs, crate::partition::human(p.size));
    ellipsize(&text, width, "…")
}

fn pick_view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    let theme = &app.theme;
    let n = st.rows().len() as u16;
    let blocked = st.rows().iter().filter(|r| r.driver().is_none()).count() as u16;
    let (list_area, details) = split(body, (n + blocked + 2).clamp(5, 12));
    let title = " Pick the partition ";
    if st.loading || st.scan.is_none() {
        let msg = match &st.error {
            Some(e) => Line::styled(format!("{} {e}", theme.sym.fail), theme.error()),
            None => Line::styled(format!("{} Looking at the disks{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis), theme.dim()),
        };
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }).block(block(theme, title)), list_area);
        f.render_widget(Paragraph::new("").block(block(theme, " Details ")), details);
        return;
    }
    let w = list_area.width.saturating_sub(6) as usize;
    let items: Vec<ListItem> = st
        .rows()
        .iter()
        .map(|r| {
            let t = row_text(r, w);
            match r.reason() {
                // Dim, with the reason under it
                Some(why) => ListItem::new(Text::from(vec![
                    Line::styled(t, theme.dim()),
                    Line::styled(format!("   {} {}", theme.sym.bullet, ellipsize(why, w.saturating_sub(5), "…")), theme.dim()),
                ])),
                None => ListItem::new(Line::from(vec![Span::raw(t)])),
            }
        })
        .collect();
    let list = List::new(items).block(block(theme, title)).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(st.cursor));
    f.render_stateful_widget(list, list_area, &mut ls);

    let mut lines = Vec::new();
    if let Some(r) = st.rows().get(st.cursor) {
        let p = &r.part;
        lines.push(Line::styled(p.path.clone(), theme.title()));
        lines.push(field(theme, "Disk:        ", p.model.clone().unwrap_or_else(|| p.disk.clone())));
        lines.push(field(
            theme,
            "Filesystem:  ",
            match r.driver() {
                Some(d) => format!("{} (driver {})", p.fstype.clone().unwrap_or_default(), d.fstab_type()),
                None => p.fstype.clone().unwrap_or_else(|| "none".into()),
            },
        ));
        lines.push(field(
            theme,
            "Size:        ",
            match p.fsused {
                Some(u) => format!("{} ({} used)", crate::partition::human(p.size), crate::partition::human(u)),
                None => crate::partition::human(p.size),
            },
        ));
        lines.push(field(
            theme,
            "UUID:        ",
            p.uuid.clone().or_else(|| p.partuuid.clone().map(|u| format!("PARTUUID {u}"))).unwrap_or_else(|| "none".into()),
        ));
        if !p.mountpoints.is_empty() {
            lines.push(field(theme, "Mounted at:  ", p.mountpoints.join(", ")));
        }
        for l in &r.existing {
            lines.push(field(theme, "In fstab:    ", l.raw.trim().to_string()));
        }
        lines.push(Line::raw(""));
        match r.reason() {
            Some(why) => lines.push(Line::styled(format!("{} Can't be used: {why}", theme.sym.warn), theme.warn())),
            None => lines.push(Line::styled(format!("{} Can be used", theme.sym.ok), theme.ok())),
        }
        if p.fs() == Some(Fs::Ntfs) && r.driver().is_some() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("{} If Windows has Fast Startup or hibernation on, the partition is left \"dirty\" and Linux can only mount it read-only (or not at all). Turn Fast Startup off in Windows and shut down fully.", theme.sym.warn),
                theme.warn(),
            ));
        }
    } else {
        lines.push(Line::styled("No partitions found.", theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " Details ")), details);
}

fn mount_view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    let theme = &app.theme;
    let [input, info] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(body);
    let b = block(theme, " Mount point ");
    let inner = b.inner(input);
    f.render_widget(b, input);
    st.mount.render(f, inner, theme, true);
    let mut lines = Vec::new();
    if let Some(e) = &st.mount_error {
        lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error()));
        lines.push(Line::raw(""));
    }
    if st.checking {
        lines.push(Line::styled(format!("{} Checking the folder{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis), theme.dim()));
    }
    if !st.completions.is_empty() {
        lines.push(Line::styled("Folders:", theme.bold()));
        lines.extend(st.completions.iter().take(8).map(|c| Line::raw(format!("  {c}"))));
        lines.push(Line::raw(""));
    }
    if let Some(r) = st.row() {
        lines.push(Line::raw(format!(
            "{} will be mounted here. The folder is created if it doesn't exist (and removed again by undo if it is empty).",
            r.part.path
        )));
    }
    lines.push(Line::styled("Spaces and special characters are escaped in /etc/fstab for you. Tab completes a folder.", theme.dim()));
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " About ")), info);
}

fn opt_line(theme: &crate::tui::theme::Theme, st: &PartState, o: Opt) -> (String, String) {
    let c = st.choices.as_ref().expect("choices");
    let check = |b: bool| if b { theme.sym.on } else { theme.sym.off };
    match o {
        Opt::Mode => ("Mount".into(), (if c.mode == Mode::Boot { "at boot" } else { "on first access, unmounted when idle (automount)" }).to_string()),
        Opt::Preset => ("Who can use it".into(), format!("{}: {}", c.preset.label(), c.preset.about())),
        Opt::ReadOnly => (format!("{} Read-only", check(c.read_only)), String::new()),
        Opt::NoAtime => (format!("{} Don't record access times (noatime)", check(c.noatime)), String::new()),
        Opt::Prealloc => (format!("{} Preallocate on write (prealloc)", check(c.prealloc)), String::new()),
        Opt::Compress => (format!("{} Compress files (zstd:1)", check(c.compress)), String::new()),
        Opt::Subvol => (
            "Subvolume".into(),
            match (&st.subvols, &c.subvol) {
                (Subvols::Loading, _) => "looking…".into(),
                (Subvols::Failed(e), _) => format!("default (couldn't list: {e})"),
                (_, Some(s)) => s.clone(),
                _ => "the default one".into(),
            },
        ),
        Opt::OwnTop => (format!("{} Make the top folder yours (chown the folder only, not what is in it)", check(c.own_top_folder)), String::new()),
        Opt::Replace => (format!("{} Replace the /etc/fstab line in the way", check(st.replace)), String::new()),
        Opt::Unmount => (format!("{} Unmount it from {} first", check(st.unmount_first), st.mounted_at().join(", ")), String::new()),
        Opt::Manual => ("Edit the options by hand".into(), if c.manual.is_some() { "edited".into() } else { String::new() }),
        Opt::Continue => (format!("{} Continue", theme.sym.start), String::new()),
    }
}

fn options_view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    let theme = &app.theme;
    let opts = st.opts();
    let Some(c) = &st.choices else { return };
    let (list_area, line_area) = if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Length(62), Constraint::Min(30)]).areas(body);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Length(opts.len() as u16 + 2), Constraint::Min(4)]).areas(body);
        (l, r)
    };
    let items: Vec<ListItem> = opts
        .iter()
        .map(|o| {
            let (a, b) = opt_line(theme, st, *o);
            let mut spans = vec![Span::raw(a)];
            if !b.is_empty() {
                spans.push(Span::styled(format!("  {b}"), theme.dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items).block(block(theme, " Options ")).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(st.focus.min(opts.len().saturating_sub(1))));
    f.render_stateful_widget(list, list_area, &mut ls);

    let Some(row) = st.row() else { return };
    let entry = fstab::fstab_entry(&row.part, c);
    let mut lines = vec![Line::styled("This line is added to /etc/fstab:", theme.bold()), Line::styled(entry.to_string(), theme.accent()), Line::raw("")];
    if body.width >= 100 {
        // The same line, a field at a time (the long one wraps wherever the terminal ends)
        let pad = "            ";
        lines.push(field(theme, "Device:     ", entry.spec.clone()));
        lines.push(field(theme, "Folder:     ", entry.file.clone()));
        lines.push(field(theme, "Type:       ", entry.vfstype.clone()));
        for (i, o) in entry.options.iter().enumerate() {
            lines.push(if i == 0 { field(theme, "Options:    ", o.clone()) } else { Line::raw(format!("{pad}{o}")) });
        }
        lines.push(field(theme, "Dump, fsck: ", format!("{} {}", entry.freq, entry.passno)));
        lines.push(Line::raw(""));
    }
    for n in &entry.notes {
        lines.push(Line::styled(format!("{} {n}", theme.sym.warn), theme.warn()));
    }
    for cf in st.conflicts() {
        lines.push(Line::styled(format!("{} Line {} ({}): {}", theme.sym.warn, cf.line.no, cf.why(), cf.line.raw.trim()), theme.warn()));
    }
    if let Some(e) = &st.edit {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Options (comma separated, only ones the driver accepts):", theme.bold()));
        lines.push(Line::raw(e.value.clone()));
        if let Some(err) = &st.edit_error {
            lines.push(Line::styled(format!("{} {err}", theme.sym.fail), theme.error()));
        }
        lines.push(Line::styled("Enter keeps them, Esc cancels, empty goes back to the generated ones.", theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " fstab line ")), line_area);
}

fn folders_view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    let theme = &app.theme;
    let [list, info] = Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).areas(body);
    st.folders.view(f, list, theme, "Move home folders onto the partition (optional)");
    let mut lines = vec![Line::raw("A checked folder is copied to the partition, and only after a full copy replaced by a link. Undo copies it back.")];
    if st.choices.as_ref().is_some_and(|c| c.mode == Mode::Automount || c.external) {
        lines.push(Line::styled(format!("{} This drive can be unplugged: linked folders are empty while it is away.", theme.sym.warn), theme.warn()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " About ")), info);
}

fn review_view(f: &mut Frame, body: Rect, app: &App, st: &PartState) {
    let theme = &app.theme;
    let [text, buttons] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(body);
    let lines: Vec<Line> = match st.plan(who(app), ymd(app.now)) {
        Ok(p) => apply::describe(&p).into_iter().map(Line::raw).collect(),
        Err(e) => vec![Line::styled(e, theme.error())],
    };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Review ")), text);
    let button = |label: &str, on: bool, style: Style| {
        let s = if on { theme.selected() } else { style };
        Span::styled(format!(" {} {label} ", if on { theme.sym.pointer } else { " " }), s)
    };
    let line = Line::from(vec![button("Start", st.start_focused, theme.ok()), Span::raw("   "), button("Back", !st.start_focused, Style::new())]);
    f.render_widget(Paragraph::new(line).block(block(theme, "")), buttons);
}

pub fn hints(st: &PartState) -> &'static [(&'static str, &'static str)] {
    if st.edit.is_some() {
        return &[("Enter", "keep"), ("Esc", "cancel")];
    }
    match st.stage {
        Stage::Pick => &[("↑/↓", "move"), ("Enter", "pick"), ("r", "scan again"), ("Esc", "back"), ("?", "help")],
        Stage::Mount => &[("Enter", "check"), ("Tab", "complete"), ("Esc", "back")],
        Stage::Options => &[("Space", "change"), ("←/→", "cycle"), ("c", "continue"), ("Esc", "back"), ("?", "help")],
        Stage::Folders => &[("Space", "toggle"), ("Enter", "continue"), ("Esc", "back"), ("?", "help")],
        Stage::Review => &[("Enter", "choose"), ("←/→", "Start/Back"), ("s", "start"), ("Esc", "back"), ("?", "help")],
    }
}

pub const HELP: [(&str, &str); 7] = [
    ("↑/↓  j/k", "Move"),
    ("Enter", "Pick the partition, check the mount point, continue"),
    ("Space", "Change an option, check a folder"),
    ("←/→", "Cycle a choice (permissions, subvolume)"),
    ("r", "Scan the disks again"),
    ("c", "Continue from the options"),
    ("Esc  q", "Back one stage (your choices are kept)"),
];

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::partition::Partition;
    use crate::partition::detect::Verdict;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;

    fn part(path: &str, fs: &str, label: &str, uuid: &str, size: u64, external: bool) -> Partition {
        Partition {
            path: path.into(),
            name: path.trim_start_matches("/dev/").into(),
            disk: path.trim_end_matches(|c: char| c.is_ascii_digit()).into(),
            fstype: Some(fs.into()),
            label: (!label.is_empty()).then(|| label.into()),
            uuid: (!uuid.is_empty()).then(|| uuid.into()),
            size,
            fsused: Some(size / 3),
            external,
            model: Some(if external { "SanDisk Ultra" } else { "Samsung 970 EVO" }.into()),
            ..Default::default()
        }
    }

    fn row(p: Partition, v: Verdict) -> Row {
        Row { part: p, verdict: v, existing: Vec::new() }
    }

    pub fn scan() -> Box<Scan> {
        let mut root = part("/dev/nvme0n1p2", "btrfs", "", "2720fb5d-cbdd-4e4d-9b5f-0e3c7a1f4d11", 500 << 30, false);
        root.mountpoints = vec!["/".into()];
        let rows = vec![
            row(part("/dev/sda1", "ntfs", "Data", "6C62ADEA62ADB96A", 931 << 30, false), Verdict::Pick(Driver::Ntfs3)),
            row(part("/dev/sdb1", "exfat", "STICK", "5A1B-77C2", 59 << 30, true), Verdict::Pick(Driver::Exfat)),
            row(part("/dev/sdc1", "ext4", "Backup", "9d8a0c4e-1f0b-4c52-8d3e-6a7b5c4d3e2f", 2000 << 30, false), Verdict::Pick(Driver::Ext4)),
            row(part("/dev/sdd1", "btrfs", "Pool", "11111111-2222-3333-4444-555555555555", 1000 << 30, false), Verdict::Pick(Driver::Btrfs)),
            row(root, Verdict::Blocked("in use by the system (mounted at /)".into())),
            row(part("/dev/nvme0n1p1", "vfat", "", "D0A3-F918", 512 << 20, false), Verdict::Blocked("EFI system partition".into())),
            row(
                part("/dev/nvme0n1p3", "crypto_LUKS", "", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", 100 << 30, false),
                Verdict::Blocked("encrypted (LUKS): needs /etc/crypttab, not supported yet".into()),
            ),
        ];
        let homes = ["Desktop", "Documents", "Downloads", "Music", "Pictures", "Public", "Templates", "Videos", "Projects"]
            .iter()
            .map(|d| {
                let st = match *d {
                    "Public" => HomeState::Link("/media/old/Public".into()),
                    "Projects" => HomeState::Absent,
                    _ => HomeState::Dir,
                };
                (d.to_string(), st)
            })
            .collect();
        Box::new(Scan {
            rows,
            fstab: "UUID=D0A3-F918 /boot vfat defaults 0 2\nUUID=2720fb5d-cbdd-4e4d-9b5f-0e3c7a1f4d11 / btrfs subvol=/@ 0 0\n".into(),
            homes,
        })
    }

    fn st(a: &App) -> &PartState {
        match &a.screen {
            Screen::Partition(s) => s,
            _ => panic!("not on the partition screen"),
        }
    }

    /// The fstab line the options stage would add, whatever the terminal wraps.
    fn line(a: &App) -> String {
        let s = st(a);
        fstab::fstab_entry(&s.row().unwrap().part, s.choices.as_ref().unwrap()).to_string()
    }

    fn opened(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        a.now = 1_792_400_000; // 2026-10-19
        assert!(matches!(update(&mut a, key(KeyCode::Char('2'))), Some(Command::LoadPartitions)));
        update(&mut a, Event::Loaded(Loaded::Partitions(Ok(scan()))));
        a
    }

    fn type_text(a: &mut App, s: &str) {
        for c in s.chars() {
            update(a, key(KeyCode::Char(c)));
        }
    }

    /// Down to the row, Enter, then the mount point answers.
    fn pick(a: &mut App, device: &str, mp_ok: Option<&str>) {
        let i = st(a).rows().iter().position(|r| r.part.path == device).unwrap();
        if let Screen::Partition(s) = &mut a.screen {
            s.cursor = i;
        }
        update(a, key(KeyCode::Enter));
        assert_eq!(st(a).stage, Stage::Mount);
        if let Some(mp) = mp_ok {
            let Some(Command::CheckMountpoint(raw)) = update(a, key(KeyCode::Enter)) else { panic!("expected a check") };
            assert_eq!(raw, mp);
            update(a, Event::Loaded(Loaded::Mountpoint { raw, result: Ok((mp.into(), Spot::Missing)) }));
        }
    }

    #[test]
    fn dates_and_default_mount_points() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_792_400_000), "2026-10-19");
        assert_eq!(ymd(1_709_164_800), "2024-02-29");
        let s = scan();
        assert_eq!(crate::partition::default_mountpoint(&s.rows[0].part), "/media/data");
        assert_eq!(crate::partition::default_mountpoint(&s.rows[4].part), "/media/data", "no label");
        let mut p = s.rows[0].part.clone();
        p.label = Some("My Files!".into());
        assert_eq!(crate::partition::default_mountpoint(&p), "/media/my-files");
    }

    #[test]
    fn menu_two_opens_the_list_and_blocked_rows_say_why() {
        let mut a = opened(140, 40);
        let out = render(&a);
        assert!(out.contains("/dev/sda1") && out.contains("EFI system partition"), "{out}");
        // A blocked row can't be entered; the reason comes as a toast
        let i = st(&a).rows().iter().position(|r| r.part.path == "/dev/nvme0n1p1").unwrap();
        if let Screen::Partition(s) = &mut a.screen {
            s.cursor = i;
        }
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).stage, Stage::Pick);
        assert!(a.toast.as_ref().unwrap().text.contains("EFI system partition"));
        insta::assert_snapshot!("partition_pick_140x40", out);
        let a = opened(80, 24);
        insta::assert_snapshot!("partition_pick_80x24", render(&a));
    }

    #[test]
    fn ntfs_details_warn_about_fast_startup() {
        let a = opened(140, 40);
        let out = render(&a);
        assert!(out.contains("Fast Startup"), "{out}");
        insta::assert_snapshot!("partition_details_ntfs_140x40", out);
    }

    #[test]
    fn the_mount_point_is_checked_and_errors_stay_on_the_stage() {
        let mut a = opened(80, 24);
        pick(&mut a, "/dev/sda1", None);
        assert_eq!(st(&a).mount.value, "/media/data");
        // Not absolute
        update(&mut a, key(KeyCode::Char('u')));
        for _ in 0..20 {
            update(&mut a, key(KeyCode::Backspace));
        }
        type_text(&mut a, "media/x");
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(st(&a).mount_error.as_deref().unwrap().contains("absolute"));
        insta::assert_snapshot!("partition_mount_error_80x24", render(&a));
        // A system folder
        for _ in 0..20 {
            update(&mut a, key(KeyCode::Backspace));
        }
        type_text(&mut a, "/home");
        update(&mut a, key(KeyCode::Enter));
        assert!(st(&a).mount_error.as_deref().unwrap().contains("system folder"));
        // The machine says no (not empty)
        for _ in 0..20 {
            update(&mut a, key(KeyCode::Backspace));
        }
        type_text(&mut a, "/media/full");
        let Some(Command::CheckMountpoint(raw)) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Mountpoint { raw, result: Err("/media/full is not empty; files in it would be hidden by the mount".into()) }));
        assert_eq!(st(&a).stage, Stage::Mount);
        assert!(render(&a).contains("is not empty"));
        // Esc goes back to the list with the pick kept
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(st(&a).stage, Stage::Pick);
        assert_eq!(st(&a).picked, Some(0));
    }

    #[test]
    fn completion_extends_or_lists() {
        let mut a = opened(80, 24);
        pick(&mut a, "/dev/sda1", None);
        let Some(Command::Complete(v)) = update(&mut a, key(KeyCode::Tab)) else { panic!("expected a completion") };
        assert_eq!(v, "/media/data");
        update(&mut a, Event::Loaded(Loaded::Completion { value: v.clone(), matches: vec!["/media/data/".into()] }));
        assert_eq!(st(&a).mount.value, "/media/data/");
        update(&mut a, Event::Loaded(Loaded::Completion { value: "/media/data/".into(), matches: vec!["a/".into(), "b/".into()] }));
        assert_eq!(st(&a).completions, ["a/", "b/"]);
    }

    #[test]
    fn options_show_the_exact_line_and_it_follows_the_choices() {
        let mut a = opened(140, 40);
        pick(&mut a, "/dev/sda1", Some("/media/data"));
        assert_eq!(st(&a).stage, Stage::Options);
        let out = render(&a);
        assert_eq!(
            line(&a),
            "UUID=6C62ADEA62ADB96A /media/data ntfs3 uid=1000,gid=1000,dmask=0077,fmask=0177,iocharset=utf8,windows_names,nofail,x-systemd.device-timeout=10s 0 0"
        );
        assert!(out.contains("Options:    uid=1000") && out.contains("windows_names") && out.contains("Dump, fsck: 0 0"), "{out}");
        insta::assert_snapshot!("partition_options_ntfs_140x40", out);
        // Mount: first row. Boot → automount adds the two systemd options
        update(&mut a, key(KeyCode::Char(' ')));
        let out = render(&a);
        assert!(out.contains("x-systemd.automount") && out.contains("x-systemd.idle-timeout=5min"), "{out}");
        assert!(line(&a).ends_with("nofail,x-systemd.device-timeout=10s,x-systemd.automount,x-systemd.idle-timeout=5min 0 0"));
        // Permissions: "Me, others can read"
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Right));
        assert!(line(&a).contains("dmask=0022,fmask=0133"));
        // Read-only
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        assert!(line(&a).contains("ntfs3 ro,uid=1000"));
        let a80 = {
            let mut b = opened(80, 24);
            pick(&mut b, "/dev/sdc1", Some("/media/backup"));
            b
        };
        let out = render(&a80);
        assert_eq!(line(&a80), "UUID=9d8a0c4e-1f0b-4c52-8d3e-6a7b5c4d3e2f /media/backup ext4 nofail,x-systemd.device-timeout=10s 0 2");
        insta::assert_snapshot!("partition_options_ext4_80x24", out);
        assert!(st(&a80).choices.as_ref().unwrap().own_top_folder, "ext4: the top folder is made the user's by default");
    }

    #[test]
    fn external_drives_default_to_automount_and_a_short_timeout() {
        let mut a = opened(140, 40);
        pick(&mut a, "/dev/sdb1", Some("/media/stick"));
        let out = render(&a);
        assert_eq!(
            line(&a),
            "UUID=5A1B-77C2 /media/stick exfat uid=1000,gid=1000,dmask=0077,fmask=0177,nofail,x-systemd.device-timeout=5s,x-systemd.automount,x-systemd.idle-timeout=5min 0 0"
        );
        assert!(out.contains("x-systemd.device-timeout=5s"), "{out}");
        assert!(out.contains("on first access"), "{out}");
    }

    #[test]
    fn editing_the_options_by_hand_is_checked_per_driver() {
        let mut a = opened(140, 40);
        pick(&mut a, "/dev/sdc1", Some("/media/backup"));
        let n = st(&a).opts().iter().position(|o| *o == Opt::Manual).unwrap();
        if let Screen::Partition(s) = &mut a.screen {
            s.focus = n;
        }
        update(&mut a, key(KeyCode::Enter));
        assert!(st(&a).edit.is_some());
        for _ in 0..40 {
            update(&mut a, key(KeyCode::Backspace));
        }
        type_text(&mut a, "noatime,exec");
        update(&mut a, key(KeyCode::Enter));
        assert!(st(&a).edit.is_some(), "exec is refused");
        assert!(st(&a).edit_error.is_some());
        insta::assert_snapshot!("partition_options_manual_error_140x40", render(&a));
        for _ in 0..40 {
            update(&mut a, key(KeyCode::Backspace));
        }
        type_text(&mut a, "noatime");
        update(&mut a, key(KeyCode::Enter));
        assert!(st(&a).edit.is_none());
        assert_eq!(st(&a).choices.as_ref().unwrap().manual.as_deref(), Some(&["noatime".to_string()][..]));
        assert!(line(&a).ends_with("/media/backup ext4 noatime,nofail,x-systemd.device-timeout=10s 0 2"));
        // Esc cancels an edit
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Esc));
        assert!(st(&a).edit.is_none() && st(&a).stage == Stage::Options);
    }

    #[test]
    fn btrfs_lists_its_subvolumes_and_the_choice_goes_in_the_line() {
        let mut a = opened(140, 40);
        let i = st(&a).rows().iter().position(|r| r.part.path == "/dev/sdd1").unwrap();
        if let Screen::Partition(s) = &mut a.screen {
            s.cursor = i;
        }
        update(&mut a, key(KeyCode::Enter));
        let Some(Command::CheckMountpoint(raw)) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        let cmd = update(&mut a, Event::Loaded(Loaded::Mountpoint { raw, result: Ok(("/media/pool".into(), Spot::Missing)) }));
        let Some(Command::LoadSubvolumes(p)) = cmd else { panic!("expected the subvolumes to be listed") };
        assert_eq!(p.path, "/dev/sdd1");
        update(&mut a, Event::Loaded(Loaded::Subvolumes(Ok(vec!["/@data".into(), "/@snapshots".into()]))));
        let n = st(&a).opts().iter().position(|o| *o == Opt::Subvol).unwrap();
        if let Screen::Partition(s) = &mut a.screen {
            s.focus = n;
        }
        update(&mut a, key(KeyCode::Right));
        let out = render(&a);
        assert!(line(&a).ends_with("/media/pool btrfs subvol=/@data,nofail,x-systemd.device-timeout=10s 0 0"));
        assert!(out.contains("subvol=/@data"), "{out}");
        insta::assert_snapshot!("partition_options_btrfs_140x40", out);
        update(&mut a, key(KeyCode::Left));
        assert!(st(&a).choices.as_ref().unwrap().subvol.is_none(), "back to the default subvolume");
    }

    #[test]
    fn a_line_in_the_way_must_be_allowed_to_go() {
        let mut a = opened(140, 40);
        if let Screen::Partition(s) = &mut a.screen {
            s.scan.as_mut().unwrap().fstab += "UUID=6C62ADEA62ADB96A /media/old ntfs3 defaults 0 0\n";
        }
        pick(&mut a, "/dev/sda1", Some("/media/data"));
        let out = render(&a);
        assert!(out.contains("the same partition, mounted at another folder"), "{out}");
        let n = st(&a).opts().iter().position(|o| *o == Opt::Replace).expect("a replace row");
        if let Screen::Partition(s) = &mut a.screen {
            s.focus = n;
        }
        update(&mut a, key(KeyCode::Char(' ')));
        assert!(!st(&a).replace);
        update(&mut a, key(KeyCode::Char('c')));
        assert_eq!(st(&a).stage, Stage::Options, "blocked");
        assert!(a.toast.as_ref().unwrap().text.contains("in the way"));
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Char('c')));
        assert_eq!(st(&a).stage, Stage::Folders);
    }

    #[test]
    fn folders_that_are_already_links_are_greyed_and_the_plan_is_complete() {
        let mut a = opened(140, 40);
        pick(&mut a, "/dev/sdc1", Some("/media/backup"));
        update(&mut a, key(KeyCode::Char('c')));
        assert_eq!(st(&a).stage, Stage::Folders);
        let public = st(&a).folders.item("Public").unwrap();
        assert!(!public.enabled && public.note.as_deref().unwrap().contains("already a link"));
        // Documents and Videos
        for name in ["Documents", "Videos"] {
            let i = st(&a).folders.rows.iter().position(|r| matches!(r, CheckRow::Item(it) if it.key == name)).unwrap();
            if let Screen::Partition(s) = &mut a.screen {
                s.folders.cursor = i;
            }
            update(&mut a, key(KeyCode::Char(' ')));
        }
        insta::assert_snapshot!("partition_folders_140x40", render(&a));
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).stage, Stage::Review);
        let out = render(&a);
        assert!(out.contains("Documents, Videos") && out.contains("chown 1000:1000"), "{out}");
        insta::assert_snapshot!("partition_review_140x40", out);
        let a80 = {
            let mut b = opened(80, 24);
            pick(&mut b, "/dev/sdc1", Some("/media/backup"));
            update(&mut b, key(KeyCode::Char('c')));
            update(&mut b, key(KeyCode::Enter));
            b
        };
        insta::assert_snapshot!("partition_review_80x24", render(&a80));

        // Start: the job carries the whole plan
        let Some(Command::Start(JobKind::Partition(plan))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the job") };
        assert_eq!(plan.part.path, "/dev/sdc1");
        assert_eq!(plan.choices.mountpoint, "/media/backup");
        assert_eq!(plan.folders, ["Documents", "Videos"]);
        assert_eq!(plan.date, "2026-10-19");
        assert_eq!((plan.who.user.as_str(), plan.who.uid), ("p3ter", 1000));
        assert_eq!(plan.entry().to_string(), "UUID=9d8a0c4e-1f0b-4c52-8d3e-6a7b5c4d3e2f /media/backup ext4 nofail,x-systemd.device-timeout=10s 0 2");
        assert!(plan.replace.is_empty() && !plan.unmount_first);
        assert!(matches!(a.screen, Screen::Run(_)));
    }

    #[test]
    fn esc_walks_back_and_keeps_the_choices() {
        let mut a = opened(80, 24);
        pick(&mut a, "/dev/sdc1", Some("/media/backup"));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(st(&a).stage, Stage::Mount);
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(st(&a).stage, Stage::Pick);
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).stage, Stage::Mount);
        assert!(st(&a).choices.as_ref().unwrap().read_only, "the same partition keeps its options");
        update(&mut a, key(KeyCode::Esc));
        update(&mut a, key(KeyCode::Esc));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn scanning_again_keeps_the_pick_and_errors_are_shown() {
        let mut a = opened(80, 24);
        pick(&mut a, "/dev/sdc1", Some("/media/backup"));
        update(&mut a, Event::Loaded(Loaded::Partitions(Ok(scan()))));
        assert_eq!(st(&a).stage, Stage::Options, "the same partition is still there");
        let mut gone = scan();
        gone.rows.retain(|r| r.part.path != "/dev/sdc1");
        update(&mut a, Event::Loaded(Loaded::Partitions(Ok(gone))));
        assert_eq!(st(&a).stage, Stage::Pick, "unplugged: back to the list");
        let mut b = app(80, 24);
        update(&mut b, key(KeyCode::Char('2')));
        update(&mut b, Event::Loaded(Loaded::Partitions(Err("lsblk failed: not found".into()))));
        assert!(render(&b).contains("lsblk failed"));
        assert!(matches!(update(&mut b, key(KeyCode::Char('r'))), Some(Command::LoadPartitions)));
    }
}
