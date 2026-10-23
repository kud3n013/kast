//! Menu option 1: choose the automatic run's steps, then go through a stage for each enabled
//! step that needs choices (apps, the backup to restore, what to back up and where), then a
//! review. Everything is decided before the job starts, so it can run unattended. Esc steps
//! back one stage; nothing chosen is lost by going back, or by leaving to the menu.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::checklist::{self, Checklist, Item, Outcome, Row};
use super::export::{self, ExportState};
use super::import::{self, ImportState};
use super::packages::{self, PackagesState};
use crate::setup::{AutoPlan, Step};
use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind, start_job};
use crate::tui::job::JobKind;
use crate::tui::theme::Theme;
use crate::tui::widgets::block;
use crate::vault::backup::BackupOpts;
use crate::vault::import::ImportOpts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Steps,
    /// Pick the partition, mount point, options and folders (the partition screen's stages)
    Partition,
    /// The upgrade can't be undone: say so, offer a snapshot
    Upgrade,
    Apps,
    Import,
    Export,
    Review,
}

#[derive(Debug, Clone)]
pub struct Wizard {
    pub stage: Stage,
    pub steps: Checklist,
    pub apps: PackagesState,
    /// Created on first visit, then kept (going back and forth keeps the choices)
    pub import: Option<ImportState>,
    pub export: Option<ExportState>,
    pub import_opts: Option<ImportOpts>,
    pub export_opts: Option<BackupOpts>,
    /// The backup goes to Proton Drive like this
    pub export_upload: Option<crate::drive::UploadOpts>,
    /// The data partition's stages, kept while going back and forth
    pub partition: Option<Box<super::partition::PartState>>,
    /// What they decided
    pub partition_plan: Option<crate::partition::apply::Plan>,
    /// Review: Start (true) or Back highlighted
    pub start_focused: bool,
    /// "Take a snapshot before upgrading", built once the machine was looked at
    pub snap: Option<Checklist>,
}

/// The checklist label, what the step does and what it changes.
pub fn step_text(s: Step) -> (&'static str, &'static str, &'static str) {
    match s {
        Step::Partition => (
            "Data partition (pick one, fstab line, mount)",
            "Mounts a partition you pick through /etc/fstab, after a test mount, and optionally moves your home folders onto it. Runs first: backups usually live there.",
            "/etc/fstab (backup kept), the mount point, optionally your home folders",
        ),
        Step::Export => (
            "Export data (back up before anything changes)",
            "Backs up app data, desktop settings or the Howdy face model to a folder you pick. Runs before the upgrade and the installs change anything.",
            "Only the new backup file",
        ),
        Step::Upgrade => ("System upgrade (pacman -Syuu)", "Upgrades every installed package.", "Installed packages"),
        Step::Repo => (
            "kud3n's repository",
            "Adds [archrepo] to /etc/pacman.conf (or updates its server), then syncs and upgrades.",
            "/etc/pacman.conf, upgraded packages",
        ),
        Step::Packages => (
            "Install packages (pick the apps next)",
            "Installs the apps you pick from the package list. Anything pacman can't install is retried with yay as your user.",
            "Installed packages; a temporary sudoers rule for yay, removed right after",
        ),
        Step::Services => (
            "Services (VPN, IME, Howdy, Valkey/SearXNG)",
            "Installs ProtonVPN and Fcitx5, configures Howdy for the IR camera and its emitter, enables valkey and searxng.",
            "/etc/pam.d files (backups kept), Howdy config, systemd services",
        ),
        Step::Import => (
            "Import data (restore a backup; runs last)",
            "Restores a backup you pick. Runs last, once the apps are installed and /media/data is mounted.",
            "App folders in ~/.config, desktop settings, /etc/howdy/models (a safety snapshot is kept)",
        ),
    }
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Wizard {
    pub fn new() -> Self {
        let rows = Step::ALL.into_iter().map(|s| Row::Item(Item::new(s.key(), step_text(s).0, s.standard()))).collect();
        Wizard {
            stage: Stage::Steps,
            steps: Checklist::new(rows),
            apps: PackagesState::new(true),
            import: None,
            export: None,
            import_opts: None,
            export_opts: None,
            export_upload: None,
            partition: None,
            partition_plan: None,
            start_focused: true,
            snap: None,
        }
    }

    /// The snapshot checkbox for what the machine has (on by default unless snap-pac).
    pub fn set_detected(&mut self, d: &crate::snapshot::Detected) {
        if self.snap.is_some() {
            return;
        }
        let label = if d.offered() { format!("Take a snapshot before upgrading ({})", d.tool_name()) } else { "Take a snapshot before upgrading".into() };
        let mut it = Item::new("snapshot", label, d.default_on());
        if !d.offered() {
            it.enabled = false;
            it.checked = false;
            it.note = Some("no snapshot tool for /".into());
        }
        self.snap = Some(Checklist::new(vec![Row::Item(it)]));
    }

    fn snapshot_on(&self) -> bool {
        self.snap.as_ref().and_then(|c| c.item("snapshot")).is_some_and(Item::selected)
    }

    pub fn chosen(&self) -> Vec<Step> {
        self.steps.selected_keys().iter().filter_map(|k| Step::from_key(k)).collect()
    }

    fn has(&self, s: Step) -> bool {
        self.steps.item(s.key()).is_some_and(Item::selected)
    }

    pub fn apps_mut(&mut self) -> Option<&mut PackagesState> {
        (self.stage == Stage::Apps).then_some(&mut self.apps)
    }

    pub fn import_mut(&mut self) -> Option<&mut ImportState> {
        if self.stage == Stage::Import { self.import.as_mut() } else { None }
    }

    pub fn import_ref(&self) -> Option<&ImportState> {
        if self.stage == Stage::Import { self.import.as_ref() } else { None }
    }

    pub fn export_mut(&mut self) -> Option<&mut ExportState> {
        if self.stage == Stage::Export { self.export.as_mut() } else { None }
    }

    pub fn export_ref(&self) -> Option<&ExportState> {
        if self.stage == Stage::Export { self.export.as_ref() } else { None }
    }

    /// The stages for the steps now checked, in order.
    pub fn stages(&self) -> Vec<Stage> {
        let mut v = vec![Stage::Steps];
        if self.has(Step::Partition) {
            v.push(Stage::Partition);
        }
        if self.has(Step::Upgrade) {
            v.push(Stage::Upgrade);
        }
        if self.has(Step::Packages) {
            v.push(Stage::Apps);
        }
        if self.has(Step::Import) {
            v.push(Stage::Import);
        }
        if self.has(Step::Export) {
            v.push(Stage::Export);
        }
        v.push(Stage::Review);
        v
    }

    /// What will run.
    pub fn plan(&self) -> AutoPlan {
        let steps = self.chosen();
        AutoPlan {
            packages: if steps.contains(&Step::Packages) { self.apps.selection() } else { None },
            import: if steps.contains(&Step::Import) { self.import_opts.clone() } else { None },
            export: if steps.contains(&Step::Export) { self.export_opts.clone() } else { None },
            upload: if steps.contains(&Step::Export) { self.export_upload.clone() } else { None },
            snapshot: steps.contains(&Step::Upgrade) && self.snapshot_on(),
            partition: if steps.contains(&Step::Partition) { self.partition_plan.clone().map(Box::new) } else { None },
            steps,
        }
    }

    fn list(&self) -> Option<Arc<crate::pkglist::PkgList>> {
        self.apps.for_list.clone()
    }
}

fn wiz(app: &mut App) -> Option<&mut Wizard> {
    match &mut app.screen {
        Screen::Auto(w) => Some(w),
        _ => None,
    }
}

/// Menu option 1: the step list, as it was left the last time.
pub fn open(app: &mut App) -> Option<Command> {
    let mut w = app.wizard.take().unwrap_or_default();
    let cmd = match &app.snapshots {
        Some(d) => {
            w.set_detected(d);
            None
        }
        None => Some(Command::DetectSnapshots),
    };
    app.screen = Screen::Auto(w);
    cmd
}

/// The machine was looked at for snapshot tools.
pub fn on_detected(app: &mut App, d: crate::snapshot::Detected) {
    if let Screen::Auto(w) = &mut app.screen {
        w.set_detected(&d);
    }
    app.snapshots = Some(d);
}

/// Leave to the menu, keeping the choices for the next visit.
pub fn leave(app: &mut App) {
    if let Screen::Auto(w) = std::mem::replace(&mut app.screen, Screen::Menu) {
        app.wizard = Some(w);
    }
}

/// Go to `stage`, setting up what it needs.
fn enter(app: &mut App, stage: Stage) -> Option<Command> {
    let user = app.header.user.clone();
    let import_dir = import::default_dir(&app.header.home, app.dir_exists);
    let export_dir = export::default_dir(&app.header.home, app.dir_exists);
    let w = wiz(app)?;
    w.stage = stage;
    match stage {
        Stage::Partition => {
            let fresh = w.partition.is_none();
            w.partition.get_or_insert_with(|| Box::new(super::partition::PartState::new(true)));
            fresh.then_some(Command::LoadPartitions)
        }
        Stage::Apps => packages::sync(app),
        Stage::Import => {
            w.import.get_or_insert_with(|| ImportState::new(import::Mode::Restore, true, import_dir));
            super::drive::check_if_unknown(app)
        }
        Stage::Export if w.export.is_none() => {
            w.export = Some(ExportState::new(&user, true, export_dir));
            Some(match super::drive::check_if_unknown(app) {
                Some(check) => Command::Many(vec![Command::LoadExportTargets, check]),
                None => Command::LoadExportTargets,
            })
        }
        _ => None,
    }
}

/// On to the next stage, after checking the current one.
pub fn next(app: &mut App) -> Option<Command> {
    let w = wiz(app)?;
    match w.stage {
        Stage::Steps if w.chosen().is_empty() => {
            app.toast(ToastKind::Error, "Check at least one step");
            return None;
        }
        Stage::Upgrade if w.snap.is_none() => {
            app.toast(ToastKind::Info, "Still checking for snapshot tools");
            return None;
        }
        Stage::Apps => match &w.apps.picker {
            None => {
                app.toast(ToastKind::Info, "Still fetching the package list");
                return None;
            }
            Some(p) if p.count().0 == 0 => {
                app.modal = Some(Modal {
                    kind: ModalKind::SkipPackages,
                    title: "No apps checked".into(),
                    text: "Skip the packages step?".into(),
                    details: vec!["No app is checked, so the packages step would do nothing.".into(), "Back: check some apps (a checks all).".into()],
                    yes: "Skip it",
                    no: "Back",
                    yes_selected: false,
                });
                return None;
            }
            _ => {}
        },
        _ => {}
    }
    let stages = w.stages();
    let i = stages.iter().position(|s| *s == w.stage).unwrap_or(0);
    let to = stages.get(i + 1).copied().unwrap_or(Stage::Review);
    if to == Stage::Review {
        w.start_focused = true;
    }
    enter(app, to)
}

/// Back one stage; from the step list, to the menu.
pub fn back(app: &mut App) {
    let Some(w) = wiz(app) else { return };
    let stages = w.stages();
    match stages.iter().position(|s| *s == w.stage) {
        Some(0) | None => leave(app),
        Some(i) => {
            let to = stages[i - 1];
            enter(app, to);
        }
    }
}

/// "Skip it" on the empty-apps question: drop the packages step and go on.
pub fn skip_packages(app: &mut App) -> Option<Command> {
    let w = wiz(app)?;
    // The stage that came after the apps stage
    let old = w.stages();
    let to = old.iter().position(|s| *s == Stage::Apps).and_then(|i| old.get(i + 1)).copied().unwrap_or(Stage::Review);
    if let Some(it) = w.steps.item_mut(Step::Packages.key()) {
        it.checked = false;
    }
    if w.chosen().is_empty() {
        w.stage = Stage::Steps;
        app.toast(ToastKind::Error, "No steps left: check at least one");
        return None;
    }
    enter(app, to)
}

/// The partition stages' end: remember what was decided, go on.
pub fn partition_done(app: &mut App, plan: crate::partition::apply::Plan) -> Option<Command> {
    wiz(app)?.partition_plan = Some(plan);
    next(app)
}

/// The import stage's "Use this": remember the restore, go on.
pub fn import_done(app: &mut App, opts: ImportOpts) -> Option<Command> {
    wiz(app)?.import_opts = Some(opts);
    next(app)
}

/// The export stage's "Use this": remember the backup, go on.
pub fn export_done(app: &mut App, opts: BackupOpts) -> Option<Command> {
    let w = wiz(app)?;
    w.export_opts = Some(opts);
    w.export_upload = None;
    next(app)
}

/// The backup goes to Proton Drive (signed in, checked while configuring).
pub fn export_upload_done(app: &mut App, opts: BackupOpts, up: crate::drive::UploadOpts) -> Option<Command> {
    let w = wiz(app)?;
    w.export_opts = Some(opts);
    w.export_upload = Some(up);
    next(app)
}

fn start(app: &mut App) -> Option<Command> {
    let w = wiz(app)?;
    let plan = w.plan();
    let list = w.list();
    if plan.has(Step::Partition) && plan.partition.is_none() {
        app.toast(ToastKind::Error, "The data partition step has no partition chosen");
        return None;
    }
    if plan.has(Step::Packages) && list.is_none() {
        app.toast(ToastKind::Error, "The package list isn't loaded yet");
        return None;
    }
    let kind = JobKind::Auto(list, Box::new(plan));
    leave(app);
    Some(start_job(app, kind))
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let stage = wiz(app)?.stage;
    match stage {
        Stage::Upgrade => {
            let w = wiz(app)?;
            let outcome = match &mut w.snap {
                Some(c) => c.key(&k, 4),
                None => match k.code {
                    KeyCode::Enter => Outcome::Confirm,
                    KeyCode::Esc => Outcome::Back,
                    _ => Outcome::Ignored,
                },
            };
            match outcome {
                Outcome::Confirm => next(app),
                Outcome::Back => {
                    back(app);
                    None
                }
                Outcome::Ignored if k.code == KeyCode::Char('q') => {
                    back(app);
                    None
                }
                _ => None,
            }
        }
        Stage::Partition => super::partition::key(app, k),
        Stage::Apps => packages::key(app, k),
        Stage::Import => import::key(app, k),
        Stage::Export => export::key(app, k),
        Stage::Steps => {
            let w = wiz(app)?;
            match w.steps.key(&k, 10) {
                Outcome::Confirm => next(app),
                Outcome::Back => {
                    leave(app);
                    None
                }
                Outcome::Ignored if k.code == KeyCode::Char('q') => {
                    leave(app);
                    None
                }
                _ => None,
            }
        }
        Stage::Review => {
            let w = wiz(app)?;
            match k.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h' | 'l') => {
                    w.start_focused = !w.start_focused;
                    None
                }
                KeyCode::Enter if w.start_focused => start(app),
                KeyCode::Char('s') => start(app),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    back(app);
                    None
                }
                _ => None,
            }
        }
    }
}

pub fn mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    let body = app.body_area();
    let stage = wiz(app)?.stage;
    match stage {
        Stage::Apps => packages::mouse(app, m),
        Stage::Import => import::mouse(app, m, body),
        Stage::Export => export::mouse(app, m),
        Stage::Steps => {
            let (list, _) = layout(body);
            let w = wiz(app)?;
            match m.kind {
                MouseEventKind::ScrollUp => w.steps.step(false),
                MouseEventKind::ScrollDown => w.steps.step(true),
                MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                    w.steps.click(list, m.row);
                }
                _ => {}
            }
            None
        }
        Stage::Review | Stage::Upgrade | Stage::Partition => None,
    }
}

// --- Drawing -------------------------------------------------------------------------------

/// (list, details): side by side from 100 columns, stacked below.
fn layout(body: Rect) -> (Rect, Rect) {
    if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Length(62), Constraint::Min(30)]).areas(body);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Length(Step::ALL.len() as u16 + 2), Constraint::Min(3)]).areas(body);
        (l, r)
    }
}

pub fn view(f: &mut Frame, body: Rect, app: &App, w: &Wizard) {
    match w.stage {
        Stage::Partition => {
            if let Some(st) = &w.partition {
                super::partition::view(f, body, app, st);
            }
        }
        Stage::Apps => packages::view(f, body, app, &w.apps),
        Stage::Import => {
            if let Some(st) = &w.import {
                import::view(f, body, app, st);
            }
        }
        Stage::Export => {
            if let Some(st) = &w.export {
                export::view(f, body, app, st);
            }
        }
        Stage::Steps => steps_view(f, body, app, w),
        Stage::Upgrade => upgrade_view(f, body, app, w),
        Stage::Review => review_view(f, body, app, w),
    }
}

/// What the machine has for the upgrade, and the snapshot checkbox.
fn upgrade_view(f: &mut Frame, body: Rect, app: &App, w: &Wizard) {
    let theme = &app.theme;
    let [text, list] = Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).areas(body);
    let mut lines = Vec::new();
    match &app.snapshots {
        None => lines.push(Line::styled(
            format!("{} Checking for btrfs and snapshot tools{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis),
            theme.dim(),
        )),
        Some(d) => {
            for (i, l) in d.warning().into_iter().enumerate() {
                lines.push(if i == 0 { Line::styled(format!("{} {l}", theme.sym.warn), theme.warn().patch(theme.bold())) } else { Line::raw(l) });
                lines.push(Line::raw(""));
            }
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " System upgrade ")), text);
    match &w.snap {
        Some(c) => c.view(f, list, theme, "Before the upgrade"),
        None => f.render_widget(Paragraph::new("").block(block(theme, " Before the upgrade ")), list),
    }
}

fn steps_view(f: &mut Frame, body: Rect, app: &App, w: &Wizard) {
    let theme = &app.theme;
    let (list, details) = layout(body);
    w.steps.view(f, list, theme, "Automatic setup: steps");
    let mut lines = Vec::new();
    if let Some(step) = w.steps.current().and_then(|i| Step::from_key(&i.key)) {
        let (label, about, changes) = step_text(step);
        let field = |k: &'static str, v: &str| Line::from(vec![Span::styled(k, theme.bold()), Span::raw(v.to_string())]);
        if body.width >= 100 {
            lines.push(Line::styled(label, theme.title()));
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(about));
        lines.push(field("Changes:    ", changes));
        lines.push(field("Needs root: ", "yes (kast already runs as root)"));
        if step == Step::Upgrade {
            let warn = app.snapshots.as_ref().map_or_else(|| vec!["System upgrades can't be undone by kast.".to_string()], |d| d.warning());
            lines.extend(warn.into_iter().map(|l| Line::styled(l, theme.warn())));
        }
        let next = match step {
            Step::Partition => "Next: pick the partition, mount point, options and folders.",
            Step::Packages => "Next: pick the apps.",
            Step::Import => "Next: pick the backup and what to restore.",
            Step::Export => "Next: pick what to back up and where.",
            _ => "",
        };
        if !next.is_empty() {
            lines.push(Line::styled(next, theme.dim()));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " Details ")), details);
}

/// What the run will do, step by step.
pub fn review_lines(theme: &Theme, w: &Wizard) -> Vec<Line<'static>> {
    let plan = w.plan();
    let mut lines = vec![Line::styled("These steps run in this order, without further questions:", theme.bold()), Line::raw("")];
    for (i, s) in plan.steps.iter().enumerate() {
        let detail = match s {
            Step::Packages => match (&w.apps.picker, &w.list()) {
                (Some(p), Some(pl)) => {
                    let (n, m) = p.count();
                    let left: Vec<String> = p.items().filter(|i| !i.selected()).map(|i| i.label.clone()).collect();
                    let mut d = format!("{n} of {m} apps");
                    if !left.is_empty() {
                        d += &format!(" (left out: {})", left.join(", "));
                    }
                    d + &format!(" · {}", packages::sep(theme, &pl.source_line()))
                }
                _ => String::new(),
            },
            Step::Import => match &plan.import {
                Some(o) => {
                    let what = o.app.clone().unwrap_or_else(|| "everything in it".into());
                    let mut d = format!("{} → {what}", o.file.display());
                    if o.dry_run {
                        d += " (dry run)";
                    }
                    if !o.safety_backup {
                        d += ", no safety snapshot";
                    }
                    d
                }
                None => String::new(),
            },
            Step::Partition => match &plan.partition {
                Some(p) => format!("{} → {} ({})", p.part.path, p.choices.mountpoint, p.entry()),
                None => String::new(),
            },
            Step::Upgrade => {
                if plan.snapshot {
                    "snapshot first".into()
                } else {
                    "no snapshot (can't be undone by kast)".into()
                }
            }
            Step::Export => match (&plan.export, &plan.upload) {
                (Some(o), Some(up)) => {
                    let unchanged = if up.if_unchanged == crate::drive::IfUnchanged::Skip { ", skipped if unchanged" } else { "" };
                    format!("{} → Proton Drive (keeps {}{unchanged})", o.target, up.keep)
                }
                (Some(o), None) => format!("{} → {}", o.target, o.output.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
                _ => String::new(),
            },
            _ => String::new(),
        };
        let mut spans = vec![Span::styled(format!("  {}. ", i + 1), theme.bold()), Span::raw(s.label())];
        if !detail.is_empty() {
            spans.push(Span::styled(format!(": {detail}"), theme.dim()));
        }
        lines.push(Line::from(spans));
    }
    let skipped: Vec<&str> = Step::ALL.into_iter().filter(|s| !plan.has(*s)).map(Step::label).collect();
    if !skipped.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("Skipped: {}", skipped.join(", ")), theme.dim()));
    }
    if plan.has(Step::Import) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("{} The restore overwrites the settings it contains.", theme.sym.warn), theme.warn()));
    }
    lines
}

fn review_view(f: &mut Frame, body: Rect, app: &App, w: &Wizard) {
    let theme = &app.theme;
    let [text, buttons] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(body);
    f.render_widget(Paragraph::new(Text::from(review_lines(theme, w))).wrap(Wrap { trim: false }).block(block(theme, " Review ")), text);
    let button = |label: &str, on: bool, style| {
        let s = if on { theme.selected() } else { style };
        Span::styled(format!(" {} {label} ", if on { theme.sym.pointer } else { " " }), s)
    };
    let line = Line::from(vec![button("Start", w.start_focused, theme.ok()), Span::raw("   "), button("Back", !w.start_focused, ratatui::style::Style::new())]);
    f.render_widget(Paragraph::new(line).block(block(theme, "")), buttons);
}

pub fn hints(app: &App, w: &Wizard) -> &'static [(&'static str, &'static str)] {
    match w.stage {
        Stage::Steps => &[("Space", "toggle"), ("Enter", "continue"), ("Esc", "back"), ("?", "help"), ("a", "all"), ("n", "none"), ("i", "invert")],
        Stage::Upgrade => &[("Space", "toggle"), ("Enter", "continue"), ("Esc", "back"), ("?", "help")],
        Stage::Partition => w.partition.as_ref().map_or(&[], |p| super::partition::hints(p)),
        Stage::Apps => packages::hints(app),
        Stage::Import => w.import.as_ref().map_or(&[], import::hints),
        Stage::Export => w.export.as_ref().map_or(&[], export::hints),
        Stage::Review => &[("Enter", "choose"), ("←/→", "Start/Back"), ("s", "start"), ("Esc", "back"), ("?", "help")],
    }
}

pub fn help(w: &Wizard) -> (&'static str, &'static [(&'static str, &'static str)]) {
    match w.stage {
        Stage::Steps => ("Automatic setup: steps", &STEPS_HELP),
        Stage::Upgrade => ("System upgrade", &UPGRADE_HELP),
        Stage::Partition => ("Data partition", &super::partition::HELP),
        Stage::Apps => ("Apps to install", &packages::HELP),
        Stage::Import => ("Backup to restore", &import::HELP),
        Stage::Export => ("Back up", &export::HELP),
        Stage::Review => ("Review", &REVIEW_HELP),
    }
}

pub const STEPS_HELP: [(&str, &str); 7] = [
    checklist::HELP[0],
    checklist::HELP[1],
    checklist::HELP[2],
    checklist::HELP[3],
    checklist::HELP[4],
    ("Enter", "Continue: a stage for each checked step that needs choices, then a review"),
    ("Esc  q", "Back to the menu (your choices are kept)"),
];

pub const UPGRADE_HELP: [(&str, &str); 3] =
    [("Space", "Take a snapshot before upgrading, or not"), ("Enter", "Continue"), ("Esc  q", "Back to the steps (choices are kept)")];

pub const REVIEW_HELP: [(&str, &str); 4] =
    [("Enter", "Start, or Back when highlighted"), ("←/→  Tab", "Switch between Start and Back"), ("s", "Start"), ("Esc  q", "Back to the previous stage")];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{app, installed, key, loaded, online_list, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;
    use crate::tui::screens::export::TargetInfo;
    use crate::tui::screens::import::{Candidate, Inspected};
    use crate::vault::info::{InfoApp, InfoReport};
    use std::path::{Path, PathBuf};

    fn w(a: &App) -> &Wizard {
        match &a.screen {
            Screen::Auto(w) => w,
            _ => panic!("not in the automatic run's stages"),
        }
    }

    fn opened(width: u16, height: u16) -> App {
        let mut a = app(width, height);
        assert!(matches!(update(&mut a, key(KeyCode::Char('1'))), Some(Command::DetectSnapshots)), "option 1 doesn't start a job any more");
        update(&mut a, Event::Loaded(Loaded::Snapshots(crate::snapshot::test_cases()[0].1.clone())));
        loaded(&mut a, online_list());
        installed(&mut a, &["zotero"]);
        a
    }

    /// Steps → (upgrade) → the next stage.
    fn past_steps(a: &mut App) {
        update(a, key(KeyCode::Enter));
        if w(a).stage == Stage::Upgrade {
            update(a, key(KeyCode::Enter));
        }
    }

    /// Check or uncheck the step with this key.
    fn flip(a: &mut App, s: Step) {
        let i = Step::ALL.iter().position(|x| *x == s).unwrap();
        if let Screen::Auto(w) = &mut a.screen {
            w.steps.cursor = i;
        }
        update(a, key(KeyCode::Char(' ')));
    }

    #[test]
    fn defaults_are_the_standard_run() {
        let a = opened(80, 24);
        assert_eq!(w(&a).chosen(), [Step::Upgrade, Step::Repo, Step::Packages, Step::Services]);
        assert_eq!(w(&a).stages(), [Stage::Steps, Stage::Upgrade, Stage::Apps, Stage::Review]);
        assert_eq!(w(&a).plan(), AutoPlan { snapshot: true, ..AutoPlan::standard() }, "snapper without snap-pac: a snapshot by default");
    }

    #[test]
    fn no_steps_or_no_apps_is_caught() {
        let mut a = opened(80, 24);
        update(&mut a, key(KeyCode::Char('n')));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!(w(&a).stage, Stage::Steps);
        assert_eq!(a.toast.as_ref().unwrap().kind, ToastKind::Error);

        // Packages on, no app checked: offer to skip the step instead of running it empty
        let mut a = opened(80, 24);
        past_steps(&mut a);
        assert_eq!(w(&a).stage, Stage::Apps);
        update(&mut a, key(KeyCode::Char('n')));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::SkipPackages));
        // Back (the default) keeps the apps stage
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(w(&a).stage, Stage::Apps);
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Char('y')));
        assert_eq!(w(&a).stage, Stage::Review);
        assert_eq!(w(&a).chosen(), [Step::Upgrade, Step::Repo, Step::Services]);
    }

    #[test]
    fn stages_follow_the_steps_and_esc_keeps_choices() {
        let t = tempfile::tempdir().unwrap();
        let mut a = opened(80, 24);
        flip(&mut a, Step::Export);
        flip(&mut a, Step::Import);
        flip(&mut a, Step::Upgrade);
        assert_eq!(w(&a).stages(), [Stage::Steps, Stage::Apps, Stage::Import, Stage::Export, Stage::Review], "no upgrade, no upgrade stage");

        // Apps: leave gimp out
        update(&mut a, key(KeyCode::Enter));
        let i = w(&a).apps.picker.as_ref().unwrap().rows.iter().position(|r| matches!(r, Row::Item(it) if it.key == "gimp")).unwrap();
        if let Screen::Auto(w) = &mut a.screen {
            w.apps.picker.as_mut().unwrap().cursor = i;
        }
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(w(&a).stage, Stage::Import);

        // Esc goes back, and the apps are as they were
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(w(&a).stage, Stage::Apps);
        assert_eq!(w(&a).apps.picker.as_ref().unwrap().count(), (35, 36));
        update(&mut a, key(KeyCode::Enter));

        // Import: local folder → folder → file → options → "Use this"
        update(&mut a, key(KeyCode::Enter));
        if let Screen::Auto(w) = &mut a.screen {
            w.import.as_mut().unwrap().folder.set(t.path().display().to_string());
        }
        let Some(Command::LoadCandidates { dir, .. }) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a load") };
        assert_eq!(dir, t.path());
        let file = t.path().join("all_backup_2026-10-16_210400.zip");
        let cand = Candidate {
            path: file.clone(),
            name: "all_backup_2026-10-16_210400.zip".into(),
            tag: "[Full System Backup]",
            size: "1.2G".into(),
            modified: "2026-10-16 21:04".into(),
        };
        update(&mut a, Event::Loaded(Loaded::Candidates(vec![cand])));
        let Some(Command::Inspect { path, pick: true }) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the file to be opened") };
        let info = InfoReport {
            file: file.display().to_string(),
            size: "1.2G".into(),
            created: "2026-10-16T21:04:00+02:00".into(),
            host: "archbox".into(),
            user: "p3ter".into(),
            target: "all".into(),
            media: "true".into(),
            apps: vec![InfoApp { key: "kde".into(), name: "KDE".into(), fields: vec![] }, InfoApp { key: "zen".into(), name: "ZEN".into(), fields: vec![] }],
        };
        let ins = Inspected { info, destinations: vec![("kde".into(), vec!["KDE settings".into()]), ("zen".into(), vec!["/home/p3ter/.config/zen".into()])] };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(ins)) }));
        assert!(render(&a).contains("Use this restore"));
        // KDE only, then "Use this": recorded, no job, on to export
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        let cmd = update(&mut a, key(KeyCode::Char('s')));
        assert!(a.modal.is_none(), "no restore confirmation in the automatic run");
        assert!(matches!(cmd, Some(Command::LoadExportTargets)));
        assert_eq!(w(&a).stage, Stage::Export);
        assert_eq!(w(&a).import_opts.as_ref().map(|o| (o.file.clone(), o.app.clone())), Some((file.clone(), Some("kde".into()))));

        // Export: a folder that doesn't exist yet is fine (the backup step creates it)
        update(&mut a, key(KeyCode::Enter));
        let dest_dir = t.path().join("new");
        if let Screen::Auto(w) = &mut a.screen {
            w.export.as_mut().unwrap().folder.set(dest_dir.display().to_string());
        }
        update(&mut a, key(KeyCode::Enter));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none(), "no folder is created now");
        let targets = export::TARGETS.iter().map(|(k, _)| TargetInfo { key: k, detected: true, size: String::new(), location: String::new() }).collect();
        update(&mut a, Event::Loaded(Loaded::ExportTargets { targets, ts: "2026-10-17_101500".into() }));
        assert!(update(&mut a, key(KeyCode::Char('s'))).is_none());
        assert_eq!(w(&a).stage, Stage::Review);

        let plan = w(&a).plan();
        assert_eq!(plan.steps, [Step::Export, Step::Repo, Step::Packages, Step::Services, Step::Import]);
        assert_eq!(plan.packages.as_ref().map(Vec::len), Some(35));
        assert!(!plan.packages.as_ref().unwrap().contains(&"gimp".to_string()));
        assert_eq!(plan.export.as_ref().unwrap().output.as_deref(), Some(dest_dir.join("all_backup_2026-10-17_101500.zip").as_path()));
        assert_eq!(plan.import.as_ref().unwrap().app.as_deref(), Some("kde"));
        let review = render(&a);
        assert!(review.contains("35 of 36 apps (left out: gimp)"), "{review}");

        // Back to the menu and in again: everything is still there
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(w(&a).stage, Stage::Export);
        for _ in 0..3 {
            update(&mut a, key(KeyCode::Esc));
        }
        assert_eq!(w(&a).stage, Stage::Import);
        if let Screen::Auto(w) = &mut a.screen {
            w.stage = Stage::Steps;
        }
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
        update(&mut a, key(KeyCode::Char('1')));
        assert_eq!(w(&a).plan(), plan);

        // Start from the review
        if let Screen::Auto(w) = &mut a.screen {
            w.stage = Stage::Review;
        }
        let Some(Command::Start(JobKind::Auto(Some(_), started))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the run to start") };
        assert_eq!(*started, plan);
        let Screen::Run(r) = &a.screen else { panic!("run screen") };
        assert_eq!(r.steps, ["Export data", "kud3n's repository", "Install packages", "Services", "Import data"]);
    }

    #[test]
    fn export_to_proton_drive_is_decided_while_configuring() {
        let mut a = app(140, 40);
        a.drive = Some(Box::new(super::super::drive::tests::info(Some(crate::proton::Status::SignedIn))));
        update(&mut a, key(KeyCode::Char('1')));
        for s in [Step::Upgrade, Step::Repo, Step::Packages, Step::Services, Step::Export] {
            flip(&mut a, s);
        }
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::LoadExportTargets)));
        assert_eq!(w(&a).stage, Stage::Export);
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Enter));
        let targets = export::TARGETS.iter().map(|(k, _)| TargetInfo { key: k, detected: true, size: String::new(), location: String::new() }).collect();
        update(&mut a, Event::Loaded(Loaded::ExportTargets { targets, ts: "2026-10-17_101500".into() }));
        let out = render(&a);
        assert!(out.contains("Skip the upload if nothing changed since the last one"), "{out}");
        insta::assert_snapshot!("wizard_export_proton_140x40", out);
        assert!(update(&mut a, key(KeyCode::Char('s'))).is_none());
        assert_eq!(w(&a).stage, Stage::Review);
        let plan = w(&a).plan();
        let up = plan.upload.clone().expect("an upload");
        assert_eq!((up.if_unchanged, up.retention_ok, up.keep), (crate::drive::IfUnchanged::Skip, true, 5));
        assert!(render(&a).contains("all → Proton Drive (keeps 5, skipped if unchanged)"));
        let Some(Command::Start(JobKind::Auto(_, p))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the run") };
        assert_eq!(p.steps, [Step::Export]);
        assert!(p.upload.is_some());
    }

    #[test]
    fn plan_without_packages_needs_no_list() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('1')));
        flip(&mut a, Step::Packages);
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(w(&a).stage, Stage::Upgrade);
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!(w(&a).stage, Stage::Upgrade, "waits for the snapshot check");
        update(&mut a, Event::Loaded(Loaded::Snapshots(crate::snapshot::test_cases()[4].1.clone())));
        assert!(!w(&a).snap.as_ref().unwrap().item("snapshot").unwrap().enabled, "ext4: nothing to take it with");
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(w(&a).stage, Stage::Review);
        let Some(Command::Start(JobKind::Auto(None, plan))) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a start") };
        assert_eq!(plan.steps, [Step::Upgrade, Step::Repo, Step::Services]);
        assert!(Path::new("/").exists());
        let _ = PathBuf::new();
    }

    #[test]
    fn the_data_partition_is_chosen_while_configuring() {
        use crate::tui::screens::partition::tests::scan;
        let mut a = opened(140, 40);
        a.now = 1_792_400_000;
        for s in [Step::Upgrade, Step::Repo, Step::Packages, Step::Services, Step::Partition] {
            flip(&mut a, s);
        }
        assert_eq!(w(&a).stages(), [Stage::Steps, Stage::Partition, Stage::Review]);
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::LoadPartitions)));
        assert_eq!(w(&a).stage, Stage::Partition);
        update(&mut a, Event::Loaded(Loaded::Partitions(Ok(scan()))));
        // /dev/sdc1 (ext4), mount point, options, folders: Documents
        if let Screen::Auto(w) = &mut a.screen {
            w.partition.as_mut().unwrap().cursor = 2;
        }
        update(&mut a, key(KeyCode::Enter));
        let Some(Command::CheckMountpoint(raw)) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected a check") };
        update(&mut a, Event::Loaded(Loaded::Mountpoint { raw, result: Ok(("/media/backup".into(), crate::partition::fstab::Spot::Missing)) }));
        update(&mut a, key(KeyCode::Char('c')));
        let i = w(&a).partition.as_ref().unwrap().folders.rows.iter().position(|r| matches!(r, Row::Item(it) if it.key == "Documents")).unwrap();
        if let Screen::Auto(w) = &mut a.screen {
            w.partition.as_mut().unwrap().folders.cursor = i;
        }
        update(&mut a, key(KeyCode::Char(' ')));
        // Enter ends the partition stages: no review of its own, on to the run's review
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(w(&a).stage, Stage::Review);
        let plan = w(&a).plan();
        let p = plan.partition.as_ref().expect("a partition plan");
        assert_eq!((p.part.path.as_str(), p.choices.mountpoint.as_str(), p.folders.as_slice()), ("/dev/sdc1", "/media/backup", &["Documents".to_string()][..]));
        assert!(plan.has(Step::Partition));
        let out = render(&a);
        assert!(out.contains("Data partition: /dev/sdc1 → /media/backup (UUID=9d8a0c4e"), "{out}");
        insta::assert_snapshot!("wizard_review_partition_140x40", out);
        // Esc goes back into the partition stages with everything as it was
        update(&mut a, key(KeyCode::Esc));
        assert_eq!(w(&a).stage, Stage::Partition);
        assert_eq!(w(&a).partition.as_ref().unwrap().choices.as_ref().unwrap().mountpoint, "/media/backup");
        // Unchecking the step drops the plan
        if let Screen::Auto(w) = &mut a.screen {
            w.stage = Stage::Steps;
        }
        flip(&mut a, Step::Partition);
        assert!(w(&a).plan().partition.is_none() && !w(&a).plan().has(Step::Partition));
    }

    #[test]
    fn a_run_with_the_partition_step_needs_a_partition() {
        let mut a = opened(80, 24);
        flip(&mut a, Step::Partition);
        if let Screen::Auto(w) = &mut a.screen {
            w.stage = Stage::Review;
        }
        assert!(update(&mut a, key(KeyCode::Char('s'))).is_none());
        assert!(a.toast.as_ref().unwrap().text.contains("no partition chosen"));
    }

    #[test]
    fn snapshot_steps_and_review() {
        for (wd, h) in [(80, 24), (140, 40)] {
            let mut a = opened(wd, h);
            flip(&mut a, Step::Partition);
            insta::assert_snapshot!(format!("steps_{wd}x{h}"), render(&a));
            past_steps(&mut a);
            update(&mut a, key(KeyCode::Char('n')));
            update(&mut a, key(KeyCode::Char('a')));
            update(&mut a, key(KeyCode::End));
            update(&mut a, key(KeyCode::Char(' ')));
            update(&mut a, key(KeyCode::Enter));
            insta::assert_snapshot!(format!("review_{wd}x{h}"), render(&a));
        }
    }

    #[test]
    fn upgrade_warning_for_each_machine() {
        for (name, d) in crate::snapshot::test_cases() {
            for (wd, h) in [(80, 24), (140, 40)] {
                let mut a = app(wd, h);
                update(&mut a, key(KeyCode::Char('1')));
                update(&mut a, Event::Loaded(Loaded::Snapshots(d.clone())));
                update(&mut a, key(KeyCode::Enter));
                assert_eq!(w(&a).stage, Stage::Upgrade);
                let out = render(&a);
                assert!(out.contains("System upgrades can't be undone by kast."), "{name}");
                insta::assert_snapshot!(format!("upgrade_{name}_{wd}x{h}"), out);
                // The choice ends up in the plan
                assert_eq!(w(&a).plan().snapshot, d.default_on(), "{name}");
                if d.offered() {
                    update(&mut a, key(KeyCode::Char(' ')));
                    assert_eq!(w(&a).plan().snapshot, !d.default_on(), "{name}");
                }
            }
        }
    }
}
