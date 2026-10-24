//! The user's own kast settings (`~/.config/kast/settings.toml`) and state files
//! (`~/.local/state/kast/*.json`), and the helpers that touch the user's home.
//!
//! Everything written into the home is written *as the user* (through `run_as_user` when kast
//! is root), never as root and then chowned: a root write into a folder the user controls can
//! be redirected by a symlink. Reads as root don't follow symlinks and only accept files the
//! user (or root) owns.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;

use crate::ctx::{self, ctx};

pub const DEFAULT_FOLDER: &str = "/my-files/kast-backups";
pub const DEFAULT_KEEP: u32 = 5;
const MAX_READ: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub proton: Proton,
    pub schedule: Schedule,
    pub vault: Vault,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Proton {
    /// "keychain" (Secret Service / KWallet) or "pass"
    pub credentials_store: Option<String>,
    /// Where backups go on Drive (`/my-files/…`)
    pub folder: Option<String>,
    /// Archives kept per target and host
    pub keep: Option<u32>,
    /// Also keep a local copy here
    pub also_local: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub notify_success: bool,
    /// What the scheduled backup covers (as last set up from kast)
    pub targets: Option<Vec<String>>,
    /// "daily" or "weekly"
    pub every: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Vault {
    /// `all` (and so the schedule) includes KDE Wallet and GNOME Keyring
    pub keyrings: bool,
}

impl Settings {
    /// Unknown keys are ignored; a file that doesn't parse gives the defaults (and a warning).
    pub fn parse(text: &str) -> Result<Settings, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// The remote base folder: under /my-files, no `..`, else the default.
    pub fn folder(&self) -> String {
        match self.proton.folder.as_deref().map(|f| f.trim().trim_end_matches('/')) {
            Some(f) if f.starts_with("/my-files/") && !f.split('/').any(|p| p == ".." || p == ".") && !f.contains("//") => f.to_string(),
            _ => DEFAULT_FOLDER.to_string(),
        }
    }

    pub fn keep(&self) -> u32 {
        self.proton.keep.filter(|k| *k >= 1).unwrap_or(DEFAULT_KEEP)
    }

    /// The credential store to pass on; only the two safe ones.
    pub fn store(&self) -> Option<String> {
        self.proton.credentials_store.clone().filter(|s| s == "pass" || s == "keychain")
    }

    pub fn also_local(&self) -> Option<PathBuf> {
        self.proton.also_local.as_deref().map(str::trim).filter(|s| s.starts_with('/')).map(PathBuf::from)
    }
}

/// Set (or with None remove) `path` (`["proton", "keep"]`) in the TOML `text`, keeping every
/// other key, known to kast or not.
pub fn set_key(text: &str, path: &[&str], value: Option<toml::Value>) -> Result<String, String> {
    let mut root: toml::Table = if text.trim().is_empty() { toml::Table::new() } else { text.parse().map_err(|e: toml::de::Error| e.to_string())? };
    let (last, parents) = path.split_last().ok_or("empty key")?;
    let mut t = &mut root;
    for p in parents {
        let entry = t.entry(p.to_string()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        t = entry.as_table_mut().unwrap();
    }
    match value {
        Some(v) => {
            t.insert(last.to_string(), v);
        }
        None => {
            t.remove(*last);
        }
    }
    toml::to_string(&root).map_err(|e| e.to_string())
}

pub fn path() -> PathBuf {
    ctx().config_home.join("kast/settings.toml")
}

pub fn state_dir() -> PathBuf {
    ctx().state_home.join("kast")
}

pub fn load() -> Settings {
    let Some(text) = user_read(&path()) else { return Settings::default() };
    Settings::parse(&text).unwrap_or_else(|e| {
        ctx::log("WARN", &format!("Ignoring {} (not valid TOML: {e})", path().display()));
        Settings::default()
    })
}

/// Change one setting (creating the file the first time something is set).
pub fn update(key: &[&str], value: Option<toml::Value>) -> io::Result<()> {
    let p = path();
    let text = user_read(&p).unwrap_or_default();
    let new = set_key(&text, key, value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", p.display())))?;
    user_write(&p, new.as_bytes(), 0o600)
}

// --- Files in the user's home ----------------------------------------------------------------

/// Read a small file of the user's: not through a symlink, owned by them (or root).
pub fn user_read(path: &Path) -> Option<String> {
    read_owned(path, ctx().target_uid)
}

pub fn read_owned(path: &Path, uid: u32) -> Option<String> {
    let f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    let m = f.metadata().ok()?;
    if !m.is_file() || (m.uid() != uid && m.uid() != 0) || m.len() > MAX_READ {
        return None;
    }
    let mut s = String::new();
    f.take(MAX_READ).read_to_string(&mut s).ok()?;
    Some(s)
}

const WRITE_SCRIPT: &str = r#"umask 077; mkdir -p -- "$1" || exit 1; t=$(mktemp -- "$1/.kast-XXXXXX") || exit 1; if cat > "$t" && chmod "$3" -- "$t" && mv -f -- "$t" "$2"; then exit 0; fi; rm -f -- "$t"; exit 1"#;

/// The argv that writes stdin to `path` as the user (atomically, through a temp file).
pub fn write_argv(path: &Path, mode: u32) -> Vec<String> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    ["sh", "-c", WRITE_SCRIPT, "kast-write"]
        .iter()
        .map(|s| s.to_string())
        .chain([dir.display().to_string(), path.display().to_string(), format!("{mode:o}")])
        .collect()
}

/// Write `data` to `path` in the user's home, as the user. Missing folders are created 0700.
pub fn user_write(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    if !ctx().is_root {
        return write_own(path, data, mode);
    }
    let argv = write_argv(path, mode);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut child = ctx::run_as_user(&refs).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(ctx::log_stdio()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data)?;
    }
    let st = child.wait()?;
    if st.success() { Ok(()) } else { Err(io::Error::other(format!("could not write {} as {}", path.display(), ctx().target_user))) }
}

/// Our own write (kast isn't root, so the file is ours anyway).
pub fn write_own(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut tmp = tempfile::Builder::new().prefix(".kast-").tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Create `dir` (and its parents) as the user, the last one 0700, and check the result is a
/// real directory of theirs that nobody else can enter.
pub fn user_mkdir_private(dir: &Path) -> io::Result<()> {
    let c = ctx();
    if c.is_root {
        let d = dir.display().to_string();
        let st = ctx::run_as_user(&["sh", "-c", "umask 077; mkdir -p -- \"$1\" && chmod 700 -- \"$1\"", "kast-mkdir", &d])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(ctx::log_stdio())
            .status()?;
        if !st.success() {
            return Err(io::Error::other(format!("could not create {d} as {}", c.target_user)));
        }
    } else {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    check_private(dir, c.target_uid)
}

pub fn check_private(dir: &Path, uid: u32) -> io::Result<()> {
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!("{} is not a private folder of the user's", dir.display())));
    }
    Ok(())
}

/// Remove a folder of the user's (staging), as the user.
pub fn user_remove_dir(dir: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    if ctx().is_root {
        let d = dir.display().to_string();
        let st = ctx::run_as_user(&["rm", "-rf", "--", &d]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(ctx::log_stdio()).status()?;
        if st.success() {
            return Ok(());
        }
        // sudo itself failed (e.g. on the way out): Rust's remove_dir_all never follows links
    }
    fs::remove_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_unknown_keys() {
        let s = Settings::parse("").unwrap();
        assert_eq!((s.folder(), s.keep(), s.store(), s.also_local()), (DEFAULT_FOLDER.to_string(), 5, None, None));
        let s = Settings::parse(
            "future = 1\n[proton]\ncredentials_store = \"pass\"\nfolder = \"/my-files/Backups/kast/\"\nkeep = 3\nalso_local = \"/media/data/backups\"\nnew_key = true\n\
             [schedule]\nnotify_success = true\n[vault]\nkeyrings = true\n[other]\nx = 1\n",
        )
        .unwrap();
        assert_eq!(s.folder(), "/my-files/Backups/kast");
        assert_eq!((s.keep(), s.store().as_deref(), s.schedule.notify_success), (3, Some("pass"), true));
        assert_eq!(s.also_local(), Some(PathBuf::from("/media/data/backups")));
        assert!(s.vault.keyrings);
        assert!(!Settings::parse("").unwrap().vault.keyrings, "keyrings stay out of all by default");
        assert!(Settings::parse("[proton\n").is_err());
    }

    #[test]
    fn unsafe_values_fall_back() {
        let s = |t: &str| Settings::parse(t).unwrap();
        assert_eq!(s("[proton]\nfolder = \"/trash/x\"").folder(), DEFAULT_FOLDER);
        assert_eq!(s("[proton]\nfolder = \"/my-files/../x\"").folder(), DEFAULT_FOLDER);
        assert_eq!(s("[proton]\nkeep = 0").keep(), DEFAULT_KEEP);
        assert_eq!(s("[proton]\ncredentials_store = \"unsafe_file\"").store(), None);
        assert_eq!(s("[proton]\nalso_local = \"relative\"").also_local(), None);
    }

    #[test]
    fn round_trip_keeps_other_keys() {
        let text = "# mine\nfuture = 1\n[proton]\nkeep = 3\nmystery = \"x\"\n";
        let new = set_key(text, &["proton", "credentials_store"], Some("pass".into())).unwrap();
        let s = Settings::parse(&new).unwrap();
        assert_eq!((s.store().as_deref(), s.keep()), (Some("pass"), 3));
        let table: toml::Table = new.parse().unwrap();
        assert_eq!(table["future"].as_integer(), Some(1));
        assert_eq!(table["proton"]["mystery"].as_str(), Some("x"));
        let new = set_key(&new, &["schedule", "notify_success"], Some(true.into())).unwrap();
        let new = set_key(&new, &["proton", "keep"], None).unwrap();
        let s = Settings::parse(&new).unwrap();
        assert_eq!((s.keep(), s.schedule.notify_success), (DEFAULT_KEEP, true));
        assert_eq!(Settings::parse(&set_key("", &["proton", "keep"], Some(7.into())).unwrap()).unwrap().keep(), 7);
        assert!(set_key("[x\n", &["a"], None).is_err());
    }

    #[test]
    fn own_writes_are_private_and_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a/b/settings.toml");
        write_own(&p, b"x = 1\n", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "x = 1\n");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(tmp.path().join("a/b")).unwrap().permissions().mode() & 0o777, 0o700);
        let me = nix::unistd::Uid::effective().as_raw();
        assert_eq!(read_owned(&p, me).as_deref(), Some("x = 1\n"));
        // Not through a symlink, and not someone else's
        std::os::unix::fs::symlink(&p, tmp.path().join("link")).unwrap();
        assert_eq!(read_owned(&tmp.path().join("link"), me), None);
        assert_eq!(read_owned(&p, me + 1), if me == 0 { Some("x = 1\n".into()) } else { None });
    }

    #[test]
    fn write_argv_goes_through_a_temp_file() {
        let v = write_argv(Path::new("/home/p/.config/kast/settings.toml"), 0o600);
        assert_eq!(&v[..2], ["sh", "-c"]);
        assert!(v[2].contains("umask 077") && v[2].contains("mktemp") && v[2].contains("mv -f"));
        assert_eq!(&v[4..], ["/home/p/.config/kast", "/home/p/.config/kast/settings.toml", "600"]);
        // The script really works (run as ourselves)
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("d/f.json");
        let v = write_argv(&target, 0o640);
        let mut child = std::process::Command::new(&v[0]).args(&v[1..]).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"{}").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(tmp.path().join("d")).unwrap().count(), 1);
    }

    #[test]
    fn private_dir_check() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("s");
        fs::DirBuilder::new().mode(0o700).create(&d).unwrap();
        let me = nix::unistd::Uid::effective().as_raw();
        assert!(check_private(&d, me).is_ok());
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_private(&d, me).is_err());
        std::os::unix::fs::symlink(&d, tmp.path().join("l")).unwrap();
        assert!(check_private(&tmp.path().join("l"), me).is_err());
    }
}
