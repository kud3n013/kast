//! Proton Drive flows on top of `proton`: `kast drive …`, and (later in this file) uploading
//! backups, restoring from Drive and retention.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::ctx::ctx;
use crate::proton::{self, Cli, Install, Problem, Status};
use crate::settings;

/// Exit status of a command that needs Proton Drive when it isn't usable (not installed,
/// signed out, keyring locked, offline). Documented in the README.
pub const EXIT_NOT_READY: u8 = 3;

/// "Proton Drive isn't ready": `main` exits with `EXIT_NOT_READY` on it.
#[derive(Debug)]
pub struct NotReady(pub Problem);

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Problem::NotInstalled => write!(f, "proton-drive is not installed (see `kast drive status`)"),
            Problem::Keyring(m) => {
                write!(f, "The keyring holding the Proton Drive session is locked or unavailable ({m}). Unlock it, or switch to pass: kast drive store pass")
            }
            p => write!(f, "{p}"),
        }
    }
}

impl std::error::Error for NotReady {}

/// The hostname as a Drive folder name: `[A-Za-z0-9._-]`, everything else `_`.
pub fn sanitize_host(h: &str) -> String {
    let s: String = h.trim().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() || s == ".." { "unknown-host".into() } else { s }
}

pub fn host_name() -> String {
    sanitize_host(&nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default())
}

/// Find the CLI and make sure it can be used, without contacting Drive.
pub fn installed_cli() -> Result<Cli, Problem> {
    match proton::detect() {
        (Install::Found { compat: proton::Compat::TooOld, version, .. }, _) => {
            Err(Problem::Other(format!("proton-drive {version} is too old; kast needs {} or newer", proton::PROTON_MIN)))
        }
        (Install::Broken { problem, .. }, _) => Err(problem),
        (i, Some(cli)) if i.usable() => Ok(cli),
        _ => Err(Problem::NotInstalled),
    }
}

/// The CLI, signed in (checked now, not from a cache).
#[allow(dead_code)] // used by the upload in the next commit
pub fn ready_cli() -> Result<Cli, NotReady> {
    let cli = installed_cli().map_err(NotReady)?;
    proton::forget_status();
    match proton::cached_status(&cli) {
        Status::SignedIn => Ok(cli),
        s => Err(NotReady(s.problem().unwrap_or(Problem::SignedOut))),
    }
}

/// Short state for lists: "signed in", "not signed in", …
pub fn status_word(s: &Status) -> &'static str {
    match s {
        Status::SignedIn => "signed in",
        Status::SignedOut => "not signed in",
        Status::KeyringUnavailable(_) => "keyring locked",
        Status::Offline(_) => "offline",
        Status::NotAllowed(_) => "not available for this account",
        Status::Other(_) => "error",
    }
}

/// What a state means and what to do about it (details pane, `kast drive status`).
pub fn explain(s: &Status, pass_ready: bool) -> Vec<String> {
    match s {
        Status::SignedIn => vec!["Signed in. kast can back up to and restore from Proton Drive.".into()],
        Status::SignedOut => {
            vec!["There's no Proton Drive session on this computer (or it expired). Sign in through your browser; kast never sees your password.".into()]
        }
        Status::KeyringUnavailable(m) => {
            let mut v = vec![
                "proton-drive keeps its session in your desktop keyring (KWallet or GNOME Keyring), and it can't be reached: it may be locked, \
                 or this is a TTY or SSH session without one."
                    .into(),
                format!("proton-drive said: {m}"),
                "Log in to your desktop (which unlocks the keyring) and re-check.".into(),
            ];
            if pass_ready {
                v.push("Or keep the session in pass (your password store is set up): switching means signing in again.".into());
            }
            v
        }
        Status::Offline(m) => vec!["Proton Drive can't be reached. Check the network and re-check.".into(), format!("proton-drive said: {m}")],
        Status::NotAllowed(m) => {
            vec!["Proton offers the Drive CLI for some plans only (Proton for Business); this account can't use it.".into(), format!("proton-drive said: {m}")]
        }
        Status::Other(m) => vec![format!("proton-drive said: {m}")],
    }
}

/// `pass` is installed and initialised for the user.
pub fn pass_ready() -> bool {
    crate::ctx::have("pass") && ctx().target_home.join(".password-store/.gpg-id").is_file()
}

// --- kast drive -------------------------------------------------------------------------------

pub fn cmd_status(json_out: bool) -> Result<()> {
    let (install, cli) = proton::detect();
    let status = cli.as_ref().filter(|_| install.usable()).map(|c| c.status());
    let s = settings::load();
    let host = host_name();
    let folder = proton::join(&s.folder(), &[&host]);
    if json_out {
        let (path, version, compat) = match &install {
            Install::Found { path, version, compat } => {
                (Some(path.display().to_string()), Some(version.to_string()), Some(format!("{compat:?}").to_lowercase()))
            }
            Install::Broken { path, .. } => (Some(path.display().to_string()), None, None),
            Install::Missing => (None, None, None),
        };
        let v = json!({
            "installed": !matches!(install, Install::Missing),
            "path": path,
            "version": version,
            "compat": compat,
            "problem": match &install { Install::Broken { problem, .. } => Some(problem.to_string()), _ => None },
            "status": status.as_ref().map(Status::key),
            "detail": status.as_ref().and_then(Status::detail),
            "folder": folder,
            "keep": s.keep(),
            "credentials_store": s.store().unwrap_or_else(|| "keychain".into()),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("proton-drive: {}", install.describe());
        if matches!(install, Install::Missing) {
            print_install_help();
        }
        if let Some(st) = &status {
            println!("Status:       {}", status_word(st));
            if *st != Status::SignedIn {
                for l in explain(st, pass_ready()) {
                    println!("              {l}");
                }
            }
        }
        println!("Backups go to: {folder}");
        println!("Keep:          {} per backup type", s.keep());
        println!("Credentials:   {}", if s.store().as_deref() == Some("pass") { "pass" } else { "keychain (Secret Service / KWallet)" });
    }
    match status {
        Some(Status::SignedIn) => Ok(()),
        Some(st) => Err(NotReady(st.problem().unwrap_or(Problem::SignedOut)).into()),
        None => Err(NotReady(Problem::NotInstalled).into()),
    }
}

pub fn print_install_help() {
    let cpu = proton::cpu_build(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default());
    if proton::package_available() {
        println!("Install it with: sudo pacman -S {}", proton::PROTON_PKG);
        return;
    }
    println!("To install it:");
    println!("  1. Download it from {}", proton::DOWNLOAD_PAGE);
    println!("     Pick {cpu} (this CPU {} AVX2)", if cpu == "linux/x64" { "has" } else { "lacks" });
    println!("  2. chmod +x proton-drive");
    println!("  3. Move it to ~/.local/bin (or anywhere on your PATH)");
}

pub fn cmd_login() -> Result<()> {
    let cli = installed_cli().map_err(NotReady)?;
    let mut login = cli.start_login().map_err(NotReady)?;
    let url = login.url(Duration::from_secs(60)).map_err(|p| anyhow::anyhow!("{p}"))?;
    println!("Sign in to Proton Drive: open this link in a browser (it can be on another device, e.g. your phone):");
    println!();
    println!("{}", url.reveal());
    println!();
    if let Some(qr) = crate::qr::ansi(url.reveal()) {
        let width = qr.lines().next().map_or(0, |l| l.chars().count().saturating_sub(16));
        let cols = terminal_columns();
        if cols == 0 || width <= cols {
            print!("{qr}");
            println!();
        } else {
            println!("(The terminal is too narrow for the QR code; widen it, or open the link.)");
        }
    }
    println!("Waiting for you to sign in (up to 10 minutes; Ctrl+C cancels)…");
    let _ = std::io::stdout().flush();
    match login.finish(proton::LOGIN_TIMEOUT + Duration::from_secs(30)) {
        Ok(()) => {
            proton::forget_status();
            match cli.status() {
                Status::SignedIn => println!("Signed in to Proton Drive."),
                s => println!("Sign-in finished, but the check says: {} ({})", status_word(&s), s.detail().unwrap_or("")),
            }
            Ok(())
        }
        Err(p) => anyhow::bail!("Sign-in failed: {p}"),
    }
}

fn terminal_columns() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ only fills the struct we pass
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 { ws.ws_col as usize } else { 0 }
}

pub fn cmd_logout() -> Result<()> {
    let cli = installed_cli().map_err(NotReady)?;
    cli.logout().map_err(|p| anyhow::anyhow!("Sign-out failed: {p}"))?;
    proton::forget_status();
    println!("Signed out of Proton Drive; its session and local caches on this computer are gone.");
    Ok(())
}

pub fn cmd_store(store: &str) -> Result<()> {
    if store == "pass" && !pass_ready() {
        anyhow::bail!("pass isn't set up for {} (no ~/.password-store/.gpg-id); run `pass init <gpg-id>` first", ctx().target_user);
    }
    settings::update(&["proton", "credentials_store"], Some(store.into()))?;
    println!("Proton Drive sessions are now kept in {}.", if store == "pass" { "pass" } else { "the desktop keyring" });
    println!("Sign in again for it to take effect: kast drive login");
    Ok(())
}

// --- Names on Drive ------------------------------------------------------------------------------

pub const SIDECAR_EXT: &str = ".manifest.json";

/// `<target>_backup_<YYYY-MM-DD_HHMMSS>.zip`
pub fn archive_name(target: &str, ts: &str) -> String {
    format!("{target}_backup_{ts}.zip")
}

/// `<user>_face_model_<ts>.dat`
pub fn face_model_name(user: &str, ts: &str) -> String {
    format!("{user}_face_model_{ts}.dat")
}

pub fn sidecar_name(archive: &str) -> String {
    format!("{archive}{SIDECAR_EXT}")
}

/// What retention groups archives by: the target's archives, or a user's face models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Series {
    Target(String),
    FaceModel(String),
}

impl Series {
    fn regex(&self) -> regex::Regex {
        let (prefix, ext) = match self {
            Series::Target(t) => (format!("{}_backup_", regex::escape(t)), "zip"),
            Series::FaceModel(u) => (format!("{}_face_model_", regex::escape(u)), "dat"),
        };
        // `(1)` is what the CLI's rename strategy adds on a name clash
        regex::Regex::new(&format!(r"^{prefix}(\d{{4}}-\d{{2}}-\d{{2}}_\d{{6}})(?: \(\d+\))?\.{ext}$")).unwrap()
    }

    /// The timestamp of a name of this series.
    pub fn timestamp(&self, name: &str) -> Option<String> {
        self.regex().captures(name).map(|c| c[1].to_string())
    }
}

/// The names to move to the trash so `keep` archives of `series` stay (newest by the time in
/// their name), with their sidecars. Other targets' and hosts' files are never touched.
pub fn retention_victims(names: &[String], series: &Series, keep: usize) -> Vec<String> {
    let mut ours: Vec<(String, &String)> = names.iter().filter_map(|n| series.timestamp(n).map(|ts| (ts, n))).collect();
    ours.sort_by(|a, b| b.cmp(a));
    let mut out = Vec::new();
    for (_, n) in ours.into_iter().skip(keep.max(1)) {
        out.push(n.clone());
        let side = sidecar_name(n);
        if names.contains(&side) {
            out.push(side);
        }
    }
    out
}

// --- Fingerprints -------------------------------------------------------------------------------

/// What a backup holds, independent of when it was made: sha256 over the sorted lines
/// `path \0 size \0 sha256(content) \n` of the archive's files, the manifest hashed without
/// `created_at`. Two backups of unchanged data have the same fingerprint.
pub fn fingerprint_zip(zip: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut ar = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(zip)?))?;
    let mut lines: Vec<String> = Vec::new();
    for i in 0..ar.len() {
        let mut f = ar.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let name = f.name()?.trim_start_matches("./").to_string();
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        if name == crate::vault::archive::MANIFEST || name.ends_with(&format!("/{}", crate::vault::archive::MANIFEST)) {
            data = manifest_without_time(&data);
        }
        lines.push(format!("{name}\0{}\0{}\n", data.len(), hex(&Sha256::digest(&data))));
    }
    lines.sort();
    Ok(hex(&Sha256::digest(lines.concat().as_bytes())))
}

fn manifest_without_time(data: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(data) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("created_at");
            }
            serde_json::to_vec(&v).unwrap_or_default()
        }
        Err(_) => data.to_vec(),
    }
}

/// A face model's fingerprint: its content.
pub fn fingerprint_file(p: &std::path::Path) -> Result<String> {
    crate::journal::sha256_file(p).ok_or_else(|| anyhow::anyhow!("could not read {}", p.display()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The sidecar next to an archive on Drive: the manifest's fields, the archive's size and
/// sha256, and the content fingerprint, so a restore list can show contents without a download.
pub fn sidecar(archive: &std::path::Path, manifest: Option<&serde_json::Value>, fingerprint: &str) -> Result<serde_json::Value> {
    let size = std::fs::metadata(archive)?.len();
    let sha = crate::journal::sha256_file(archive).ok_or_else(|| anyhow::anyhow!("could not read {}", archive.display()))?;
    let m = manifest.cloned().unwrap_or(serde_json::Value::Null);
    let apps: Vec<String> = m.get("apps").and_then(|a| a.as_object()).map(|a| a.keys().cloned().collect()).unwrap_or_default();
    Ok(json!({
        "kast_sidecar": 1,
        "archive": archive.file_name().map(|n| n.to_string_lossy().into_owned()),
        "size": size,
        "sha256": sha,
        "content_fingerprint": fingerprint,
        "kast_version": env!("CARGO_PKG_VERSION"),
        "created_at": m.get("created_at").cloned().unwrap_or_else(|| json!(chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false))),
        "hostname": m.get("hostname").cloned().unwrap_or_else(|| json!(host_name())),
        "user": m.get("user").cloned().unwrap_or_else(|| json!(ctx().target_user)),
        "target": m.get("target").cloned(),
        "apps": apps,
        "manifest": m,
    }))
}

// --- State: what was uploaded last ---------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LastUpload {
    pub fingerprint: String,
    pub at: String,
    pub file: String,
}

/// `~/.local/state/kast/proton.json`
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DriveState {
    /// By `<host>/<series>`
    pub last: std::collections::BTreeMap<String, LastUpload>,
    /// The user agreed once to retention moving old backups to the trash
    pub retention_confirmed: bool,
}

impl DriveState {
    pub fn path() -> std::path::PathBuf {
        settings::state_dir().join("proton.json")
    }

    pub fn load() -> DriveState {
        settings::user_read(&Self::path()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        settings::user_write(&Self::path(), (serde_json::to_string_pretty(self)? + "\n").as_bytes(), 0o600)
    }
}

pub fn series_key(host: &str, series: &Series) -> String {
    match series {
        Series::Target(t) => format!("{host}/{t}"),
        Series::FaceModel(u) => format!("{host}/face_model/{u}"),
    }
}

// --- Staging --------------------------------------------------------------------------------------

/// A private folder of the user's under `~/.cache/kast/<kind>/` (0700). It holds backup data
/// (decrypted app data on a restore), so it goes away when dropped, on errors and cancels
/// too, and on signals through `ctx::cleanup_on_exit`; `keep` hands it over instead.
pub struct Staging {
    pub dir: std::path::PathBuf,
    kept: bool,
}

impl Staging {
    pub fn new(kind: &str) -> Result<Staging> {
        Self::under(&ctx().cache_home.join("kast").join(kind))
    }

    pub fn under(parent: &std::path::Path) -> Result<Staging> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = parent.join(format!("{}-{:08x}{:x}", crate::ctx::now_str("%Y%m%d-%H%M%S"), nanos, std::process::id()));
        settings::user_mkdir_private(&dir).map_err(|e| anyhow::anyhow!("Could not create a private staging folder: {e}"))?;
        crate::ctx::track_private_dir(&dir);
        Ok(Staging { dir, kept: false })
    }

    /// Leave the folder in place (an upload to retry later).
    pub fn keep(mut self) -> std::path::PathBuf {
        self.kept = true;
        crate::ctx::untrack_private_dir(&self.dir);
        self.dir.clone()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.kept {
            let _ = settings::user_remove_dir(&self.dir);
            crate::ctx::untrack_private_dir(&self.dir);
        }
    }
}

// --- Uploading ----------------------------------------------------------------------------------

/// When the content is the same as the last upload's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IfUnchanged {
    /// Ask (interactive)
    #[default]
    Ask,
    /// Don't upload (scheduled; the automatic run's default)
    Skip,
    /// Upload anyway (--force-upload)
    Upload,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UploadOpts {
    /// Archives kept per series on Drive
    pub keep: u32,
    /// Also keep the archive in this folder (else it's built in a staging folder)
    pub also_local: Option<std::path::PathBuf>,
    pub if_unchanged: IfUnchanged,
    /// Retention may trash old backups without asking (agreed to up front)
    pub retention_ok: bool,
    /// The name's target, when not the backup's own ("apps" for a scheduled "all")
    pub name_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Uploaded {
    Done {
        remote: String,
        size: u64,
        /// The CLI found the same file there already
        identical: bool,
        trashed: Vec<String>,
        local: Option<std::path::PathBuf>,
    },
    /// Same content as the last upload: nothing sent
    Unchanged {
        since: String,
    },
    /// Nothing was backed up (every app running, a dry run, nothing found)
    Nothing,
    Canceled,
}

fn human(bytes: u64) -> String {
    crate::undo::human(bytes)
}

/// Back up as `o` says and upload the result to Proton Drive.
pub fn backup_upload(o: crate::vault::backup::BackupOpts, up: &UploadOpts) -> Result<Uploaded> {
    use crate::vault::{is_dat, log_step, normalize_app};
    let cli = ready_cli()?;
    let c = ctx();
    let target = normalize_app(&o.target);
    let dat = target == "howdy" && o.output.as_deref().is_some_and(is_dat);
    let ts = crate::ctx::now_str("%Y-%m-%d_%H%M%S");
    let (name, series) = if dat {
        (face_model_name(&c.target_user, &ts), Series::FaceModel(c.target_user.clone()))
    } else {
        let t = up.name_target.clone().unwrap_or(target.clone());
        (archive_name(&t, &ts), Series::Target(t))
    };
    let (dir, staging) = match &up.also_local {
        Some(d) => {
            crate::ctx::mkdir_user(d)?;
            (d.clone(), None)
        }
        None => {
            let st = Staging::new("proton-upload")?;
            (st.dir.clone(), Some(st))
        }
    };
    let file = dir.join(&name);
    log_step(&format!("Building the backup for Proton Drive: {name}"));
    let mut bo = o.clone();
    bo.output = Some(file.clone());
    bo.staged = staging.is_some();
    crate::vault::backup::cmd_backup(bo)?;
    if crate::ctx::canceled() {
        return Ok(Uploaded::Canceled);
    }
    if !file.is_file() {
        return Ok(Uploaded::Nothing);
    }
    let fp = if dat { fingerprint_file(&file)? } else { fingerprint_zip(&file)? };
    let host = host_name();
    if let Some(last) = DriveState::load().last.get(&series_key(&host, &series)).filter(|l| l.fingerprint == fp) {
        let go = match up.if_unchanged {
            IfUnchanged::Upload => true,
            IfUnchanged::Skip => false,
            IfUnchanged::Ask => {
                crate::report::ask(&crate::report::Prompt::yes_no(format!("Nothing changed since the last upload ({}). Upload anyway?", last.at), false))
            }
        };
        if !go {
            crate::vault::log_info(&format!("Nothing changed since the last upload ({}): not uploading {name}.", last.at));
            return Ok(Uploaded::Unchanged { since: last.at.clone() });
        }
    }
    let manifest = if dat { None } else { crate::vault::archive::read_zip_manifest(&file).and_then(|t| serde_json::from_str(&t).ok()) };
    let side = sidecar(&file, manifest.as_ref(), &fp)?;
    match upload_file(&cli, &file, &side, &series, &host, up) {
        Ok(mut done) => {
            if let Uploaded::Done { local, .. } = &mut done {
                *local = up.also_local.as_ref().map(|_| file.clone());
            }
            Ok(done)
        }
        Err(e) => {
            // A session that ended mid-way: keep the archive so it can go up without a rebuild
            if e.downcast_ref::<Problem>().is_some_and(|p| *p == Problem::SignedOut)
                && let Some(st) = staging
            {
                let kept = st.keep();
                anyhow::bail!(
                    "The Proton Drive session ended. The backup is kept in {}: sign in (kast drive login), then upload it with: kast drive upload {}",
                    kept.display(),
                    kept.join(&name).display()
                );
            }
            Err(e)
        }
    }
}

/// Upload `file` (with its sidecar) into this host's folder, then verify, record and apply
/// retention. Errors are `Problem`s where proton-drive said what went wrong.
pub fn upload_file(cli: &Cli, file: &std::path::Path, side: &serde_json::Value, series: &Series, host: &str, up: &UploadOpts) -> Result<Uploaded> {
    let s = settings::load();
    let base = s.folder();
    let host_folder = proton::join(&base, &[host]);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = std::fs::metadata(file)?.len();
    let side_path = file.with_file_name(sidecar_name(&name));
    settings::user_write(&side_path, (serde_json::to_string_pretty(side)? + "\n").as_bytes(), 0o600)?;
    struct Remove(std::path::PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _side_guard = Remove(side_path.clone());

    ensure_folders(cli, &base, host)?;
    crate::vault::log_step(&format!("Uploading {name} ({}) to Proton Drive: {host_folder}", human(size)));
    let summary = upload_with_retry(cli, &[file.to_path_buf(), side_path.clone()], &host_folder, &name, size)?;
    if summary.failed_items > 0 {
        let text: Vec<String> = summary.failures.iter().map(|f| format!("{}: {}", f.name, f.error)).collect();
        let problem = proton::classify(&proton::Exit::Code(1), &text.join("\n"));
        crate::vault::log_error(&format!("The upload failed: {}", text.join("; ")));
        return Err(problem.into());
    }
    let identical = summary.transferred_items == 0 && summary.skipped_items > 0;
    let remote = proton::join(&host_folder, &[&name]);
    if identical {
        crate::vault::log_info("An identical file is already on Proton Drive: nothing new was sent.");
    } else {
        match cli.info(&remote) {
            Ok(Some(n)) if n.size.is_some_and(|r| r != size) => {
                anyhow::bail!(
                    "Proton Drive reports {} for {remote}, but the file has {}: the upload may be incomplete",
                    human(n.size.unwrap_or(0)),
                    human(size)
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => crate::vault::log_warn(&format!("Uploaded, but {remote} isn't there under that name (renamed on a clash?)")),
            Err(p) => crate::vault::log_warn(&format!("Uploaded; could not check its size: {p}")),
        }
        crate::vault::log_success(&format!("Uploaded to Proton Drive: {remote} ({})", human(size)));
    }
    let mut state = DriveState::load();
    let fp = side["content_fingerprint"].as_str().unwrap_or_default().to_string();
    state.last.insert(series_key(host, series), LastUpload { fingerprint: fp, at: crate::ctx::now_str("%Y-%m-%d %H:%M"), file: remote.clone() });
    if let Err(e) = state.save() {
        crate::vault::log_warn(&format!("Could not save {}: {e}", DriveState::path().display()));
    }
    if !identical {
        crate::journal::remote_upload(
            &remote,
            &proton::join(&host_folder, &[&sidecar_name(&name)]),
            size,
            side["sha256"].as_str().unwrap_or_default(),
            &host_folder,
        );
    }
    let trashed = apply_retention(cli, &host_folder, series, up.keep, up.retention_ok)?;
    Ok(Uploaded::Done { remote, size, identical, trashed, local: None })
}

/// `/my-files/kast-backups/<host>`, folder by folder ("already exists" is fine).
fn ensure_folders(cli: &Cli, base: &str, host: &str) -> Result<()> {
    let full = proton::join(base, &[host]);
    if cli.info(&full)?.is_some() {
        return Ok(());
    }
    let rest = full.strip_prefix(proton::MY_FILES).unwrap_or(&full).trim_matches('/').to_string();
    let mut parent = proton::MY_FILES.to_string();
    for part in rest.split('/').filter(|p| !p.is_empty()) {
        cli.create_folder(&parent, part)?;
        parent = proton::join(&parent, &[part]);
    }
    Ok(())
}

/// Upload, showing the size and the time it's taking. Rate-limited: one more try after 60 s.
fn upload_with_retry(cli: &Cli, files: &[std::path::PathBuf], parent: &str, name: &str, size: u64) -> Result<proton::Summary> {
    let mut tried = false;
    loop {
        let start = std::time::Instant::now();
        let dash = crate::ui::Dash::new(false, 0, 1, 1, "Upload to Proton Drive");
        let tui = crate::report::owns_terminal();
        let mut on_line = |l: Option<&str>| match l {
            Some(line) => crate::report::current().output_line(line),
            None if tui => {
                let e = start.elapsed().as_secs();
                dash.render(0, 1, &format!("Uploading {name} ({}) · {}:{:02}", human(size), e / 60, e % 60), "", None);
            }
            None => {}
        };
        match cli.upload(files, parent, &mut on_line) {
            Err(Problem::RateLimited(m)) if !tried => {
                tried = true;
                crate::vault::log_warn(&format!("Proton Drive is rate-limiting this account ({m}); trying once more in 60 seconds."));
                for _ in 0..60 {
                    crate::report::checkpoint();
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            Err(Problem::Canceled) => {
                crate::report::checkpoint();
                anyhow::bail!("Upload canceled");
            }
            Err(p) => return Err(p.into()),
            Ok(s) => return Ok(s),
        }
    }
}

/// Keep the newest `keep` of `series` in the host folder; older ones (and their sidecars) go
/// to Proton Drive's trash, where they can still be restored. Asks once, interactively.
fn apply_retention(cli: &Cli, host_folder: &str, series: &Series, keep: u32, ok: bool) -> Result<Vec<String>> {
    let names: Vec<String> = match cli.list(host_folder) {
        Ok(n) => n.into_iter().filter(|n| n.kind != proton::Kind::Folder).map(|n| n.name).collect(),
        Err(p) => {
            crate::vault::log_warn(&format!("Could not list {host_folder} to apply retention: {p}"));
            return Ok(Vec::new());
        }
    };
    let victims = retention_victims(&names, series, keep as usize);
    if victims.is_empty() {
        return Ok(victims);
    }
    let archives: Vec<&String> = victims.iter().filter(|v| !v.ends_with(SIDECAR_EXT)).collect();
    let mut state = DriveState::load();
    if !ok && !state.retention_confirmed {
        let p = crate::report::Prompt::yes_no(
            format!("Keep the newest {keep} backups of this kind on Proton Drive and move {} older ones to its trash?", archives.len()),
            true,
        )
        .with_details(
            archives.iter().map(|a| a.to_string()).chain(["(Proton Drive's trash keeps them restorable; kast never empties it.)".to_string()]).collect(),
        );
        if !crate::report::ask(&p) {
            crate::vault::log_info("Retention skipped: older backups stay on Proton Drive.");
            return Ok(Vec::new());
        }
        state.retention_confirmed = true;
        let _ = state.save();
    }
    let paths: Vec<String> = victims.iter().map(|v| proton::join(host_folder, &[v])).collect();
    cli.trash(&paths).map_err(|p| anyhow::anyhow!("Could not move old backups to Proton Drive's trash: {p}"))?;
    for a in &archives {
        crate::vault::log_info(&format!("Moved to Proton Drive's trash (retention, keep {keep}): {a}"));
    }
    Ok(victims)
}

/// `kast backup … --upload proton`: one backup and upload per target.
pub fn cmd_backup_upload(opts: Vec<crate::vault::backup::BackupOpts>, up: UploadOpts) -> Result<Vec<Uploaded>> {
    let mut out = Vec::new();
    for o in opts {
        let dry = o.dry_run;
        if dry {
            crate::vault::log_warn("--dry-run with --upload: the backup is only simulated and nothing is uploaded.");
            crate::vault::backup::cmd_backup(o)?;
            continue;
        }
        let r = crate::journal::recorded(&["export"], false, || backup_upload(o, &up))?;
        out.push(r);
    }
    Ok(out)
}

/// `kast drive upload <file>`: an archive built earlier (a kept staging copy, a local backup).
pub fn cmd_upload_file(file: &std::path::Path, keep: Option<u32>) -> Result<()> {
    let cli = ready_cli()?;
    let c = ctx();
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dat = crate::vault::is_dat(file);
    let manifest: Option<serde_json::Value> =
        if dat { None } else { crate::vault::archive::read_zip_manifest(file).and_then(|t| serde_json::from_str(&t).ok()) };
    let target =
        manifest.as_ref().and_then(|m| m["target"].as_str().map(String::from)).unwrap_or_else(|| name.split("_backup_").next().unwrap_or("all").to_string());
    let series = if dat { Series::FaceModel(c.target_user.clone()) } else { Series::Target(name.split("_backup_").next().map(String::from).unwrap_or(target)) };
    let fp = if dat { fingerprint_file(file)? } else { fingerprint_zip(file)? };
    let side = sidecar(file, manifest.as_ref(), &fp)?;
    let up = UploadOpts { keep: keep.unwrap_or_else(|| settings::load().keep()), ..Default::default() };
    crate::journal::recorded(&["export"], false, || upload_file(&cli, file, &side, &series, &host_name(), &up))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn retention_picks_per_target_and_trashes_sidecars() {
        let on_drive = names(&[
            "all_backup_2026-10-01_100000.zip",
            "all_backup_2026-10-01_100000.zip.manifest.json",
            "all_backup_2026-10-03_100000.zip",
            "all_backup_2026-10-03_100000.zip.manifest.json",
            "all_backup_2026-10-02_100000 (1).zip",
            "all_backup_2026-10-04_090000.zip",
            "apps_backup_2026-09-01_100000.zip",
            "zen_backup_2026-08-01_100000.zip",
            "p3ter_face_model_2026-07-01_100000.dat",
            "p3ter_face_model_2026-07-02_100000.dat",
            "all_backup_notes.zip",
            "something else.txt",
        ]);
        let all = Series::Target("all".into());
        assert_eq!(
            retention_victims(&on_drive, &all, 2),
            names(&["all_backup_2026-10-02_100000 (1).zip", "all_backup_2026-10-01_100000.zip", "all_backup_2026-10-01_100000.zip.manifest.json"])
        );
        assert!(retention_victims(&on_drive, &all, 5).is_empty());
        // Never another target's (apps_ isn't all_), and keep is at least 1
        assert!(retention_victims(&on_drive, &Series::Target("apps".into()), 1).is_empty());
        assert_eq!(retention_victims(&on_drive, &Series::Target("zen".into()), 0), Vec::<String>::new());
        assert_eq!(retention_victims(&on_drive, &Series::FaceModel("p3ter".into()), 1), names(&["p3ter_face_model_2026-07-01_100000.dat"]));
        // A target name with regex characters is taken literally
        assert_eq!(Series::Target("a.b".into()).timestamp("aXb_backup_2026-10-01_100000.zip"), None);
    }

    #[test]
    fn remote_names() {
        assert_eq!(archive_name("all", "2026-10-17_101500"), "all_backup_2026-10-17_101500.zip");
        assert_eq!(face_model_name("p3ter", "2026-10-17_101500"), "p3ter_face_model_2026-10-17_101500.dat");
        assert_eq!(sidecar_name("a.zip"), "a.zip.manifest.json");
        assert_eq!(series_key("box", &Series::Target("all".into())), "box/all");
        assert_eq!(proton::join(crate::settings::DEFAULT_FOLDER, &["box", "all_backup_x.zip"]), "/my-files/kast-backups/box/all_backup_x.zip");
    }

    fn zip_of(dir: &std::path::Path, created: &str, extra: &str) -> std::path::PathBuf {
        let src = dir.join(format!("src-{created}-{}", extra.len()));
        std::fs::create_dir_all(src.join("zen")).unwrap();
        std::fs::write(src.join(crate::vault::archive::MANIFEST), format!("{{\"created_at\":\"{created}\",\"target\":\"zen\",\"apps\":{{\"zen\":{{}}}}}}"))
            .unwrap();
        std::fs::write(src.join("zen/prefs.js"), format!("prefs{extra}")).unwrap();
        let zip = dir.join(format!("{created}-{}.zip", extra.len()));
        crate::vault::archive::create_zip_archive(&src, &zip).unwrap();
        zip
    }

    #[test]
    fn fingerprint_follows_content_not_time() {
        let t = tempfile::tempdir().unwrap();
        let a = zip_of(t.path(), "2026-10-01T10:00:00+02:00", "");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = zip_of(t.path(), "2026-10-02T11:00:00+02:00", "");
        let c = zip_of(t.path(), "2026-10-02T11:00:00+02:00", "x");
        assert_ne!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), "the archives differ");
        assert_eq!(fingerprint_zip(&a).unwrap(), fingerprint_zip(&b).unwrap());
        assert_ne!(fingerprint_zip(&a).unwrap(), fingerprint_zip(&c).unwrap());
        assert_eq!(fingerprint_zip(&a).unwrap().len(), 64);
    }

    #[test]
    fn sidecar_carries_manifest_and_checksum() {
        crate::ctx::init_for_tests();
        let t = tempfile::tempdir().unwrap();
        let z = zip_of(t.path(), "2026-10-01T10:00:00+02:00", "");
        let m: serde_json::Value = serde_json::from_str(&crate::vault::archive::read_zip_manifest(&z).unwrap()).unwrap();
        let s = sidecar(&z, Some(&m), "fp").unwrap();
        assert_eq!(s["size"].as_u64(), Some(std::fs::metadata(&z).unwrap().len()));
        assert_eq!(s["sha256"].as_str(), crate::journal::sha256_file(&z).as_deref());
        assert_eq!((s["content_fingerprint"].as_str(), s["created_at"].as_str()), (Some("fp"), Some("2026-10-01T10:00:00+02:00")));
        assert_eq!(s["apps"], json!(["zen"]));
        assert_eq!(s["kast_version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(s["manifest"]["target"].as_str(), Some("zen"));
    }

    #[test]
    fn staging_is_private_and_always_wiped() {
        use std::os::unix::fs::PermissionsExt;
        crate::ctx::init_for_tests();
        let t = tempfile::tempdir().unwrap();
        // Dropped (success, failure and cancel all end here)
        let st = Staging::under(t.path()).unwrap();
        let dir = st.dir.clone();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::write(dir.join("a.zip"), "x").unwrap();
        drop(st);
        assert!(!dir.exists());
        // An unwinding job (a cancel) drops it too
        let d2 = std::panic::catch_unwind(|| {
            let st = Staging::under(t.path()).unwrap();
            let d = st.dir.clone();
            std::fs::write(d.join("b"), "x").unwrap();
            std::panic::resume_unwind(Box::new((d, st.dir.clone())));
        })
        .unwrap_err();
        let (d, _) = d2.downcast_ref::<(std::path::PathBuf, std::path::PathBuf)>().unwrap();
        assert!(!d.exists());
        // Kept for a retry: stays, and isn't wiped on exit
        let kept = Staging::under(t.path()).unwrap().keep();
        crate::ctx::cleanup_private_dirs();
        assert!(kept.exists());
        // A signal on the way out: cleanup_on_exit's sweep removes what's still tracked
        let st = Staging::under(t.path()).unwrap();
        let d3 = st.dir.clone();
        std::mem::forget(st);
        crate::ctx::cleanup_private_dirs();
        assert!(!d3.exists());
    }

    /// A reporter for engines that must not ask anything: a question fails the test.
    pub struct NoPrompts;

    impl crate::report::Reporter for NoPrompts {
        fn log(&self, _: crate::report::Level, _: &str) {}
        fn progress(&self, _: &crate::report::Progress) {}
        fn ask(&self, p: &crate::report::Prompt) -> bool {
            panic!("unexpected question: {}", p.text)
        }
    }

    #[test]
    fn upload_verify_record_and_retention_with_the_fake_cli() {
        crate::ctx::init_for_tests();
        crate::report::install(std::sync::Arc::new(NoPrompts));
        let (dir, cli) = proton::tests::fake("ok");
        let t = tempfile::tempdir().unwrap();
        let host = format!("host-{}", std::process::id());
        let series = Series::Target("zen".into());
        let up = UploadOpts { keep: 1, retention_ok: true, ..Default::default() };
        let mut uploaded = Vec::new();
        for (i, ts) in ["2026-10-01_100000", "2026-10-02_100000"].iter().enumerate() {
            let z = zip_of(t.path(), ts, &"x".repeat(i));
            let named = t.path().join(archive_name("zen", ts));
            std::fs::rename(&z, &named).unwrap();
            let side = sidecar(&named, None, &format!("fp{i}")).unwrap();
            let r = upload_file(&cli, &named, &side, &series, &host, &up).unwrap();
            let Uploaded::Done { remote, identical: false, trashed, .. } = r else { panic!("{r:?}") };
            assert!(!named.with_file_name(sidecar_name(&archive_name("zen", ts))).exists(), "the local sidecar is removed");
            uploaded.push((remote, trashed));
        }
        let folder = dir.path().join(format!("drive/my-files/kast-backups/{host}"));
        let mut on_drive: Vec<String> = std::fs::read_dir(&folder).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        on_drive.sort();
        assert_eq!(on_drive, names(&["zen_backup_2026-10-02_100000.zip", "zen_backup_2026-10-02_100000.zip.manifest.json"]));
        assert_eq!(uploaded[1].1, names(&["zen_backup_2026-10-01_100000.zip", "zen_backup_2026-10-01_100000.zip.manifest.json"]));
        let side: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join("zen_backup_2026-10-02_100000.zip.manifest.json")).unwrap()).unwrap();
        assert_eq!(side["content_fingerprint"].as_str(), Some("fp1"));
        let state = DriveState::load();
        assert_eq!(state.last[&series_key(&host, &series)].fingerprint, "fp1");
        // A full Drive: the upload fails with the reason
        proton::tests::set_mode(&dir, "upload_fail");
        let z = t.path().join(archive_name("zen", "2026-10-03_100000"));
        std::fs::write(&z, "zip").unwrap();
        let err = upload_file(&cli, &z, &sidecar(&z, None, "fp2").unwrap(), &series, &host, &up).unwrap_err();
        assert_eq!(err.downcast_ref::<Problem>(), Some(&Problem::QuotaFull("x: ValidationError: Insufficient storage quota".into())));
        assert_eq!(err.to_string(), "Proton Drive is full");
    }

    #[test]
    fn host_names() {
        assert_eq!(sanitize_host("archbox"), "archbox");
        assert_eq!(sanitize_host("my box/../ü"), "my_box_..__");
        assert_eq!(sanitize_host("..."), "unknown-host");
        assert_eq!(sanitize_host(""), "unknown-host");
        assert_eq!(sanitize_host(".hidden."), "hidden");
        assert_eq!(sanitize_host("Host-1.lan"), "Host-1.lan");
    }

    #[test]
    fn explanations() {
        assert_eq!(status_word(&Status::KeyringUnavailable("x".into())), "keyring locked");
        assert!(explain(&Status::KeyringUnavailable("x".into()), true).iter().any(|l| l.contains("pass")));
        assert!(!explain(&Status::KeyringUnavailable("x".into()), false).iter().any(|l| l.contains("in pass")));
        assert!(NotReady(Problem::SignedOut).to_string().contains("kast drive login"));
    }
}
