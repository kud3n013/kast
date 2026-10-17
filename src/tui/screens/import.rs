//! Restore (and inspect) backups: where from (a local folder; Proton Drive later), which
//! folder, then a table of the archives and face models in it with a manifest panel, then
//! the restore options and a confirmation that names exactly what gets overwritten. In the
//! automatic run the options end with "Use this", which records the restore for the run.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};

use super::drive;
use crate::tui::app::{App, Command, Modal, ModalKind, Screen, ToastKind};
use crate::tui::theme::Theme;
use crate::tui::widgets::{Input, block, ellipsize};
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

/// Where backups are looked for first: the first of these that exists.
pub fn default_dir(home: &str, exists: fn(&Path) -> bool) -> String {
    let dirs = ["/media/data/backups".to_string(), format!("{home}/.local/share/app-vault-sync/backups"), format!("{home}/Downloads")];
    dirs.iter().find(|d| exists(Path::new(d))).unwrap_or(&dirs[2]).clone()
}

/// `~` and `~/…` against `home`.
pub fn expand(value: &str, home: &str) -> PathBuf {
    match value.trim() {
        "~" => PathBuf::from(home),
        v => v.strip_prefix("~/").map_or_else(|| PathBuf::from(v), |rest| Path::new(home).join(rest)),
    }
}

/// A folder the user typed, if it can be listed.
pub fn check_dir(value: &str, home: &str) -> Result<PathBuf, String> {
    if value.trim().is_empty() {
        return Err("Type a folder".into());
    }
    let p = expand(value, home);
    if !p.exists() {
        return Err(format!("{} doesn't exist", p.display()));
    }
    if !p.is_dir() {
        return Err(format!("{} is not a folder", p.display()));
    }
    std::fs::read_dir(&p).map_err(|e| format!("Can't read {}: {e}", p.display()))?;
    Ok(p)
}

/// The backups directly in `dir` (not in subfolders), newest first: every .zip, and the
/// .dat face models (names with "face" or "howdy", or `<user>.dat`).
pub fn find_in(dir: &Path, user: &str, zips_only: bool) -> Vec<PathBuf> {
    let user_dat = format!("{user}.dat");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                    n.ends_with(".zip") || !zips_only && n.ends_with(".dat") && (n.contains("face") || n.contains("howdy") || n == user_dat)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    files.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    files
}

/// The backups in `dir` with their size and date (runs `du`, so off the UI thread).
pub fn load_candidates(dir: &Path, user: &str, zips_only: bool) -> Vec<Candidate> {
    find_in(dir, user, zips_only)
        .into_iter()
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
    /// Size of what the snapshot would copy (None: still measuring)
    pub snap_bytes: Option<u64>,
    /// Turning the snapshot off asked once: the next Space confirms it
    pub confirm_off: bool,
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
        Options { file, inspected, choices, choice: 0, kill: true, safety: true, dry_run: false, focus: 0, snap_bytes: None, confirm_off: false }
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
            // This dialog already confirmed the overwrite. Without kill the engine still
            // asks about running apps itself
            force: false,
            confirmed: true,
            dry_run: self.dry_run,
        }
    }

    pub fn apps(&self) -> Vec<String> {
        match &self.choices[self.choice].1 {
            Some(a) => vec![a.clone()],
            None => self.inspected.info.apps.iter().map(|a| a.key.clone()).collect(),
        }
    }
}

/// Where backups come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Local,
    /// Through proton-drive, once it's installed and signed in
    ProtonDrive,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Local, Source::ProtonDrive];

    pub fn label(self) -> &'static str {
        match self {
            Source::Local => "Local folder",
            Source::ProtonDrive => "Proton Drive",
        }
    }
}

/// Enter on the source list's Proton Drive row: None when it's ready to use (signed in),
/// else whatever the row needs (install, sign in, explain) as the command.
pub fn proton_gate(app: &mut App) -> Result<(), Option<Command>> {
    if drive::row(app).1 { Ok(()) } else { Err(drive::enter(app)) }
}

/// Restoring from Proton Drive: the machines with backups, and the chosen one's backups.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Remote {
    pub hosts: Vec<String>,
    pub host_sel: usize,
    pub host: Option<String>,
    pub loading_hosts: bool,
    pub error: Option<String>,
    /// This machine's folder name, and the base folder on Drive
    pub this_host: String,
    pub folder: String,
    pub backups: Vec<crate::drive::RemoteBackup>,
    /// Being downloaded (name, since when)
    pub downloading: Option<(String, u64)>,
}

impl Remote {
    fn backup(&self, remote: &Path) -> Option<&crate::drive::RemoteBackup> {
        self.backups.iter().find(|b| Path::new(&b.remote) == remote)
    }
}

/// A size the way `du -sh` prints it (as the local list shows sizes): 8.0K, 512M, 2.1G.
pub fn short_size(bytes: u64) -> String {
    let mut v = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if v < 1024.0 || unit == "T" {
            return if unit == "B" {
                format!("{bytes}B")
            } else if v < 10.0 {
                format!("{v:.1}{unit}")
            } else {
                format!("{}{unit}", v.round())
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// The table rows for backups on Drive.
pub fn remote_candidates(list: &[crate::drive::RemoteBackup]) -> Vec<Candidate> {
    list.iter()
        .map(|b| {
            let path = PathBuf::from(&b.remote);
            let modified = b
                .sidecar
                .as_ref()
                .and_then(|s| s["created_at"].as_str().map(String::from))
                .or_else(|| b.modified.clone())
                .map(|t| t.chars().take(16).collect::<String>().replace('T', " "))
                .unwrap_or_default();
            let size = b.sidecar.as_ref().and_then(|s| s["size"].as_u64()).or(b.size).map(short_size).unwrap_or_default();
            Candidate { name: b.name.clone(), tag: crate::interactive::candidate_tag(&path), size, modified, path }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum Stage {
    /// Local folder or Proton Drive
    Source,
    /// Proton Drive: which machine's backups
    Hosts,
    /// The folder to look in
    Folder,
    /// The backups in it
    Pick,
    Options(Box<Options>),
}

#[derive(Debug, Clone)]
pub struct ImportState {
    pub mode: Mode,
    /// A stage of the automatic run: the options end with "Use this" instead of a restore
    pub auto: bool,
    /// Highlighted row of the source list
    pub source: usize,
    pub folder: Input,
    pub folder_error: Option<String>,
    /// The folder being listed
    pub dir: PathBuf,
    pub loading: bool,
    pub all: Vec<Candidate>,
    pub filter: Input,
    pub filtering: bool,
    /// Row in the visible list
    pub selected: usize,
    pub completions: Vec<String>,
    pub preview: Option<(PathBuf, Result<Option<Inspected>, String>)>,
    pub stage: Stage,
    /// Set while the backups come from Proton Drive
    pub remote: Option<Box<Remote>>,
    last_click: Option<(usize, u64)>,
}

impl ImportState {
    pub fn new(mode: Mode, auto: bool, folder: String) -> Self {
        ImportState {
            mode,
            auto,
            source: 0,
            folder: Input::new(folder),
            folder_error: None,
            dir: PathBuf::new(),
            loading: false,
            all: Vec::new(),
            filter: Input::default(),
            filtering: false,
            selected: 0,
            completions: Vec::new(),
            preview: None,
            stage: Stage::Source,
            remote: None,
            last_click: None,
        }
    }

    /// The folder's text field has the keyboard.
    pub fn typing(&self) -> bool {
        matches!(self.stage, Stage::Folder) || self.filtering
    }

    pub fn visible(&self) -> Vec<&Candidate> {
        let f = self.filter.value.to_lowercase();
        self.all.iter().filter(|c| f.is_empty() || c.name.to_lowercase().contains(&f) || c.path.to_string_lossy().to_lowercase().contains(&f)).collect()
    }

    fn row_count(&self) -> usize {
        self.visible().len()
    }

    fn selected_candidate(&self) -> Option<&Candidate> {
        self.visible().get(self.selected).copied()
    }

    /// Ask for the manifest of the highlighted archive if the panel shows another file.
    fn preview_cmd(&self) -> Option<Command> {
        if self.remote.is_some() {
            // The sidecar is the preview: nothing to read
            return None;
        }
        let c = self.selected_candidate()?;
        if c.is_dat() || self.preview.as_ref().is_some_and(|(p, _)| *p == c.path) {
            return None;
        }
        Some(Command::Inspect { path: c.path.clone(), pick: false })
    }

    fn move_to(&mut self, row: usize) -> Option<Command> {
        self.selected = row.min(self.row_count().saturating_sub(1));
        self.preview_cmd()
    }
}

fn state(app: &mut App) -> Option<&mut ImportState> {
    match &mut app.screen {
        Screen::Import(s) => Some(s),
        Screen::Auto(w) => w.import_mut(),
        _ => None,
    }
}

pub fn state_ref(app: &App) -> Option<&ImportState> {
    match &app.screen {
        Screen::Import(s) => Some(s),
        Screen::Auto(w) => w.import_ref(),
        _ => None,
    }
}

/// Leave the import screens (the first stage's Esc).
fn leave(app: &mut App, auto: bool) {
    if auto {
        super::wizard::back(app);
    } else {
        app.screen = Screen::Menu;
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
            let opts = ImportOpts { file: path.clone(), app: None, safety_backup: true, kill: false, force: false, confirmed: true, dry_run: false };
            if st.auto {
                return super::wizard::import_done(app, opts);
            }
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
            if st.mode == Mode::Inspect {
                st.preview = Some((path, Ok(Some(ins))));
            } else {
                let o = Options::new(path.clone(), ins);
                let apps = o.inspected.info.apps.iter().map(|a| a.key.clone()).collect();
                st.stage = Stage::Options(Box::new(o));
                return Some(Command::SnapshotSize { file: path, apps });
            }
        }
    }
    None
}

/// The size of what the snapshot would copy.
pub fn on_snapshot_size(app: &mut App, file: PathBuf, bytes: u64) {
    if let Some(st) = state(app)
        && let Stage::Options(o) = &mut st.stage
        && o.file == file
    {
        o.snap_bytes = Some(bytes);
    }
}

pub fn on_completion(app: &mut App, value: String, matches: Vec<String>) {
    if let Some(st) = state(app)
        && matches!(st.stage, Stage::Folder)
    {
        st.folder.set(value);
        st.completions = matches;
    }
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(st) = state(app) {
        if matches!(st.stage, Stage::Folder) {
            st.folder.insert(text.trim());
            st.folder_error = None;
        } else if st.filtering {
            st.filter.insert(text);
        }
    }
}

// --- Keys ----------------------------------------------------------------------------------

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let now = app.now_ms;
    let home = app.header.home.clone();
    let user = app.header.user.clone();
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
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.source = 1 - st.source.min(1),
                KeyCode::Enter | KeyCode::Char(' ') => return pick_source(app),
                KeyCode::Esc | KeyCode::Char('q') => {
                    leave(app, auto);
                    // Leaving the restore: a backup downloaded from Drive goes (the automatic
                    // run keeps it until it has run)
                    return (!auto).then_some(Command::DriveWipeRestore);
                }
                _ => {}
            }
            return None;
        }
        Stage::Folder => {
            return match k.code {
                KeyCode::Esc => {
                    st.stage = Stage::Source;
                    st.completions.clear();
                    st.folder_error = None;
                    None
                }
                KeyCode::Tab => Some(Command::Complete(st.folder.value.clone())),
                KeyCode::Enter => match check_dir(&st.folder.value, &home) {
                    Ok(dir) => {
                        st.folder_error = None;
                        st.completions.clear();
                        st.dir = dir.clone();
                        st.stage = Stage::Pick;
                        st.loading = true;
                        st.all.clear();
                        st.preview = None;
                        st.selected = 0;
                        Some(Command::LoadCandidates { dir, user, zips_only: st.mode == Mode::Inspect })
                    }
                    Err(e) => {
                        st.folder_error = Some(e);
                        None
                    }
                },
                _ => {
                    if st.folder.key(&k) {
                        st.folder_error = None;
                        st.completions.clear();
                    }
                    None
                }
            };
        }
        Stage::Options(_) => return options_key(app, k),
        Stage::Hosts => return hosts_key(app, k),
        Stage::Pick => {}
    }
    if let Some(r) = st.remote.as_mut()
        && r.downloading.is_some()
    {
        // Only Esc while downloading: it stops the download
        if matches!(k.code, KeyCode::Esc) || crate::tui::app::is_ctrl_c(&k) {
            r.downloading = None;
            return Some(Command::DriveCancelTransfer);
        }
        return None;
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
    let last = st.row_count().saturating_sub(1);
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
        KeyCode::Enter => {
            let cmd = open_row(st);
            if let Some(r) = st.remote.as_mut()
                && let Some(d) = r.downloading.as_mut()
            {
                d.1 = now;
            }
            cmd
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            st.stage = if st.remote.is_some() { Stage::Hosts } else { Stage::Folder };
            None
        }
        _ => None,
    }
}

/// Enter on the source list: Local goes on to the folder, Proton Drive to its hosts (once
/// it's installed and signed in).
fn pick_source(app: &mut App) -> Option<Command> {
    let st = state(app)?;
    if Source::ALL[st.source.min(1)] == Source::Local {
        st.stage = Stage::Folder;
        return None;
    }
    if let Err(cmd) = proton_gate(app) {
        return cmd;
    }
    let d = app.drive.clone()?;
    let st = state(app)?;
    st.remote = Some(Box::new(Remote { this_host: d.host.clone(), folder: d.folder.clone(), loading_hosts: true, ..Default::default() }));
    st.stage = Stage::Hosts;
    Some(Command::DriveHosts)
}

/// The machines with backups on Drive arrived.
pub fn on_hosts(app: &mut App, result: Result<Vec<String>, String>) {
    if let Some(st) = state(app)
        && let Some(r) = st.remote.as_mut()
    {
        r.loading_hosts = false;
        r.host_sel = 0;
        match result {
            Ok(h) => {
                r.hosts = crate::drive::order_hosts(h, &r.this_host);
                r.error = None;
            }
            Err(e) => r.error = Some(e),
        }
    }
}

/// A machine's backups (with sidecars) arrived.
pub fn on_backups(app: &mut App, host: String, result: Result<Vec<crate::drive::RemoteBackup>, String>) -> Option<Command> {
    let st = state(app)?;
    let r = st.remote.as_mut()?;
    if r.host.as_deref() != Some(host.as_str()) {
        return None;
    }
    st.loading = false;
    st.selected = 0;
    match result {
        Ok(list) => {
            st.all = remote_candidates(&list);
            r.backups = list;
            r.error = None;
        }
        Err(e) => {
            st.all.clear();
            r.backups.clear();
            r.error = Some(e);
        }
    }
    None
}

/// The download finished: on to the usual restore steps with the local copy.
pub fn on_downloaded(app: &mut App, remote: String, result: Result<PathBuf, String>) -> Option<Command> {
    let st = state(app)?;
    let r = st.remote.as_mut()?;
    r.downloading = None;
    match result {
        Ok(path) => Some(Command::Inspect { path, pick: true }),
        Err(e) => {
            let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
            app.toast(ToastKind::Error, format!("{name}: {e}"));
            None
        }
    }
}

fn hosts_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let r = st.remote.as_mut()?;
    let n = r.hosts.len();
    match k.code {
        KeyCode::Up | KeyCode::Char('k') if n > 0 => r.host_sel = r.host_sel.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') if n > 0 => r.host_sel = (r.host_sel + 1) % n,
        KeyCode::Char('r') => {
            r.loading_hosts = true;
            return Some(Command::DriveHosts);
        }
        KeyCode::Enter | KeyCode::Char(' ') if n > 0 && !r.loading_hosts => {
            let host = r.hosts[r.host_sel].clone();
            r.host = Some(host.clone());
            r.backups.clear();
            r.error = None;
            st.dir = PathBuf::from(crate::proton::join(&r.folder, &[&host]));
            st.stage = Stage::Pick;
            st.loading = true;
            st.all.clear();
            st.preview = None;
            st.selected = 0;
            st.filter = Input::default();
            return Some(Command::DriveBackups(host));
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            st.remote = None;
            st.stage = Stage::Source;
        }
        _ => {}
    }
    None
}

fn open_row(st: &mut ImportState) -> Option<Command> {
    let c = st.selected_candidate()?.clone();
    if let Some(r) = st.remote.as_mut() {
        if r.downloading.is_some() {
            return None;
        }
        let b = r.backup(&c.path)?.clone();
        if st.mode == Mode::Inspect && b.sidecar.is_some() {
            // Its sidecar already says what's inside: no download
            return None;
        }
        r.downloading = Some((b.name.clone(), 0));
        return Some(Command::DriveDownload(Box::new(b)));
    }
    Some(Command::Inspect { path: c.path.clone(), pick: true })
}

fn options_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    let Stage::Options(o) = &mut st.stage else { return None };
    let n = o.rows();
    let n_choices = o.choices.len();
    match k.code {
        // A pending "go without a snapshot?" is dropped by anything but its Space
        _ if o.confirm_off && k.code != KeyCode::Char(' ') => {
            o.confirm_off = false;
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                return None;
            }
            return options_key(app, k);
        }
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
                // Off only after the warning was shown once: this restore couldn't be undone
                if o.safety && !o.confirm_off {
                    o.confirm_off = true;
                } else {
                    o.safety = !o.safety;
                    o.confirm_off = false;
                }
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
    if st.auto {
        let opts = o.opts();
        return super::wizard::import_done(app, opts);
    }
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
    if let Stage::Source = st.stage {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let (list, _) = source_layout(body);
            if m.row > list.y && m.row < list.bottom().saturating_sub(1) && ((m.row - list.y - 1) as usize) < Source::ALL.len() {
                st.source = (m.row - list.y - 1) as usize;
                return pick_source(app);
            }
        }
        return None;
    }
    if matches!(st.stage, Stage::Folder) {
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
    match &st.stage {
        Stage::Options(o) => return options_view(f, body, theme, o, st.auto),
        Stage::Source => return source_view(f, body, app, st),
        Stage::Folder => return folder_view(f, body, theme, st),
        Stage::Hosts => return hosts_view(f, body, app, st),
        Stage::Pick => {}
    }
    let (table_area, panel) = layout(body);
    let wide = body.width >= 100;
    let visible = st.visible();
    let dir = ellipsize(&st.dir.display().to_string(), table_area.width.saturating_sub(30) as usize, theme.sym.ellipsis);
    let host = st.remote.as_ref().and_then(|r| r.host.clone());
    let title = match (st.mode, st.filtering || !st.filter.value.is_empty()) {
        (_, true) => format!(" Filter: {}{} ", st.filter.value, if st.filtering { theme.sym.caret } else { "" }),
        (_, false) if host.is_some() => format!(" Backups of {} on Proton Drive (newest first) ", host.as_deref().unwrap_or_default()),
        (Mode::Restore, false) => format!(" Backups in {dir} (newest first) "),
        (Mode::Inspect, false) => format!(" Archives in {dir} (newest first) "),
    };
    let header = if wide { vec!["Name", "Type", "Size", "Modified"] } else { vec!["Name", "Type", "Size"] };
    let rows: Vec<Row> = visible
        .iter()
        .map(|c| {
            let mut cells = vec![Cell::from(c.name.clone()), Cell::from(c.tag.trim_matches(['[', ']']).replace(" Backup", "")), Cell::from(c.size.clone())];
            if wide {
                cells.push(Cell::from(c.modified.clone()));
            }
            Row::new(cells)
        })
        .collect();
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
    let mut ts = TableState::default().with_selected((!visible.is_empty()).then_some(st.selected)).with_offset(table_offset(st, table_area));
    f.render_stateful_widget(table, table_area, &mut ts);

    // Side panel: what the highlighted entry is
    let mut lines: Vec<Line> = Vec::new();
    if st.loading {
        lines.push(Line::styled(format!("{} Looking for backups{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis), theme.dim()));
    } else if let Some(e) = st.remote.as_ref().and_then(|r| r.error.as_ref()) {
        lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error()));
    } else if st.all.is_empty() && st.remote.is_some() {
        lines.push(Line::raw(format!("No kast backups in {} yet.", st.dir.display())));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Press Esc to pick another machine.", theme.dim()));
    } else if st.all.is_empty() {
        lines.push(Line::raw(format!("No backups in {}", st.dir.display())));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Press Esc to change the folder.", theme.dim()));
    } else if let (Some(c), Some(r)) = (st.selected_candidate(), st.remote.as_ref()) {
        lines.extend(remote_lines(app, st, r, c));
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
        lines.push(Line::raw("Nothing matches the filter."));
    }
    let panel_title = if st.mode == Mode::Inspect { " Manifest " } else { " Details " };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, panel_title)), panel);
}

/// The details of a backup on Drive: from its sidecar, or "unknown until downloaded".
fn remote_lines<'a>(app: &App, st: &ImportState, r: &'a Remote, c: &Candidate) -> Vec<Line<'a>> {
    let theme = &app.theme;
    let mut lines = vec![
        Line::from(vec![Span::styled("On Drive: ", theme.bold()), Span::raw(c.path.display().to_string())]),
        Line::from(vec![Span::styled("Size: ", theme.bold()), Span::raw(format!("{}   made {}", c.size, c.modified))]),
    ];
    if let Some((name, since)) = &r.downloading {
        let e = app.now_ms.saturating_sub(*since) / 1000;
        let spin = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("{spin} Downloading {name} · {}:{:02} · Esc stops", e / 60, e % 60), theme.info()));
    }
    lines.push(Line::raw(""));
    let b = r.backup(&c.path);
    if c.is_dat() {
        lines.push(Line::raw(format!("Howdy face model. Restoring installs it as /etc/howdy/models/{}.dat.", app.header.user)));
    } else if let Some(info) = b.and_then(crate::drive::sidecar_info) {
        lines.extend(
            manifest_lines(theme, &info)
                .into_iter()
                .map(|l| Line::from(l.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style)).collect::<Vec<_>>())),
        );
    } else {
        lines.push(Line::styled("Contents unknown until downloaded (no sidecar).", theme.dim()));
    }
    lines.push(Line::raw(""));
    let next = if st.mode == Mode::Inspect && b.is_some_and(|b| b.sidecar.is_some()) {
        "Shown from its sidecar: nothing is downloaded."
    } else {
        "Enter downloads it to a private folder, checks it against its sidecar, then goes on as usual. The copy is wiped afterwards."
    };
    lines.push(Line::styled(next, theme.dim()));
    lines
}

fn hosts_view(f: &mut Frame, body: Rect, app: &App, st: &ImportState) {
    let theme = &app.theme;
    let Some(r) = st.remote.as_ref() else { return };
    let rows = (r.hosts.len().max(1) as u16 + 2).min(body.height.saturating_sub(5));
    let [list_area, details] = Layout::vertical([Constraint::Length(rows), Constraint::Min(3)]).areas(body);
    let items: Vec<ListItem> = r
        .hosts
        .iter()
        .map(|h| {
            if *h == r.this_host {
                ListItem::new(Line::from(vec![Span::raw(h.clone()), Span::styled("  this computer", theme.dim())]))
            } else {
                ListItem::new(h.clone())
            }
        })
        .collect();
    let list = List::new(items).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected((!r.hosts.is_empty()).then_some(r.host_sel));
    f.render_stateful_widget(list.block(block(theme, format!(" Machines with backups in {} ", r.folder))), list_area, &mut ls);
    let mut lines: Vec<Line> = Vec::new();
    if r.loading_hosts {
        lines.push(Line::styled(format!("{} Asking Proton Drive{}", theme.sym.spinner[app.frame % theme.sym.spinner.len()], theme.sym.ellipsis), theme.dim()));
    } else if let Some(e) = &r.error {
        lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error()));
        lines.push(Line::styled("r tries again", theme.dim()));
    } else if r.hosts.is_empty() {
        lines.push(Line::raw("No kast backups on Proton Drive yet."));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Back up to Proton Drive from Export (7) first.", theme.dim()));
    } else {
        let h = &r.hosts[r.host_sel.min(r.hosts.len() - 1)];
        lines.push(Line::raw(if *h == r.this_host {
            "This computer's backups.".to_string()
        } else {
            format!("Backups made on {h}: restoring another machine's backup works too (the apps' data is the same everywhere).")
        }));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter lists them (one listing; each backup's sidecar tells what's inside).", theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " Details ")), details);
}

/// (source list, details) areas.
fn source_layout(body: Rect) -> (Rect, Rect) {
    let [l, r] = Layout::vertical([Constraint::Length(Source::ALL.len() as u16 + 2), Constraint::Min(3)]).areas(body);
    (l, r)
}

/// The source list: Local folder, and Proton Drive with its state (dim until it's usable).
pub fn source_rows<'a>(app: &App, selected: usize) -> (List<'a>, ListState) {
    let theme = &app.theme;
    let (proton, ready) = drive::row(app);
    let items: Vec<ListItem> =
        vec![ListItem::new(Source::Local.label()), if ready { ListItem::new(proton) } else { ListItem::new(Line::styled(proton, theme.dim())) }];
    let list = List::new(items).highlight_style(theme.selected()).highlight_symbol(format!("{} ", theme.sym.pointer));
    (list, ListState::default().with_selected(Some(selected.min(1))))
}

fn source_view(f: &mut Frame, body: Rect, app: &App, st: &ImportState) {
    let theme = &app.theme;
    let (list_area, details) = source_layout(body);
    let title = if st.mode == Mode::Inspect { " Inspect backups from " } else { " Restore from " };
    let (list, mut ls) = source_rows(app, st.source);
    f.render_stateful_widget(list.block(block(theme, title)), list_area, &mut ls);
    let text = if Source::ALL[st.source.min(1)] == Source::Local {
        Text::raw("Backups in a folder on this computer: the data partition, your Downloads, a USB drive. Next: choose the folder.")
    } else {
        Text::from(drive::details(app, false))
    };
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block(theme, " Details ")), details);
}

/// The folder field, with the matches of a Tab completion and the error of a bad folder.
/// What `folder_box` shows.
pub struct Folder<'a> {
    pub title: &'a str,
    pub input: &'a Input,
    pub error: Option<&'a str>,
    pub completions: &'a [String],
    pub help: &'a str,
}

pub fn folder_box(f: &mut Frame, area: Rect, theme: &Theme, d: &Folder) {
    let Folder { title, input, error, completions, help } = *d;
    let blk = block(theme, format!(" {title} "));
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let [field, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    input.render(f, field, theme, true);
    let mut lines = Vec::new();
    if let Some(e) = error {
        lines.push(Line::styled(format!("{} {e}", theme.sym.fail), theme.error()));
    }
    lines.push(Line::styled(help.to_string(), theme.dim()));
    if !completions.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(completions.iter().take(8).map(|c| Line::raw(format!("  {c}"))));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rest);
}

fn folder_view(f: &mut Frame, body: Rect, theme: &Theme, st: &ImportState) {
    let h = 5 + st.completions.len().min(8) as u16 + u16::from(!st.completions.is_empty()) + u16::from(st.folder_error.is_some());
    let [area, _] = Layout::vertical([Constraint::Length(h.min(body.height)), Constraint::Min(0)]).areas(body);
    let help = "Tab completes, Enter lists the backups in this folder, Esc goes back";
    let d = Folder { title: "Folder with your backups", input: &st.folder, error: st.folder_error.as_deref(), completions: &st.completions, help };
    folder_box(f, area, theme, &d);
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

/// The folders a restore of the chosen apps overwrites.
fn overwritten(o: &Options) -> Vec<String> {
    let apps = o.apps();
    o.inspected.destinations.iter().filter(|(a, _)| apps.contains(a)).flat_map(|(_, d)| d.clone()).collect()
}

fn options_view(f: &mut Frame, body: Rect, theme: &Theme, o: &Options, auto: bool) {
    let wide = body.width >= 100;
    let (list_area, panel) = if wide {
        let [l, p] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        (l, Some(p))
    } else {
        (body, None)
    };
    // Under the options: what declining the snapshot means
    let warning = if o.confirm_off {
        Some(format!(
            "{} Without a snapshot this import cannot be undone: your current data in {} will be overwritten permanently. Press Space again to go without it, Esc to keep it.",
            theme.sym.warn,
            overwritten(o).join(", ")
        ))
    } else if !o.safety {
        Some(format!("{} No snapshot: this import cannot be undone.", theme.sym.warn))
    } else {
        None
    };
    let list_area = match &warning {
        Some(w) => {
            let rows = crate::tui::widgets::wrapped_rows(w, list_area.width.saturating_sub(2) as usize) as u16 + 2;
            let [l, wa] = Layout::vertical([Constraint::Min(4), Constraint::Length(rows.min(list_area.height / 2))]).areas(list_area);
            f.render_widget(Paragraph::new(w.clone()).style(theme.error()).wrap(Wrap { trim: true }).block(block(theme, " Snapshot ")), wa);
            l
        }
        None => list_area,
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
    let size = match o.snap_bytes {
        None => format!("measuring{}", theme.sym.ellipsis),
        Some(b) => format!("≈ {}, ~{} s", crate::undo::human(b), (b / 150_000_000).max(1)),
    };
    items.push(toggle(o.safety, &format!("Snapshot your current app data before importing? ({size})")));
    items.push(toggle(o.dry_run, "Dry run (change nothing)"));
    let start = match (auto, o.dry_run) {
        (true, false) => "Use this restore",
        (true, true) => "Use this restore (dry run)",
        (false, true) => "Start dry run",
        (false, false) => "Start restore",
    };
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
    match st.stage {
        Stage::Source => &[("↑/↓", "move"), ("Enter", "choose"), ("Esc", "back"), ("?", "help")],
        Stage::Folder => &[("Tab", "complete"), ("Enter", "list backups"), ("Esc", "back")],
        Stage::Hosts => &[("↑/↓", "move"), ("Enter", "list backups"), ("r", "refresh"), ("Esc", "back")],
        Stage::Pick if st.remote.as_ref().is_some_and(|r| r.downloading.is_some()) => &[("Esc", "stop the download")],
        Stage::Pick if st.remote.is_some() && !st.filtering => {
            &[("↑/↓", "move"), ("Enter", "download & choose"), ("/", "filter"), ("Esc", "machines"), ("?", "help")]
        }
        Stage::Options(_) if st.auto => &[("↑/↓", "move"), ("Space", "toggle"), ("Enter", "choose"), ("s", "use this"), ("Esc", "back"), ("?", "help")],
        Stage::Options(_) => &[("↑/↓", "move"), ("Space", "toggle"), ("Enter", "choose"), ("s", "start"), ("Esc", "back"), ("?", "help")],
        Stage::Pick if st.filtering => &[("type", "filter"), ("Enter", "done"), ("Esc", "clear")],
        Stage::Pick if st.mode == Mode::Inspect => &[("↑/↓", "move"), ("Enter", "manifest"), ("/", "filter"), ("Esc", "folder"), ("?", "help")],
        Stage::Pick => &[("↑/↓", "move"), ("Enter", "choose"), ("/", "filter"), ("Esc", "folder"), ("?", "help")],
    }
}

pub const HELP: [(&str, &str); 11] = [
    ("↑/↓  j/k", "Move"),
    ("Enter", "Choose the source, folder, backup or option"),
    ("Proton Drive", "Not available yet"),
    ("Tab", "Folder: complete the path (paste and ~ work)"),
    ("/", "Filter the backups by name or path"),
    ("Space", "Options: select / toggle"),
    ("s", "Options: start (automatic run: use this restore)"),
    ("Mouse", "Click selects, double-click chooses, wheel moves"),
    ("Esc  q", "Back one stage"),
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

    /// The file list of /media/data/backups, as if the folder had been accepted.
    fn at_files(a: &mut App) {
        let st = state(a).unwrap();
        st.dir = PathBuf::from("/media/data/backups");
        st.stage = Stage::Pick;
        st.loading = true;
    }

    /// The import screen with three candidates loaded and the first one's manifest shown.
    fn loaded(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        assert!(update(&mut a, key(KeyCode::Char('6'))).is_none());
        assert!(matches!(st(&a).stage, Stage::Source));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(st(&a).stage, Stage::Folder));
        assert_eq!(st(&a).folder.value, "/media/data/backups", "pre-filled with the first default folder that exists");
        at_files(&mut a);
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
        // Wraps to the last row
        update(&mut a, key(KeyCode::Up));
        assert_eq!(st(&a).selected, 2);
        update(&mut a, key(KeyCode::Home));
        let Some(Command::Inspect { path, pick: true }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(inspected())) }));
        let Stage::Options(o) = &st(&a).stage else { panic!("no options") };
        let labels: Vec<&str> = o.choices.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["All applications & desktop settings (recommended)", "KDE Plasma settings only", "Only beeper", "Only zen"]);
        // Choose "KDE only", turn off the safety snapshot (asked twice), start
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        for _ in 0..4 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        let Stage::Options(o) = &st(&a).stage else { panic!() };
        assert!(o.safety && o.confirm_off, "the first Space only warns");
        assert!(render(&a).contains("cannot be undone"));
        update(&mut a, key(KeyCode::Char(' ')));
        let Stage::Options(o) = &st(&a).stage else { panic!() };
        assert!(!o.safety && !o.confirm_off);
        update(&mut a, key(KeyCode::Char('s')));
        let m = a.modal.as_ref().unwrap();
        assert!(!m.yes_selected);
        assert!(m.details.iter().any(|d| d.contains("KDE Plasma settings in")));
        assert!(!m.details.iter().any(|d| d.contains("BeeperTexts")));
        assert!(m.details.iter().any(|d| d.contains("No safety snapshot")));
        let ModalKind::Confirm(kind) = &m.kind else { panic!() };
        let JobKind::Import(o) = kind.as_ref() else { panic!() };
        assert_eq!((o.app.as_deref(), o.safety_backup, o.kill, o.force, o.confirmed, o.dry_run), (Some("kde"), false, true, false, true, false));
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
        let Some(Command::Inspect { path, pick: true }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(None) }));
        let m = a.modal.as_ref().unwrap();
        assert!(m.details.iter().any(|d| d.contains("/etc/howdy/models/p3ter.dat")));
    }

    #[test]
    fn filter_and_going_back_through_the_stages() {
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
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(st(&a).stage, Stage::Folder));
        update(&mut a, Event::Paste("/tmp/x\n".into()));
        assert!(matches!(update(&mut a, key(KeyCode::Tab)), Some(Command::Complete(v)) if v == "/media/data/backups/tmp/x"));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(st(&a).stage, Stage::Source));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn short_sizes() {
        assert_eq!([short_size(900), short_size(8192), short_size(536_870_912), short_size(2_254_857_216)], ["900B", "8.0K", "512M", "2.1G"]);
    }

    fn remote_list() -> Vec<crate::drive::RemoteBackup> {
        let side = serde_json::json!({
            "size": 2_254_857_216u64, "sha256": "ab", "created_at": "2026-10-16T21:04:11+02:00",
            "manifest": { "created_at": "2026-10-16T21:04:11+02:00", "hostname": "archbox", "user": "p3ter", "target": "all", "include_media": true,
                          "apps": { "beeper": { "folder": "BeeperTexts" }, "zen": {}, "kde": {} } }
        });
        let b = |name: &str, size: u64, sidecar: Option<serde_json::Value>| crate::drive::RemoteBackup {
            name: name.into(),
            remote: format!("/my-files/kast-backups/archbox/{name}"),
            size: Some(size),
            modified: Some("2026-10-10T08:00:00.000Z".into()),
            sidecar,
        };
        vec![
            b("all_backup_2026-10-16_210411.zip", 2_254_857_216, Some(side)),
            b("zen_backup_2026-10-10_080000.zip", 536_870_912, None),
            b("p3ter_face_model_2026-09-01_100000.dat", 8192, None),
        ]
    }

    /// The restore screen on Proton Drive's machines.
    fn on_drive(w: u16, h: u16) -> App {
        let mut a = app(w, h);
        a.drive = Some(Box::new(drive::tests::info(Some(crate::proton::Status::SignedIn))));
        update(&mut a, key(KeyCode::Char('6')));
        update(&mut a, key(KeyCode::Down));
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DriveHosts)));
        update(&mut a, Event::Loaded(Loaded::DriveHosts(Ok(vec!["laptop".into(), "archbox".into(), "desktop-old".into()]))));
        a
    }

    #[test]
    fn restore_from_proton_drive() {
        let a = on_drive(80, 24);
        let r = st(&a).remote.as_ref().unwrap();
        assert_eq!(r.hosts, ["archbox", "desktop-old", "laptop"], "this machine first, preselected");
        insta::assert_snapshot!("import_drive_hosts_80x24", render(&a));
        for (w, h) in [(80, 24), (140, 40)] {
            let mut a = on_drive(w, h);
            assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DriveBackups(h)) if h == "archbox"));
            assert!(matches!(st(&a).stage, Stage::Pick) && st(&a).loading);
            update(&mut a, Event::Loaded(Loaded::DriveBackups { host: "archbox".into(), result: Ok(remote_list()) }));
            insta::assert_snapshot!(format!("import_drive_backups_{w}x{h}"), render(&a));
            // The second has no sidecar
            update(&mut a, key(KeyCode::Down));
            assert!(render(&a).contains("Contents unknown until downloaded"));
            // Enter downloads; Esc stops it; Enter again, and the copy goes to the usual steps
            let Some(Command::DriveDownload(b)) = update(&mut a, key(KeyCode::Enter)) else { panic!("no download") };
            assert_eq!(b.name, "zen_backup_2026-10-10_080000.zip");
            assert!(render(&a).contains("Downloading zen_backup_2026-10-10_080000.zip"));
            assert!(update(&mut a, key(KeyCode::Down)).is_none(), "keys wait while downloading");
            assert!(matches!(update(&mut a, key(KeyCode::Esc)), Some(Command::DriveCancelTransfer)));
            assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::DriveDownload(_))));
            let local = PathBuf::from("/home/p3ter/.cache/kast/proton-restore/x/zen_backup_2026-10-10_080000.zip");
            let cmd = update(&mut a, Event::Loaded(Loaded::DriveDownloaded { remote: b.remote.clone(), result: Ok(local.clone()) }));
            assert!(matches!(cmd, Some(Command::Inspect { path, pick: true }) if path == local));
        }
        // A failed download is shown, and Esc goes back to the machines, then the sources
        let mut a = on_drive(80, 24);
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, Event::Loaded(Loaded::DriveBackups { host: "archbox".into(), result: Ok(remote_list()) }));
        update(&mut a, key(KeyCode::Enter));
        update(
            &mut a,
            Event::Loaded(Loaded::DriveDownloaded {
                remote: "/x/all.zip".into(),
                result: Err("The download doesn't match its sidecar's checksum (sha256)".into()),
            }),
        );
        assert!(a.toast.as_ref().unwrap().text.contains("checksum"));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(st(&a).stage, Stage::Hosts));
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(st(&a).stage, Stage::Source) && st(&a).remote.is_none());
        assert!(matches!(update(&mut a, key(KeyCode::Esc)), Some(Command::DriveWipeRestore)));
        // Nothing there yet
        let mut a = on_drive(80, 24);
        update(&mut a, Event::Loaded(Loaded::DriveHosts(Ok(vec![]))));
        assert!(render(&a).contains("No kast backups on Proton Drive yet"));
    }

    #[test]
    fn inspect_from_proton_drive_uses_the_sidecar() {
        let mut a = app(140, 40);
        a.drive = Some(Box::new(drive::tests::info(Some(crate::proton::Status::SignedIn))));
        update(&mut a, key(KeyCode::Char('9')));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, Event::Loaded(Loaded::DriveHosts(Ok(vec!["archbox".into()]))));
        update(&mut a, key(KeyCode::Enter));
        update(&mut a, Event::Loaded(Loaded::DriveBackups { host: "archbox".into(), result: Ok(remote_list()) }));
        let out = render(&a);
        assert!(out.contains("Shown from its sidecar") && out.contains("BEEPER"));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none(), "no download for a sidecar");
    }

    #[test]
    fn proton_drive_waits_for_its_check() {
        let mut a = app(80, 24);
        a.drive = None;
        assert!(matches!(update(&mut a, key(KeyCode::Char('6'))), Some(Command::DriveCheck { refresh: false })));
        update(&mut a, key(KeyCode::Down));
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert!(matches!(st(&a).stage, Stage::Source));
        assert_eq!(a.toast.as_ref().unwrap().text, "Still checking Proton Drive…");
        assert!(render(&a).contains("Proton Drive · checking…"));
        update(&mut a, key(KeyCode::Up));
        update(&mut a, key(KeyCode::Enter));
        assert!(matches!(st(&a).stage, Stage::Folder));
    }

    #[test]
    fn folder_is_checked_before_listing() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("file.zip"), "").unwrap();
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('6')));
        update(&mut a, key(KeyCode::Enter));
        let type_folder = |a: &mut App, v: &str| {
            state(a).unwrap().folder.set(v);
            update(a, key(KeyCode::Enter))
        };
        let missing = t.path().join("nope").display().to_string();
        assert!(type_folder(&mut a, &missing).is_none());
        assert_eq!(st(&a).folder_error.as_deref(), Some(format!("{missing} doesn't exist").as_str()));
        assert!(matches!(st(&a).stage, Stage::Folder), "the field stays open");
        let file = t.path().join("file.zip").display().to_string();
        type_folder(&mut a, &file);
        assert!(st(&a).folder_error.as_deref().is_some_and(|e| e.ends_with("is not a folder")));
        // Typing clears the error
        update(&mut a, key(KeyCode::Backspace));
        assert!(st(&a).folder_error.is_none());
        let Some(Command::LoadCandidates { dir, user, zips_only: false }) = type_folder(&mut a, &t.path().display().to_string()) else {
            panic!("expected a load")
        };
        assert_eq!((dir.as_path(), user.as_str()), (t.path(), "p3ter"));
        assert!(matches!(st(&a).stage, Stage::Pick) && st(&a).loading);
        // ~ is the user's home
        assert_eq!(expand("~/backups", "/home/p3ter"), PathBuf::from("/home/p3ter/backups"));
        assert_eq!(expand("~", "/home/p3ter"), PathBuf::from("/home/p3ter"));
        assert_eq!(check_dir("  ", "/home/p3ter"), Err("Type a folder".into()));
    }

    #[test]
    fn backups_in_the_folder_newest_first() {
        use filetime::{FileTime, set_file_mtime};
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir(d.join("sub")).unwrap();
        for (name, age) in [
            ("old_backup.zip", 300),
            ("new.zip", 10),
            ("p3ter_face_model.dat", 100),
            ("p3ter.dat", 200),
            ("other.dat", 5),
            ("notes.txt", 1),
            ("sub/inner.zip", 1),
        ] {
            std::fs::write(d.join(name), "x").unwrap();
            set_file_mtime(d.join(name), FileTime::from_unix_time(1_800_000_000 - age, 0)).unwrap();
        }
        let names = |v: Vec<PathBuf>| v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(find_in(d, "p3ter", false)), ["new.zip", "p3ter_face_model.dat", "p3ter.dat", "old_backup.zip"]);
        assert_eq!(names(find_in(d, "p3ter", true)), ["new.zip", "old_backup.zip"]);
        assert!(find_in(&d.join("sub/inner.zip"), "p3ter", false).is_empty());
    }

    #[test]
    fn empty_folder_says_so() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('6')));
        update(&mut a, key(KeyCode::Enter));
        at_files(&mut a);
        assert!(update(&mut a, Event::Loaded(Loaded::Candidates(vec![]))).is_none());
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        let out = render(&a);
        assert!(out.contains("No backups in /media/data/backups") && out.contains("Press Esc to change the folder"), "{out}");
    }

    #[test]
    fn snapshot_import_source_and_folder() {
        for (w, h) in [(80, 24), (140, 40)] {
            let mut a = app(w, h);
            update(&mut a, key(KeyCode::Char('6')));
            insta::assert_snapshot!(format!("import_source_{w}x{h}"), render(&a));
            update(&mut a, key(KeyCode::Enter));
            state(&mut a).unwrap().folder.set("/media/data/nothing-here");
            update(&mut a, key(KeyCode::Enter));
            update(&mut a, Event::Loaded(Loaded::Completion { value: "/media/data/b".into(), matches: vec!["backups/".into(), "books/".into()] }));
            state(&mut a).unwrap().folder_error = Some("/media/data/nothing-here doesn't exist".into());
            insta::assert_snapshot!(format!("import_folder_error_{w}x{h}"), render(&a));
        }
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

    #[test]
    fn snapshot_question_has_a_size_and_esc_keeps_it() {
        let mut a = loaded(80, 24);
        let Some(Command::Inspect { path, .. }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        let Some(Command::SnapshotSize { file, apps }) = update(&mut a, Event::Loaded(Loaded::Inspected { path, pick: true, result: Ok(Some(inspected())) }))
        else {
            panic!("the size is measured off the UI thread")
        };
        assert_eq!(apps, ["beeper", "zen", "kde"]);
        assert!(render(&a).contains("before importing? (measuring…)"));
        update(&mut a, Event::Loaded(Loaded::SnapshotSize { file, bytes: 1_288_490_189 }));
        let out = render(&a);
        assert!(out.contains("Snapshot your current app data before importing? (≈ 1.2 GiB, ~8 s)"), "{out}");
        insta::assert_snapshot!("import_snapshot_question_80x24", out);
        // Space warns, Esc keeps the snapshot (and stays on the options)
        for _ in 0..5 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        insta::assert_snapshot!("import_no_snapshot_warning_80x24", render(&a));
        update(&mut a, key(KeyCode::Esc));
        let Stage::Options(o) = &st(&a).stage else { panic!("still on the options") };
        assert!(o.safety && !o.confirm_off);
    }

    #[test]
    fn snapshot_warning_140x40() {
        let mut a = loaded(140, 40);
        let Some(Command::Inspect { path, .. }) = update(&mut a, key(KeyCode::Enter)) else { panic!() };
        update(&mut a, Event::Loaded(Loaded::Inspected { path: path.clone(), pick: true, result: Ok(Some(inspected())) }));
        update(&mut a, Event::Loaded(Loaded::SnapshotSize { file: path, bytes: 52_428_800 }));
        insta::assert_snapshot!("import_snapshot_question_140x40", render(&a));
        for _ in 0..5 {
            update(&mut a, key(KeyCode::Down));
        }
        update(&mut a, key(KeyCode::Char(' ')));
        insta::assert_snapshot!("import_no_snapshot_warning_140x40", render(&a));
    }
}
