//! Back up: where to (a local folder; Proton Drive later), which folder (created as you if
//! needed), then what (with what kast detects and how big it is), the options that apply to
//! it and the file name, then confirm. In the automatic run the last button is "Use this",
//! which records the backup for the run.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::drive;
use super::import::{Source, check_dir, expand, folder_box, proton_gate, source_rows};
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

/// What kast found for one target.
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

/// Where backups go by default: the data partition when it's there.
pub fn default_dir(home: &str, exists: fn(&Path) -> bool) -> String {
    if exists(Path::new("/media/data")) { "/media/data/backups".to_string() } else { format!("{home}/.local/share/app-vault-sync/backups") }
}

/// The timestamp for the file name.
pub fn timestamp() -> String {
    crate::ctx::now_str("%Y-%m-%d_%H%M%S")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Local folder or Proton Drive
    Source,
    /// The folder the backup goes to
    Folder,
    /// What to back up, the options and the file name
    What,
}

#[derive(Debug, Clone)]
pub struct ExportState {
    pub stage: Stage,
    /// A stage of the automatic run: the last button records the backup instead
    pub auto: bool,
    pub source: usize,
    pub folder: Input,
    pub folder_error: Option<String>,
    /// The typed folder doesn't exist: the next Enter creates it
    pub create_offer: Option<PathBuf>,
    pub completions: Vec<String>,
    pub targets: Vec<TargetInfo>,
    pub loading: bool,
    pub target: usize,
    pub focus: usize,
    pub include_media: bool,
    pub dat: bool,
    pub kill: bool,
    /// The file name inside `dir` (an absolute path typed here wins)
    pub dest: Input,
    pub dest_edited: bool,
    pub editing: bool,
    pub dir: PathBuf,
    pub ts: String,
    pub user: String,
    /// Backing up to Proton Drive (the folder is then only for an optional local copy)
    pub proton: bool,
    /// The Drive folder it goes to, and the retention, shown on screen
    pub remote: String,
    pub keep: u32,
    /// Also keep the archive in `dir`
    pub local_copy: bool,
    /// Automatic run: when nothing changed since the last upload, don't upload
    pub skip_unchanged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Target(usize),
    Media,
    Format,
    Kill,
    Dest,
    /// Proton Drive: also keep a local copy
    LocalCopy,
    /// Proton Drive in the automatic run: skip an unchanged upload
    Unchanged,
    Start,
}

impl ExportState {
    pub fn new(user: &str, auto: bool, folder: String) -> Self {
        ExportState {
            stage: Stage::Source,
            auto,
            source: 0,
            folder: Input::new(folder),
            folder_error: None,
            create_offer: None,
            completions: Vec::new(),
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
            proton: false,
            remote: String::new(),
            keep: crate::settings::DEFAULT_KEEP,
            local_copy: false,
            skip_unchanged: true,
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
        rows.push(Row::Kill);
        if self.proton {
            rows.push(Row::LocalCopy);
            if self.auto {
                rows.push(Row::Unchanged);
            }
        } else {
            rows.push(Row::Dest);
        }
        rows.push(Row::Start);
        rows
    }

    /// File name for the current choice (`kast export` used the same names).
    fn default_name(&self) -> String {
        if self.key() == "howdy" && self.dat { format!("{}_face_model_{}.dat", self.user, self.ts) } else { format!("{}_backup_{}.zip", self.key(), self.ts) }
    }

    fn refresh_dest(&mut self) {
        if !self.dest_edited {
            self.dest.set(self.default_name());
        }
    }

    /// Where the file goes: the name inside the chosen folder.
    pub fn output(&self) -> PathBuf {
        self.dir.join(self.dest.value.trim())
    }

    pub fn opts(&self) -> BackupOpts {
        BackupOpts {
            target: self.key().to_string(),
            output: Some(self.output()),
            include_media: self.include_media || !self.media_applies(),
            kill: self.kill,
            force: false,
            dry_run: false,
            ..Default::default()
        }
    }

    /// How the upload goes (Proton Drive).
    pub fn upload(&self) -> crate::drive::UploadOpts {
        crate::drive::UploadOpts {
            keep: self.keep,
            also_local: self.local_copy.then(|| self.dir.clone()),
            if_unchanged: match (self.auto, self.skip_unchanged) {
                (false, _) => crate::drive::IfUnchanged::Ask,
                (true, true) => crate::drive::IfUnchanged::Skip,
                (true, false) => crate::drive::IfUnchanged::Upload,
            },
            // The automatic run is unattended: agreeing to the review covers retention
            retention_ok: self.auto,
            name_target: None,
        }
    }

    /// Start backing up to Proton Drive: straight to "what" (the folder is the local copy's).
    pub fn use_proton(&mut self, d: &drive::DriveInfo, home: &str) {
        self.proton = true;
        self.remote = d.host_folder();
        self.keep = d.keep;
        self.dir = expand(&self.folder.value, home);
        self.stage = Stage::What;
        self.focus = 0;
        self.refresh_dest();
    }
}

fn state(app: &mut App) -> Option<&mut ExportState> {
    match &mut app.screen {
        Screen::Export(s) => Some(s),
        Screen::Auto(w) => w.export_mut(),
        _ => None,
    }
}

fn state_ref(app: &App) -> Option<&ExportState> {
    match &app.screen {
        Screen::Export(s) => Some(s),
        Screen::Auto(w) => w.export_ref(),
        _ => None,
    }
}

/// Targets found, plus the timestamp for the file names.
pub fn on_targets(app: &mut App, targets: Vec<TargetInfo>, ts: String) {
    if let Some(st) = state(app) {
        st.targets = targets;
        st.loading = false;
        st.ts = ts;
        st.refresh_dest();
    }
}

/// The folder was created (or not) after the user said so.
pub fn on_dir_created(app: &mut App, dir: PathBuf, result: Result<(), String>) {
    if let Some(st) = state(app) {
        match result {
            Ok(()) => {
                st.dir = dir;
                st.stage = Stage::What;
                st.refresh_dest();
            }
            Err(e) => st.folder_error = Some(e),
        }
    }
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(st) = state(app) {
        if st.stage == Stage::Folder {
            st.folder.insert(text.trim());
            st.folder_error = None;
            st.create_offer = None;
        } else if st.editing {
            st.dest.insert(text.trim());
            st.dest_edited = true;
        }
    }
}

/// A text field has the keyboard.
pub fn editing(app: &App) -> bool {
    state_ref(app).is_some_and(|s| s.editing || s.stage == Stage::Folder)
}

fn leave(app: &mut App, auto: bool) {
    if auto {
        super::wizard::back(app);
    } else {
        app.screen = Screen::Menu;
    }
}

/// Enter in the folder field: on to "what" when it's a folder; when it doesn't exist, say
/// so and create it (as the user) on a second Enter.
fn accept_folder(app: &mut App) -> Option<Command> {
    let home = app.header.home.clone();
    let user = app.header.user.clone();
    let st = state(app)?;
    let path = expand(&st.folder.value, &home);
    if st.create_offer.as_ref() == Some(&path) {
        st.create_offer = None;
        if st.auto {
            // The backup step creates it, after the partition step has mounted /media/data
            st.dir = path;
            st.stage = Stage::What;
            st.refresh_dest();
            return None;
        }
        return Some(Command::MkdirUser(path));
    }
    if st.folder.value.trim().is_empty() {
        st.folder_error = Some("Type a folder".into());
    } else if !path.exists() {
        st.folder_error = Some(format!("{} doesn't exist. Press Enter again to create it (as {user}).", path.display()));
        st.create_offer = Some(path);
    } else {
        match check_dir(&st.folder.value, &home) {
            Ok(dir) => {
                st.folder_error = None;
                st.completions.clear();
                st.dir = dir;
                st.stage = Stage::What;
                st.refresh_dest();
            }
            Err(e) => st.folder_error = Some(e),
        }
    }
    None
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let auto = st.auto;
    match st.stage {
        Stage::Source => {
            if Source::ALL[st.source.min(1)] == Source::ProtonDrive
                && let Some(cmd) = drive::row_key(app, &k)
            {
                return cmd;
            }
            let st = state(app)?;
            match k.code {
                KeyCode::Char('s') if !auto => return super::schedule::open(app),
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.source = 1 - st.source.min(1),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if Source::ALL[st.source.min(1)] == Source::Local {
                        st.stage = Stage::Folder;
                    } else if let Err(cmd) = proton_gate(app) {
                        return cmd;
                    } else if let Some(d) = app.drive.clone() {
                        let home = app.header.home.clone();
                        state(app)?.use_proton(&d, &home);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => leave(app, auto),
                _ => {}
            }
            return None;
        }
        Stage::Folder => {
            return match k.code {
                KeyCode::Esc => {
                    st.stage = Stage::Source;
                    st.folder_error = None;
                    st.create_offer = None;
                    st.completions.clear();
                    None
                }
                KeyCode::Tab => Some(Command::Complete(st.folder.value.clone())),
                KeyCode::Enter => accept_folder(app),
                _ => {
                    if st.folder.key(&k) {
                        st.folder_error = None;
                        st.create_offer = None;
                        st.completions.clear();
                    }
                    None
                }
            };
        }
        Stage::What => {}
    }
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
            st.stage = Stage::Folder;
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
        KeyCode::Esc | KeyCode::Char('q') => {
            st.stage = if st.proton { Stage::Source } else { Stage::Folder };
            st.proton = false;
        }
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
            Row::LocalCopy => st.local_copy = !st.local_copy,
            Row::Unchanged => st.skip_unchanged = !st.skip_unchanged,
            Row::Start => return confirm(app),
        },
        _ => {}
    }
    None
}

/// The upload's confirmation (or, in the automatic run, recording it).
fn confirm_upload(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    let mut opts = st.opts();
    // Only the format matters: the engine names the file
    opts.output = Some(PathBuf::from(st.default_name()));
    let up = st.upload();
    if st.auto {
        return super::wizard::export_upload_done(app, opts, up);
    }
    let mut details = vec![format!("What:     {}", TARGETS[st.target].1), format!("Uploads:  {}/{}", st.remote, st.default_name())];
    if st.media_applies() {
        details.push(format!("Media:    {}", if st.include_media { "included" } else { "left out (smaller archive)" }));
    }
    details.push(match &up.also_local {
        Some(d) => format!("A copy stays in {}", d.display()),
        None => "Built in a private folder and removed after the upload.".into(),
    });
    details.push(format!("Keeps the newest {} on Drive; older ones go to its trash.", up.keep));
    details.push(if st.kill {
        "Running apps among these are closed first and reopened in the background after.".into()
    } else {
        "If any of these apps run, you will be asked what to do.".into()
    });
    app.modal = Some(Modal {
        kind: ModalKind::Confirm(Box::new(JobKind::Upload(opts, up))),
        title: "Back up to Proton Drive".into(),
        text: "Start this backup and upload it?".into(),
        details,
        yes: "Back up",
        no: "Back",
        yes_selected: true,
    });
    None
}

pub fn on_completion(app: &mut App, value: String, matches: Vec<String>) {
    if let Some(st) = state(app) {
        if st.stage == Stage::Folder {
            st.folder.set(value);
            st.completions = matches;
        } else if st.editing {
            st.dest.set(value);
            st.dest_edited = true;
        }
    }
}

fn confirm(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    if st.proton {
        return confirm_upload(app);
    }
    let opts = st.opts();
    if st.dest.value.trim().is_empty() {
        st.focus = st.rows().iter().position(|r| *r == Row::Dest).unwrap_or(0);
        st.editing = true;
        return None;
    }
    if st.auto {
        return super::wizard::export_done(app, opts);
    }
    let dest = st.output().display().to_string();
    let mut details = vec![format!("What:        {}", TARGETS[st.target].1), format!("Destination: {dest}")];
    if st.media_applies() {
        details.push(format!("Media:       {}", if st.include_media { "included" } else { "left out (smaller archive)" }));
    }
    details.push(if st.kill {
        "Running apps among these are closed first, so databases are consistent, and reopened in the background after.".into()
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
    if st.stage != Stage::What {
        return None;
    }
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
    match st.stage {
        Stage::Source => {
            let [list_area, details] = Layout::vertical([Constraint::Length(Source::ALL.len() as u16 + 2), Constraint::Min(3)]).areas(body);
            let (list, mut ls) = source_rows(app, st.source);
            f.render_stateful_widget(list.block(block(theme, " Back up to ")), list_area, &mut ls);
            let text = if Source::ALL[st.source.min(1)] == Source::Local {
                Text::raw("A folder on this computer: the data partition, a USB drive. Next: choose the folder.")
            } else {
                Text::from(drive::details(app, true))
            };
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block(theme, " Details ")), details);
            return;
        }
        Stage::Folder => {
            let h = 5 + st.completions.len().min(8) as u16 + u16::from(!st.completions.is_empty()) + u16::from(st.folder_error.is_some());
            let [area, _] = Layout::vertical([Constraint::Length(h.min(body.height)), Constraint::Min(0)]).areas(body);
            let help = "Tab completes, Enter uses this folder (created if missing), Esc goes back";
            let d = super::import::Folder {
                title: "Folder for the backup",
                input: &st.folder,
                error: st.folder_error.as_deref(),
                completions: &st.completions,
                help,
            };
            folder_box(f, area, theme, &d);
            return;
        }
        Stage::What => {}
    }
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
            Row::Kill => ListItem::new(format!("{} Close running apps first (reopened in the background)", check(st.kill))),
            Row::LocalCopy => ListItem::new(format!("{} Also keep a copy in {}", check(st.local_copy), st.dir.display())),
            Row::Unchanged => ListItem::new(format!("{} Skip the upload if nothing changed since the last one", check(st.skip_unchanged))),
            Row::Dest => {
                let style = if st.dest_edited { theme.bold() } else { ratatui::style::Style::new() };
                ListItem::new(Line::from(vec![Span::raw("File name: "), Span::styled(st.dest.value.clone(), style)]))
            }
            Row::Start => {
                let label = if st.auto { "Use this backup" } else { "Start backup" };
                ListItem::new(Line::styled(format!("{} {label}", theme.sym.start), theme.ok()))
            }
        })
        .collect();
    let to = if st.proton { format!("Proton Drive {}", st.remote) } else { st.dir.display().to_string() };
    let title = format!(" Back up to {} ", crate::tui::widgets::ellipsize(&to, list_area.width.saturating_sub(16) as usize, theme.sym.ellipsis));
    let list = List::new(items).block(block(theme, title)).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
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
        if st.proton {
            lines.push(Line::from(vec![Span::styled("Uploads: ", theme.bold()), Span::raw(format!("{}/{}", st.remote, st.default_name()))]));
            lines.push(Line::raw(format!(
                "With a sidecar listing its contents. Keeps the newest {} of this kind; older ones go to Proton Drive's trash.",
                st.keep
            )));
            lines.push(Line::raw(""));
            lines.push(Line::styled("Built in a private folder (0700) and removed after the upload unless you keep a copy.", theme.dim()));
        } else {
            lines.push(Line::styled("The archive holds logins and cookies: it is created readable by you only (0600).", theme.dim()));
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Details ")), p);
    }

    if st.editing {
        let r = popup(f, body, body.width.saturating_sub(8).min(100), 4);
        let blk = block(theme, format!(" File name in {} ", st.dir.display()));
        let inner = blk.inner(r);
        f.render_widget(blk, r);
        let [field, hint] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        st.dest.render(f, field, theme, true);
        f.render_widget(Paragraph::new(Line::styled("Tab completes, Enter or Esc closes", theme.dim())), hint);
    }
}

pub fn hints(st: &ExportState) -> &'static [(&'static str, &'static str)] {
    match st.stage {
        Stage::Source => &[("↑/↓", "move"), ("Enter", "choose"), ("Esc", "back"), ("?", "help")],
        Stage::Folder => &[("Tab", "complete"), ("Enter", "use folder"), ("Esc", "back")],
        Stage::What if st.editing => &[("Tab", "complete"), ("Enter", "done")],
        Stage::What if st.proton && st.auto => &[("↑/↓", "move"), ("Space", "select/toggle"), ("s", "use this"), ("Esc", "back"), ("?", "help")],
        Stage::What if st.proton => &[("↑/↓", "move"), ("Space", "select/toggle"), ("s", "start"), ("Esc", "back"), ("?", "help")],
        Stage::What if st.auto => &[("↑/↓", "move"), ("Space", "select/toggle"), ("s", "use this"), ("Esc", "folder"), ("?", "help")],
        Stage::What => &[("↑/↓", "move"), ("Space", "select/toggle"), ("s", "start"), ("Esc", "folder"), ("?", "help")],
    }
}

pub const HELP: [(&str, &str); 8] = [
    ("↑/↓  j/k", "Move"),
    ("Enter", "Choose the destination, the folder"),
    ("Proton Drive", "Enter installs / signs in when needed; r re-checks, L signs out"),
    ("Tab", "Folder / file name: complete (paste and ~ work)"),
    ("Space  Enter", "Select a target, toggle an option, edit the file name"),
    ("s", "Start the backup, asks first (automatic run: use this)"),
    ("Esc  q", "Back one stage"),
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
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).folder.value, "/media/data/backups");
        let s = state(&mut a).unwrap();
        s.dir = PathBuf::from("/media/data/backups");
        s.stage = Stage::What;
        let mut targets: Vec<TargetInfo> =
            TARGETS.iter().map(|(k, _)| TargetInfo { key: k, detected: false, size: String::new(), location: String::new() }).collect();
        for (i, size) in [(0, ""), (1, "2.1G"), (4, "512M"), (8, "8.0K"), (9, "96M")] {
            targets[i].detected = true;
            targets[i].size = size.into();
            targets[i].location = format!("/home/p3ter/.config/{}", targets[i].key);
        }
        update(&mut a, Event::Loaded(Loaded::ExportTargets { targets, ts: "2026-10-17_101500".into() }));
        a
    }

    /// The "what" stage for Proton Drive, signed in.
    fn proton(w: u16, h: u16) -> App {
        let mut a = loaded(w, h);
        a.drive = Some(Box::new(drive::tests::info(Some(crate::proton::Status::SignedIn))));
        let s = state(&mut a).unwrap();
        s.stage = Stage::Source;
        s.source = 1;
        update(&mut a, key(KeyCode::Enter));
        a
    }

    #[test]
    fn proton_drive_upload_flow() {
        let mut a = proton(80, 24);
        let s = st(&a);
        assert!(s.proton && s.stage == Stage::What);
        assert_eq!(s.remote, "/my-files/kast-backups/archbox");
        assert!(!s.rows().contains(&Row::Dest) && s.rows().contains(&Row::LocalCopy) && !s.rows().contains(&Row::Unchanged));
        insta::assert_snapshot!("export_proton_80x24", render(&a));
        insta::assert_snapshot!("export_proton_140x40", render(&proton(140, 40)));
        // s asks first, then starts the upload job
        update(&mut a, key(KeyCode::Char('s')));
        let Some(Modal { kind: ModalKind::Confirm(k), details, .. }) = &a.modal else { panic!("no confirmation") };
        let JobKind::Upload(o, up) = k.as_ref() else { panic!("not an upload: {k:?}") };
        assert_eq!((o.target.as_str(), o.output.as_deref()), ("all", Some(Path::new("all_backup_2026-10-17_101500.zip"))));
        assert_eq!((up.keep, up.also_local.as_ref(), up.if_unchanged, up.retention_ok), (5, None, crate::drive::IfUnchanged::Ask, false));
        assert!(details.iter().any(|d| d == "Uploads:  /my-files/kast-backups/archbox/all_backup_2026-10-17_101500.zip"));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(JobKind::Upload(..)))));
        // A local copy goes to the chosen folder; Esc goes back to the sources
        let mut a = proton(80, 24);
        let i = st(&a).rows().iter().position(|r| *r == Row::LocalCopy).unwrap();
        state(&mut a).unwrap().focus = i;
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).upload().also_local, Some(PathBuf::from("/media/data/backups")));
        update(&mut a, key(KeyCode::Esc));
        assert!(st(&a).stage == Stage::Source && !st(&a).proton);
    }

    #[test]
    fn unchanged_content_question() {
        for (w, h) in [(80, 24), (140, 40)] {
            let mut a = proton(w, h);
            update(&mut a, key(KeyCode::Char('s')));
            update(&mut a, key(KeyCode::Enter));
            let prompt = crate::report::Prompt::yes_no("Nothing changed since the last upload (2026-10-16 21:04). Upload anyway?", false);
            update(&mut a, Event::Job(crate::tui::event::JobMsg::Ask { id: 1, prompt }));
            let m = a.modal.as_ref().unwrap();
            assert!(!m.yes_selected, "No is the default");
            insta::assert_snapshot!(format!("export_proton_unchanged_{w}x{h}"), render(&a));
            assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Answer { id: 1, yes: false })));
        }
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
        assert_eq!(st(&a).dest.value, "all_backup_2026-10-17_101500.zip");
        // howdy: .dat by default, no media option
        for _ in 0..8 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).dest.value, "p3ter_face_model_2026-10-17_101500.dat");
        assert!(!st(&a).rows().contains(&Row::Media));
        // Format row comes right after the targets
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).dest.value, "howdy_backup_2026-10-17_101500.zip");
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
        assert_eq!(st(&a).dest.value, "howdy_backup_2026-10-17_101500.zip");
        update(&mut a, key(KeyCode::Char('s')));
        let m = a.modal.as_ref().unwrap();
        assert!(m.yes_selected);
        let ModalKind::Confirm(k) = &m.kind else { panic!() };
        let JobKind::Export(o) = k.as_ref() else { panic!() };
        assert_eq!((o.target.as_str(), o.kill, o.force, o.include_media), ("all", true, false, true));
        assert_eq!(o.output.as_deref(), Some(Path::new("/media/data/backups/howdy_backup_2026-10-17_101500.zip")));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::Start(JobKind::Export(_)))));
    }

    #[test]
    fn folder_is_created_on_request() {
        let t = tempfile::tempdir().unwrap();
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('7')));
        // Proton Drive without proton-drive: Enter explains how to install it
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).stage, Stage::Source);
        assert!(matches!(a.modal.as_ref().unwrap().kind, ModalKind::DriveRecheck));
        assert!(matches!(update(&mut a, key(KeyCode::Char('r'))), Some(Command::DriveCheck { refresh: true })));
        assert!(render(&a).contains("Proton Drive · checking…"));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Enter));
        assert_eq!(st(&a).stage, Stage::Folder);
        // A folder that exists: on to "what"
        state(&mut a).unwrap().folder.set(t.path().display().to_string());
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!((st(&a).stage, st(&a).dir.as_path()), (Stage::What, t.path()));
        // One that doesn't: say so, then create it (as the user) on a second Enter
        update(&mut a, key(KeyCode::Esc));
        let new = t.path().join("new/backups");
        state(&mut a).unwrap().folder.set(new.display().to_string());
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(st(&a).folder_error.as_deref().is_some_and(|e| e.contains("Press Enter again to create it (as p3ter)")));
        let Some(Command::MkdirUser(p)) = update(&mut a, key(KeyCode::Enter)) else { panic!("expected the folder to be created") };
        assert_eq!(p, new);
        update(&mut a, Event::Loaded(Loaded::DirCreated { path: p, result: Err("Could not create it: denied".into()) }));
        assert_eq!((st(&a).stage, st(&a).folder_error.as_deref()), (Stage::Folder, Some("Could not create it: denied")));
        update(&mut a, Event::Loaded(Loaded::DirCreated { path: new.clone(), result: Ok(()) }));
        assert_eq!((st(&a).stage, st(&a).dir.clone()), (Stage::What, new));
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
