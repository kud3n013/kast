//! Back up: pick what (with what kas detects and how big it is), the options that apply to
//! it and where the file goes, then confirm.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::tui::app::{App, Command, Modal, ModalKind, Screen};
use crate::tui::job::JobKind;
use crate::tui::widgets::{Input, block, popup};
use crate::vault::backup::BackupOpts;

pub const TARGETS: [(&str, &str); 11] = [
    ("all", "All detected apps & Howdy face model"),
    ("beeper", "BeeperTexts (~/.config/BeeperTexts)"),
    ("zalo", "ZaloData (~/.config/ZaloData)"),
    ("vesktop", "Vesktop (~/.config/vesktop)"),
    ("zen", "Zen Browser (~/.config/zen)"),
    ("obsidian", "Obsidian (~/.config/obsidian)"),
    ("fcitx5", "Fcitx5 (~/.config/fcitx5)"),
    ("antigravity", "Antigravity (~/.config/Antigravity)"),
    ("howdy", "Howdy face model (/etc/howdy/models)"),
    ("kde", "KDE Plasma settings (curated)"),
    ("hypr", "Hyprland settings (~/.config/hypr)"),
];

/// What kas found for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    pub key: &'static str,
    pub detected: bool,
    pub size: String,
    pub location: String,
}

/// Detect every target and its size (runs `du`, so off the UI thread).
pub fn load_targets() -> Vec<TargetInfo> {
    use crate::vault::{find_config, find_howdy_model, format_bytes, kde_format_total_size, kde_has_settings};
    let c = crate::ctx::ctx();
    let mut out: Vec<TargetInfo> = TARGETS
        .iter()
        .skip(1)
        .map(|(key, _)| {
            let (detected, size, location) = match *key {
                "howdy" => match find_howdy_model(&c.target_user) {
                    Some(p) => (true, format_bytes(&p), p.display().to_string()),
                    None => (false, String::new(), String::new()),
                },
                "kde" if kde_has_settings() => (true, kde_format_total_size(), c.config_home.display().to_string()),
                "kde" => (false, String::new(), String::new()),
                app => match find_config(app) {
                    Some(p) => (true, format_bytes(&p), p.display().to_string()),
                    None => (false, String::new(), String::new()),
                },
            };
            TargetInfo { key, detected, size, location }
        })
        .collect();
    let any = out.iter().any(|t| t.detected);
    out.insert(0, TargetInfo { key: "all", detected: any, size: String::new(), location: String::new() });
    out
}

/// Where backups go by default, and the timestamp for the file name.
pub fn default_destination() -> (PathBuf, String) {
    let dir = if Path::new("/media/data").is_dir() {
        PathBuf::from("/media/data/backups")
    } else {
        crate::ctx::ctx().target_home.join(".local/share/app-vault-sync/backups")
    };
    (dir, crate::ctx::now_str("%Y-%m-%d_%H%M%S"))
}

#[derive(Debug, Clone)]
pub struct ExportState {
    pub targets: Vec<TargetInfo>,
    pub loading: bool,
    pub target: usize,
    pub focus: usize,
    pub include_media: bool,
    pub dat: bool,
    pub kill: bool,
    pub dest: Input,
    pub dest_edited: bool,
    pub editing: bool,
    pub dir: PathBuf,
    pub ts: String,
    pub user: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Target(usize),
    Media,
    Format,
    Kill,
    Dest,
    Start,
}

impl ExportState {
    pub fn new(user: &str) -> Self {
        ExportState {
            targets: Vec::new(),
            loading: true,
            target: 0,
            focus: 0,
            include_media: true,
            dat: true,
            kill: true,
            dest: Input::default(),
            dest_edited: false,
            editing: false,
            dir: PathBuf::new(),
            ts: String::new(),
            user: user.to_string(),
        }
    }

    fn key(&self) -> &'static str {
        TARGETS[self.target].0
    }

    fn media_applies(&self) -> bool {
        !matches!(self.key(), "howdy" | "kde" | "hypr")
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = (0..TARGETS.len()).map(Row::Target).collect();
        if self.media_applies() {
            rows.push(Row::Media);
        }
        if self.key() == "howdy" {
            rows.push(Row::Format);
        }
        rows.extend([Row::Kill, Row::Dest, Row::Start]);
        rows
    }

    /// File name for the current choice (`kas export` used the same names).
    fn default_path(&self) -> PathBuf {
        if self.key() == "howdy" && self.dat {
            self.dir.join(format!("{}_face_model_{}.dat", self.user, self.ts))
        } else {
            self.dir.join(format!("{}_backup_{}.zip", self.key(), self.ts))
        }
    }

    fn refresh_dest(&mut self) {
        if !self.dest_edited {
            self.dest.set(self.default_path().display().to_string());
        }
    }

    pub fn opts(&self) -> BackupOpts {
        BackupOpts {
            target: self.key().to_string(),
            output: Some(PathBuf::from(self.dest.value.trim())),
            include_media: self.include_media || !self.media_applies(),
            kill: self.kill,
            force: false,
            dry_run: false,
        }
    }
}

fn state(app: &mut App) -> Option<&mut ExportState> {
    match &mut app.screen {
        Screen::Export(s) => Some(s),
        _ => None,
    }
}

pub fn on_targets(app: &mut App, targets: Vec<TargetInfo>, dir: PathBuf, ts: String) {
    if let Some(st) = state(app) {
        st.targets = targets;
        st.loading = false;
        st.dir = dir;
        st.ts = ts;
        st.refresh_dest();
    }
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(st) = state(app)
        && st.editing
    {
        st.dest.insert(text.trim());
        st.dest_edited = true;
    }
}

pub fn editing(app: &App) -> bool {
    matches!(&app.screen, Screen::Export(s) if s.editing)
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    if st.editing {
        match k.code {
            KeyCode::Enter | KeyCode::Esc => st.editing = false,
            KeyCode::Tab => return Some(Command::Complete(st.dest.value.clone())),
            _ => {
                if st.dest.key(&k) {
                    st.dest_edited = true;
                }
            }
        }
        return None;
    }
    if st.loading {
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
            app.screen = Screen::Menu;
        }
        return None;
    }
    let rows = st.rows();
    let n = rows.len();
    st.focus = st.focus.min(n - 1);
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => st.focus = st.focus.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.focus = (st.focus + 1) % n,
        KeyCode::Home | KeyCode::Char('g') => st.focus = 0,
        KeyCode::End | KeyCode::Char('G') => st.focus = n - 1,
        KeyCode::Esc | KeyCode::Char('q') => app.screen = Screen::Menu,
        KeyCode::Char('s') => return confirm(app),
        KeyCode::Char(' ') | KeyCode::Enter => match rows[st.focus] {
            Row::Target(i) => {
                st.target = i;
                st.refresh_dest();
            }
            Row::Media => st.include_media = !st.include_media,
            Row::Format => {
                st.dat = !st.dat;
                st.refresh_dest();
            }
            Row::Kill => st.kill = !st.kill,
            Row::Dest => st.editing = true,
            Row::Start => return confirm(app),
        },
        _ => {}
    }
    None
}

pub fn on_completion(app: &mut App, value: String) {
    if let Some(st) = state(app)
        && st.editing
    {
        st.dest.set(value);
        st.dest_edited = true;
    }
}

fn confirm(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    let opts = st.opts();
    let dest = st.dest.value.trim().to_string();
    if dest.is_empty() {
        st.focus = st.rows().iter().position(|r| *r == Row::Dest).unwrap_or(0);
        st.editing = true;
        return None;
    }
    let mut details = vec![format!("What:        {}", TARGETS[st.target].1), format!("Destination: {dest}")];
    if st.media_applies() {
        details.push(format!("Media:       {}", if st.include_media { "included" } else { "left out (smaller archive)" }));
    }
    details.push(if st.kill {
        "Running apps among these are closed first, so databases are consistent.".into()
    } else {
        "If any of these apps run, you will be asked what to do.".into()
    });
    if Path::new(&dest).exists() {
        details.push("The destination file already exists and will be replaced.".into());
    }
    app.modal = Some(Modal {
        kind: ModalKind::Confirm(Box::new(JobKind::Export(opts))),
        title: "Back up".into(),
        text: "Start this backup?".into(),
        details,
        yes: "Back up",
        no: "Back",
        yes_selected: true,
    });
    None
}

pub fn mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    let st = state(app)?;
    let n = st.rows().len();
    match m.kind {
        MouseEventKind::ScrollUp => st.focus = st.focus.saturating_sub(1),
        MouseEventKind::ScrollDown => st.focus = (st.focus + 1).min(n - 1),
        _ => {}
    }
    None
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &ExportState) {
    let theme = &app.theme;
    let wide = body.width >= 100;
    let (list_area, panel) = if wide {
        let [l, p] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        (l, Some(p))
    } else {
        (body, None)
    };
    let radio = |on: bool| if on { theme.sym.radio_on } else { "( )" };
    let check = |on: bool| if on { theme.sym.on } else { theme.sym.off };
    let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
    let items: Vec<ListItem> = st
        .rows()
        .into_iter()
        .map(|row| match row {
            Row::Target(i) => {
                let info = st.targets.get(i);
                let state = match info {
                    None => Span::styled(format!("  {spinner}"), theme.dim()),
                    Some(t) if t.detected && t.size.is_empty() => Span::styled(format!("  {}", theme.sym.ok), theme.ok()),
                    Some(t) if t.detected => Span::styled(format!("  {} {}", theme.sym.ok, t.size), theme.ok()),
                    Some(_) => Span::styled("  not found", theme.dim()),
                };
                ListItem::new(Line::from(vec![Span::raw(format!("{} {}", radio(i == st.target), TARGETS[i].1)), state]))
            }
            Row::Media => ListItem::new(format!("{} Include large media (attachments, site data)", check(st.include_media))),
            Row::Format => ListItem::new(format!("{} Standalone .dat (otherwise a .zip with manifest)", check(st.dat))),
            Row::Kill => ListItem::new(format!("{} Close running apps first", check(st.kill))),
            Row::Dest => {
                let style = if st.dest_edited { theme.bold() } else { ratatui::style::Style::new() };
                ListItem::new(Line::from(vec![Span::raw("Save to: "), Span::styled(st.dest.value.clone(), style)]))
            }
            Row::Start => ListItem::new(Line::styled(format!("{} Start backup", theme.sym.start), theme.ok())),
        })
        .collect();
    let list = List::new(items).block(block(theme, " Back up ")).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(st.focus));
    f.render_stateful_widget(list, list_area, &mut ls);

    if let Some(p) = panel {
        let mut lines = vec![Line::styled(TARGETS[st.target].1, theme.title()), Line::raw("")];
        match st.targets.get(st.target) {
            None => lines.push(Line::styled(format!("Looking for apps{}", theme.sym.ellipsis), theme.dim())),
            Some(t) if !t.detected => lines.push(Line::styled(format!("{} Nothing found to back up for this target.", theme.sym.warn), theme.warn())),
            Some(t) => {
                if !t.location.is_empty() {
                    lines.push(Line::from(vec![Span::styled("Found:   ", theme.bold()), Span::raw(t.location.clone())]));
                }
                if !t.size.is_empty() {
                    lines.push(Line::from(vec![Span::styled("Size:    ", theme.bold()), Span::raw(t.size.clone())]));
                }
                if t.key == "all" {
                    let found: Vec<&str> = st.targets.iter().skip(1).filter(|t| t.detected).map(|t| t.key).collect();
                    lines.push(Line::raw(format!("Includes: {}", found.join(", "))));
                }
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("The archive holds logins and cookies: it is created readable by you only (0600).", theme.dim()));
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Details ")), p);
    }

    if st.editing {
        let r = popup(f, body, body.width.saturating_sub(8).min(100), 4);
        let blk = block(theme, " Save the backup to ");
        let inner = blk.inner(r);
        f.render_widget(blk, r);
        let [field, hint] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        st.dest.render(f, field, theme, true);
        f.render_widget(Paragraph::new(Line::styled("Tab completes, Enter or Esc closes", theme.dim())), hint);
    }
}

pub fn hints(st: &ExportState) -> &'static [(&'static str, &'static str)] {
    if st.editing {
        &[("Tab", "complete"), ("Enter", "done")]
    } else {
        &[("↑/↓", "move"), ("Space", "select/toggle"), ("s", "start"), ("Esc", "back"), ("?", "help")]
    }
}

pub const HELP: [(&str, &str); 6] = [
    ("↑/↓  j/k", "Move"),
    ("Space  Enter", "Select a target, toggle an option, edit the path"),
    ("s", "Start the backup (asks first)"),
    ("Tab", "Path field: complete"),
    ("Esc  q", "Back to the menu"),
    ("?", "This help"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;

    fn loaded(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        assert!(matches!(update(&mut a, key(KeyCode::Char('7'))), Some(Command::LoadExportTargets)));
        let mut targets: Vec<TargetInfo> =
            TARGETS.iter().map(|(k, _)| TargetInfo { key: k, detected: false, size: String::new(), location: String::new() }).collect();
        for (i, size) in [(0, ""), (1, "2.1G"), (4, "512M"), (8, "8.0K"), (9, "96M")] {
            targets[i].detected = true;
            targets[i].size = size.into();
            targets[i].location = format!("/home/p3ter/.config/{}", targets[i].key);
        }
        update(&mut a, Event::Loaded(Loaded::ExportTargets { targets, dir: "/media/data/backups".into(), ts: "2026-10-17_101500".into() }));
        a
    }

    fn st(a: &App) -> &ExportState {
        match &a.screen {
            Screen::Export(s) => s,
            _ => panic!("not on the export screen"),
        }
    }

    #[test]
    fn destination_follows_target_until_edited() {
        let mut a = loaded(80, 24);
        assert_eq!(st(&a).dest.value, "/media/data/backups/all_backup_2026-10-17_101500.zip");
        // howdy: .dat by default, no media option
        for _ in 0..8 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).dest.value, "/media/data/backups/p3ter_face_model_2026-10-17_101500.dat");
        assert!(!st(&a).rows().contains(&Row::Media));
        // Format row comes right after the targets
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).dest.value, "/media/data/backups/howdy_backup_2026-10-17_101500.zip");
        // Edit the path: later target changes keep it
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Enter));
        assert!(st(&a).editing);
        update(&mut a, key(KeyCode::Backspace));
        update(&mut a, key(KeyCode::Backspace));
        update(&mut a, key(KeyCode::Backspace));
        update(&mut a, Event::Paste("zip".into()));
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, key(KeyCode::Home));
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).dest.value, "/media/data/backups/howdy_backup_2026-10-17_101500.zip");
        update(&mut a, key(KeyCode::Char('s')));
        let m = a.modal.as_ref().unwrap();
        assert!(m.yes_selected);
        let ModalKind::Confirm(k) = &m.kind else { panic!() };
        let JobKind::Export(o) = k.as_ref() else { panic!() };
        assert_eq!((o.target.as_str(), o.kill, o.force, o.include_media), ("all", true, false, true));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(JobKind::Export(_)))));
    }

    #[test]
    fn snapshot_export_80x24() {
        insta::assert_snapshot!(render(&loaded(80, 24)));
    }

    #[test]
    fn snapshot_export_140x40() {
        let mut a = loaded(140, 40);
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        insta::assert_snapshot!(render(&a));
    }
}
