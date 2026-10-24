//! Desktop keyrings: KDE Wallet (`kwallet`) and GNOME Keyring (`gnome-keyring`).
//!
//! The Chromium and Electron apps kast backs up keep the key that encrypts their saved
//! passwords, cookies and tokens in one of these, so restoring their profiles next to an
//! empty keyring signs the user out anyway. Which one an app uses depends on the desktop it
//! runs in: Chromium's os_crypt (`FreedesktopSecretKeyProvider`) talks to `org.kde.kwalletd6`
//! when `XDG_CURRENT_DESKTOP` says KDE, and to whatever owns `org.freedesktop.secrets` (usually
//! gnome-keyring) everywhere else, Hyprland included; `--password-store=` overrides it. A user
//! of both desktops can have keys in both stores.
//!
//! kast copies the encrypted files as they are on disk. It never asks for, reads or stores a
//! keyring password, and never opens or unlocks a keyring: only the file headers are read, to
//! tell the formats apart.
//!
//! Formats (checked against the sources):
//! - KWallet (`kwalletbackend.cc`, `backendpersisthandler.cpp`): `<name>.kwl` starts with the
//!   12-byte magic `KWALLET\n\r\0\r\n`, then major and minor version, cipher (0 Blowfish ECB,
//!   1 3DES (unsupported), 2 GPG, 3 Blowfish CBC) and hash (0 SHA1, 1 MD5 (unsupported),
//!   2 PBKDF2-SHA512). The PBKDF2 salt is `<name>.salt` (56 bytes): without it the wallet
//!   can't be opened. KWallet 6's ksecretd keeps the Secret Service attributes of a wallet's
//!   items in `<name>_attributes.json` next to it. There is no unencrypted KWallet format.
//! - GNOME Keyring (`gkm-secret-binary.c`, `gkm-secret-textual.c`): a keyring with a password
//!   starts with `GnomeKeyring\n\r\0\n`, then version, crypto (0 AES-128-CBC) and hash bytes,
//!   the name, two times, flags, lock timeout, the hash iterations and an 8-byte salt: the
//!   salt is inside the file. A keyring with an empty password is written as plain text (a
//!   key file starting with `[keyring]`) with every secret readable.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::ctx::{self, ctx, have, ok};

use super::info::jq_str;
use super::{log_error, log_info, log_step, log_success, log_warn, rsync_as, rsync_dir_as, running_pids, terminate_apps_with};

pub const KWALLET: &str = "kwallet";
pub const GNOME: &str = "gnome-keyring";
/// What the `keyrings` group means
pub const TARGETS: [&str; 2] = [KWALLET, GNOME];

/// The export screen's note for the keyring targets.
pub const NOTE: &str = "Encrypted with your keyring password; needed for apps to stay signed in after a restore";
pub const OFFLINE_NOTE: &str = "Anyone with the archive can try passwords against it offline (KWallet: PBKDF2-SHA512, GNOME Keyring: iterated SHA-256): a weak keyring password can be cracked.";

pub fn is_keyring(app: &str) -> bool {
    TARGETS.contains(&app)
}

pub fn kwallet_dir() -> PathBuf {
    ctx().data_home.join("kwalletd")
}

pub fn gnome_dir() -> PathBuf {
    ctx().data_home.join("keyrings")
}

pub fn kwalletrc_path() -> PathBuf {
    ctx().config_home.join("kwalletrc")
}

// --- File formats ---------------------------------------------------------------------------

const KWALLET_MAGIC: &[u8] = b"KWALLET\n\r\0\r\n";
const GNOME_MAGIC: &[u8] = b"GnomeKeyring\n\r\0\n";

/// What a keyring file is, from its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    BlowfishPbkdf2,
    /// Blowfish ECB, or a SHA1 password hash (written before KDE 4.13)
    BlowfishLegacy,
    Gpg,
    GnomeAes,
    /// A GNOME keyring without a password: its secrets are readable
    Plaintext,
    Unknown,
}

impl Kind {
    /// As the manifest records it.
    pub fn id(self) -> &'static str {
        match self {
            Kind::BlowfishPbkdf2 => "blowfish-pbkdf2",
            Kind::BlowfishLegacy => "blowfish-legacy",
            Kind::Gpg => "gpg",
            Kind::GnomeAes => "gnome-aes",
            Kind::Plaintext => "plaintext",
            Kind::Unknown => "unknown",
        }
    }

    pub fn from_id(id: &str) -> Kind {
        [Kind::BlowfishPbkdf2, Kind::BlowfishLegacy, Kind::Gpg, Kind::GnomeAes, Kind::Plaintext].into_iter().find(|k| k.id() == id).unwrap_or(Kind::Unknown)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Kind::BlowfishPbkdf2 => "Blowfish, PBKDF2-SHA512",
            Kind::BlowfishLegacy => "Blowfish, legacy (ECB or SHA1)",
            Kind::Gpg => "GPG-encrypted",
            Kind::GnomeAes => "AES, password-protected",
            Kind::Plaintext => "NO PASSWORD: plain text",
            Kind::Unknown => "unknown format",
        }
    }
}

/// The type of a KWallet `.kwl` from its first 16 bytes.
pub fn kwallet_kind(head: &[u8]) -> Kind {
    if head.len() < 16 || &head[..12] != KWALLET_MAGIC || head[12] != 0 {
        return Kind::Unknown;
    }
    match (head[14], head[15]) {
        (3, 2) => Kind::BlowfishPbkdf2,
        (0, 0 | 2) | (3, 0) => Kind::BlowfishLegacy,
        (2, 0) => Kind::Gpg,
        _ => Kind::Unknown,
    }
}

/// The type of a GNOME `.keyring`, and the hash iterations of a password-protected one.
pub fn gnome_kind(data: &[u8]) -> (Kind, Option<u32>) {
    if data.starts_with(GNOME_MAGIC) {
        if data.len() < 20 || data[16..20] != [0, 0, 0, 0] {
            return (Kind::Unknown, None);
        }
        return (Kind::GnomeAes, gnome_iterations(data));
    }
    let text = String::from_utf8_lossy(&data[..data.len().min(4096)]);
    let first = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'));
    if first == Some("[keyring]") { (Kind::Plaintext, None) } else { (Kind::Unknown, None) }
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// After the 20 header bytes: the name (length-prefixed, 0xffffffff for none), two times
/// (8 bytes each), flags, lock timeout, then the iterations.
fn gnome_iterations(data: &[u8]) -> Option<u32> {
    let len = be32(data, 20)?;
    let mut at = 24usize;
    if len != u32::MAX {
        at = at.checked_add(len as usize)?;
    }
    be32(data, at.checked_add(16 + 4 + 4)?)
}

/// The key ID a GPG wallet is encrypted to: the first OpenPGP packet after the 16-byte
/// header is the public-key encrypted session key (tag 1: version 3, then the 8-byte key ID).
pub fn gpg_key_id(kwl: &[u8]) -> Option<String> {
    let p = kwl.get(16..)?;
    let tag_byte = *p.first()?;
    if tag_byte & 0x80 == 0 {
        return None;
    }
    let (tag, body) = if tag_byte & 0x40 != 0 {
        // New format: one, two or five length bytes
        let skip = match *p.get(1)? {
            0..=191 => 2,
            192..=223 => 3,
            255 => 6,
            _ => return None,
        };
        (tag_byte & 0x3f, p.get(skip..)?)
    } else {
        let skip = match tag_byte & 0x03 {
            0 => 2,
            1 => 3,
            2 => 5,
            _ => 1,
        };
        ((tag_byte >> 2) & 0x0f, p.get(skip..)?)
    };
    if tag != 1 || *body.first()? != 3 {
        return None;
    }
    let id = body.get(1..9)?;
    (id != [0u8; 8]).then(|| id.iter().map(|b| format!("{b:02X}")).collect())
}

// --- kwalletrc ---------------------------------------------------------------------------------

/// A value of an INI-style KDE config, if set.
pub fn ini_get(text: &str, group: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == format!("[{group}]");
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// `text` with `pairs` set in `[group]` (the group added when missing), everything else kept.
pub fn ini_set(text: &str, group: &str, pairs: &[(&str, &str)]) -> String {
    let header = format!("[{group}]");
    let mut out: Vec<String> = Vec::new();
    let mut done = vec![false; pairs.len()];
    let mut inside = false;
    let mut seen = false;
    let flush = |out: &mut Vec<String>, done: &mut Vec<bool>| {
        // Missing keys go after the group's last non-empty line
        let at = out.iter().rposition(|l| !l.trim().is_empty()).map_or(out.len(), |i| i + 1);
        let missing: Vec<String> = pairs
            .iter()
            .zip(done.iter_mut())
            .filter(|(_, d)| !**d)
            .map(|((k, v), d)| {
                *d = true;
                format!("{k}={v}")
            })
            .collect();
        out.splice(at..at, missing);
    };
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if inside {
                flush(&mut out, &mut done);
            }
            inside = t == header;
            seen |= inside;
        } else if inside
            && let Some((k, _)) = t.split_once('=')
            && let Some(i) = pairs.iter().position(|(pk, _)| *pk == k.trim())
        {
            done[i] = true;
            out.push(format!("{}={}", pairs[i].0, pairs[i].1));
            continue;
        }
        out.push(line.to_string());
    }
    if inside {
        flush(&mut out, &mut done);
    }
    if !seen {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header);
        out.extend(pairs.iter().map(|(k, v)| format!("{k}={v}")));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// kwalletrc as a restore leaves it: KWallet on, and no first-run wizard that would create a
/// new wallet over the restored one.
pub fn restored_kwalletrc(text: &str) -> String {
    ini_set(text, "Wallet", &[("Enabled", "true"), ("First Use", "false")])
}

/// KWallet 6's `kwalletd6` forwards to a Secret Service: its own ksecretd (the wallet files,
/// `[KSecretD] Enabled`, the default) or whatever owns `org.freedesktop.secrets`.
pub fn kde_backend(kwalletrc: &str) -> &'static str {
    if ini_get(kwalletrc, "KSecretD", "Enabled").is_some_and(|v| v.eq_ignore_ascii_case("false")) { "secret-service" } else { "ksecretd" }
}

// --- Finding keyrings -----------------------------------------------------------------------

/// One keyring file found in the user's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// KWallet: `<name>.salt` exists
    pub salt: bool,
    /// KWallet: `<name>_attributes.json` exists
    pub attributes: bool,
    /// GNOME Keyring: PBKDF iterations from the header
    pub iterations: Option<u32>,
    pub gpg_key: Option<String>,
}

/// What a scan of the folders found (never the contents).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub keyrings: Vec<Found>,
    /// Every file that belongs in the archive, by name
    pub files: Vec<String>,
    pub default: Option<String>,
}

/// The folder as a real directory of the user's (not a symlink somewhere else).
fn own_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && (m.uid() == ctx().target_uid || !ctx().is_root))
}

/// Regular files directly in `dir`, sorted (no symlinks, sockets or subfolders).
fn regular_files(dir: &Path) -> Vec<(String, fs::Metadata)> {
    let mut v: Vec<(String, fs::Metadata)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let m = e.path().symlink_metadata().ok()?;
                    m.is_file().then(|| (e.file_name().to_string_lossy().into_owned(), m))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The first bytes of a file, not through a symlink.
fn head(path: &Path, n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(f) = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        let _ = f.take(n as u64).read_to_end(&mut buf);
    }
    buf
}

/// Wallet files kast carries: the wallet, its salt and its attributes (not `*.kwl~` copies).
pub fn kwallet_file(name: &str) -> bool {
    !name.starts_with('.') && (name.ends_with(".kwl") || name.ends_with(".salt") || name.ends_with("_attributes.json"))
}

/// Everything in ~/.local/share/keyrings except lock and hidden temp files.
pub fn gnome_file(name: &str) -> bool {
    !name.starts_with('.') && !name.ends_with(".lock") && !name.ends_with('~')
}

pub fn scan_kwallet(dir: &Path) -> Scan {
    let mut s = Scan::default();
    if !own_dir(dir) {
        return s;
    }
    let files = regular_files(dir);
    let has = |n: &str| files.iter().any(|(f, _)| f == n);
    for (name, m) in &files {
        if !kwallet_file(name) {
            continue;
        }
        s.files.push(name.clone());
        if let Some(stem) = name.strip_suffix(".kwl") {
            let h = head(&dir.join(name), 4096);
            let kind = kwallet_kind(&h);
            s.keyrings.push(Found {
                name: stem.to_string(),
                kind,
                size: m.len(),
                salt: has(&format!("{stem}.salt")),
                attributes: has(&format!("{stem}_attributes.json")),
                iterations: None,
                gpg_key: if kind == Kind::Gpg { gpg_key_id(&h) } else { None },
            });
        }
    }
    let rc = crate::settings::user_read(&kwalletrc_path()).unwrap_or_default();
    s.default = Some(ini_get(&rc, "Wallet", "Default Wallet").filter(|d| !d.is_empty()).unwrap_or_else(|| "kdewallet".into()));
    s
}

pub fn scan_gnome(dir: &Path) -> Scan {
    let mut s = Scan::default();
    if !own_dir(dir) {
        return s;
    }
    for (name, m) in regular_files(dir) {
        if !gnome_file(&name) {
            continue;
        }
        if let Some(stem) = name.strip_suffix(".keyring") {
            let (kind, iterations) = gnome_kind(&head(&dir.join(&name), 4096));
            s.keyrings.push(Found { name: stem.to_string(), kind, size: m.len(), salt: false, attributes: false, iterations, gpg_key: None });
        } else if name == "default" {
            s.default = String::from_utf8(head(&dir.join(&name), 256)).ok().map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        }
        s.files.push(name);
    }
    s
}

// --- The Secret Service and PAM --------------------------------------------------------------

/// A command in the user's session (through `sudo -u` with their bus when kast is root).
fn user_cmd(args: &[&str]) -> Command {
    if ctx().is_root {
        ctx::run_as_user(args)
    } else {
        let mut c = Command::new(args[0]);
        c.args(&args[1..]);
        c
    }
}

fn user_ok(args: &[&str]) -> bool {
    user_cmd(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn user_output(args: &[&str]) -> Option<String> {
    let out = user_cmd(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The program in `busctl status` output: the executable's name (`Comm=` is cut at 15
/// characters, `gnome-keyring-d`).
pub fn parse_bus_owner(out: &str) -> Option<String> {
    let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)).map(str::trim).filter(|v| !v.is_empty());
    field("Exe=").and_then(|e| e.rsplit('/').next().map(String::from)).or_else(|| field("Comm=").map(String::from))
}

/// Which program owns `org.freedesktop.secrets` on the user's bus now (doesn't start one).
pub fn secret_service_owner() -> Option<String> {
    if !have("busctl") {
        return None;
    }
    parse_bus_owner(&user_output(&["busctl", "--user", "status", "org.freedesktop.secrets"])?)
}

/// The store behind a Secret Service provider, for people.
pub fn provider_label(owner: &str) -> String {
    match owner {
        "ksecretd" | "kwalletd6" | "kwalletd5" | "kwalletd" => "KDE Wallet (ksecretd)".into(),
        "gnome-keyring-daemon" | "gnome-keyring-d" => "GNOME Keyring".into(),
        o if o.to_ascii_lowercase().contains("keepassxc") => "KeePassXC".into(),
        o => o.to_string(),
    }
}

/// Where a PAM module is set up: the files with an `auth` and a `session` line for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PamSetup {
    pub auth: Vec<String>,
    pub session: Vec<String>,
}

impl PamSetup {
    pub fn complete(&self) -> bool {
        !self.auth.is_empty() && !self.session.is_empty()
    }
}

/// The PAM files a graphical or text login goes through.
pub const PAM_FILES: [&str; 11] =
    ["sddm", "plasmalogin", "gdm-password", "greetd", "ly", "lightdm", "login", "system-login", "system-local-login", "system-auth", "passwd"];

pub fn pam_setup(files: &[(String, String)], module: &str) -> PamSetup {
    let mut s = PamSetup::default();
    for (name, text) in files {
        for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#') && l.contains(module)) {
            let kind = line.trim_start_matches('-').split_whitespace().next().unwrap_or("");
            let list = match kind {
                "auth" => &mut s.auth,
                "session" => &mut s.session,
                _ => continue,
            };
            if !list.contains(name) {
                list.push(name.clone());
            }
        }
    }
    s
}

pub fn read_pam() -> Vec<(String, String)> {
    PAM_FILES.iter().filter_map(|f| fs::read_to_string(Path::new("/etc/pam.d").join(f)).ok().map(|t| (f.to_string(), t))).collect()
}

pub const KWALLET_PAM: &str = "pam_kwallet5.so";
pub const GNOME_PAM: &str = "pam_gnome_keyring.so";

// --- Backup -------------------------------------------------------------------------------------

/// How a keyring backup treats what it finds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    pub dry_run: bool,
    /// Proton Drive or a scheduled run: never with a plain-text keyring
    pub upload: bool,
    /// A local backup may carry a plain-text keyring (`--allow-plaintext-keyring`)
    pub allow_plaintext: bool,
    /// Part of `all`: a missing folder isn't worth a warning
    pub quiet_missing: bool,
}

/// A file's bytes, read twice-checked: a daemon that rewrote it while we read gets one retry.
pub fn read_stable(path: &Path) -> std::io::Result<Vec<u8>> {
    let stamp = |m: &fs::Metadata| (m.len(), m.mtime(), m.mtime_nsec(), m.ino());
    for _ in 0..2 {
        let mut f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
        let before = f.metadata()?;
        if !before.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let mut data = Vec::with_capacity(before.len() as usize);
        f.read_to_end(&mut data)?;
        let after = fs::symlink_metadata(path)?;
        if stamp(&before) == stamp(&after) && data.len() as u64 == after.len() {
            return Ok(data);
        }
    }
    Err(std::io::Error::other("it kept changing while being read"))
}

/// Write a staged copy: 0600 in a 0700 folder.
fn stage_file(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dir.join(name))?;
    f.write_all(data)?;
    Ok(())
}

fn plaintext_left_out(name: &str, p: &Policy) -> bool {
    let fix = "set a password for it in Seahorse (Passwords and Keys: right-click the keyring, Change Password)";
    if p.upload {
        let msg = format!(
            "Leaving the GNOME keyring '{name}' out: it has no password, so its secrets are stored as plain text, and a plain-text keyring never goes to Proton Drive or a scheduled backup. To include it, {fix}."
        );
        log_warn(&msg);
        ctx::add_issue(msg);
        return true;
    }
    if !p.allow_plaintext {
        let msg = format!(
            "Left the GNOME keyring '{name}' out: it has no password, so anyone with the archive could read its secrets. {}, or back it up anyway with --allow-plaintext-keyring.",
            fix[..1].to_uppercase() + &fix[1..]
        );
        log_error(&msg);
        ctx::add_issue(msg);
        return true;
    }
    log_warn(&format!(
        "Including the GNOME keyring '{name}' as plain text (--allow-plaintext-keyring): anyone with the archive can read its secrets. Keep the archive private, or {fix}."
    ));
    false
}

/// Back up `app` (`kwallet` or `gnome-keyring`) into `staging/<app>`; the manifest entry, or
/// None when there's nothing to back up.
pub fn backup(app: &str, staging: &Path, p: &Policy) -> Result<Option<Value>> {
    let kde = app == KWALLET;
    let (dir, label) = if kde { (kwallet_dir(), "KDE Wallet") } else { (gnome_dir(), "GNOME Keyring") };
    let scan = if kde { scan_kwallet(&dir) } else { scan_gnome(&dir) };
    if scan.keyrings.is_empty() && (kde || scan.files.is_empty()) {
        let msg = format!("{label} not found ({}), skipping...", dir.display());
        if p.quiet_missing {
            ctx::log("INFO", &msg)
        } else {
            log_warn(&msg)
        }
        return Ok(None);
    }
    log_step(&format!("Processing {label} ({})...", dir.display()));

    let mut skip: Vec<String> = Vec::new();
    for k in &scan.keyrings {
        match k.kind {
            Kind::Gpg => log_warn(&format!(
                "The wallet '{}' is GPG-encrypted{}: restoring it needs your GPG private key on the new system; kast doesn't back that up.",
                k.name,
                k.gpg_key.as_deref().map(|id| format!(" (key {id})")).unwrap_or_default()
            )),
            Kind::BlowfishLegacy => log_warn(&format!(
                "The wallet '{}' uses the legacy Blowfish format (ECB or SHA1, from before KDE 4.13). It's backed up as it is; changing its password in KWalletManager saves it in the current format.",
                k.name
            )),
            Kind::BlowfishPbkdf2 if !k.salt => {
                log_warn(&format!("The wallet '{}' has no {0}.salt: without it the wallet can't be opened. Backing it up anyway.", k.name))
            }
            Kind::Plaintext if plaintext_left_out(&k.name, p) => skip.push(format!("{}.keyring", k.name)),
            Kind::Unknown => log_warn(&format!("{}: unknown file format; backed up as it is.", k.name)),
            _ => {}
        }
    }
    let default = scan.default.clone().unwrap_or_default();
    match (kde, default.as_str()) {
        (true, d) if d != "kdewallet" => {
            log_warn(&format!("The default wallet is '{d}', not 'kdewallet': PAM can unlock a wallet at login only when it's named kdewallet."))
        }
        (false, d) if !d.is_empty() && d != "login" => log_warn(&format!(
            "The default keyring is '{d}', not 'login': pam_gnome_keyring unlocks only the login keyring at login, so '{d}' will ask for its password."
        )),
        _ => {}
    }

    let sub = if kde { "kwalletd" } else { "keyrings" };
    let dest = staging.join(app).join(sub);
    let files: Vec<&String> = scan.files.iter().filter(|f| !skip.contains(f)).collect();
    if files.is_empty() {
        log_warn(&format!("Nothing of {label} left to back up."));
        return Ok(None);
    }
    for f in &files {
        if p.dry_run {
            log_info(&format!("  [Dry-run] Would copy: {} -> {app}/{sub}/", dir.join(f).display()));
            continue;
        }
        let data = read_stable(&dir.join(f)).map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.join(f).display()))?;
        stage_file(&dest, f, &data)?;
        ctx::log("INFO", &format!("Keyring file {f}: {} bytes", data.len()));
    }
    let rc = kwalletrc_path();
    let mut has_rc = false;
    if kde && rc.is_file() {
        has_rc = true;
        if p.dry_run {
            log_info(&format!("  [Dry-run] Would copy: {} -> {app}/kwalletrc", rc.display()));
        } else {
            stage_file(&staging.join(app), "kwalletrc", &read_stable(&rc)?)?;
        }
    }

    let keyrings: Vec<Value> = scan
        .keyrings
        .iter()
        .filter(|k| !skip.contains(&format!("{}.keyring", k.name)))
        .map(|k| {
            let mut v = json!({"name": k.name, "type": k.kind.id(), "size": k.size});
            if kde {
                v["salt"] = json!(k.salt);
                v["attributes"] = json!(k.attributes);
            }
            if let Some(i) = k.iterations {
                v["iterations"] = json!(i);
            }
            if let Some(id) = &k.gpg_key {
                v["gpg_key"] = json!(id);
            }
            v
        })
        .collect();
    let mut entry = json!({
        "included": true,
        "folder": app,
        "data_path": dir.to_string_lossy(),
        "keyrings": keyrings,
        "files": files,
        "default": default,
        "secret_service_owner": secret_service_owner(),
    });
    if !skip.is_empty() {
        entry["left_out"] = json!(skip);
    }
    if kde {
        entry["kwalletrc"] = json!(has_rc);
        entry["kde_backend"] = json!(kde_backend(&crate::settings::user_read(&rc).unwrap_or_default()));
    }
    Ok(Some(entry))
}

/// Plain-text keyrings among the user's (for a confirmation before a local backup).
pub fn plaintext_keyrings() -> Vec<String> {
    scan_gnome(&gnome_dir()).keyrings.into_iter().filter(|k| k.kind == Kind::Plaintext).map(|k| k.name).collect()
}

// --- Daemons ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    KWallet,
    Gnome,
}

impl Store {
    pub fn of(app: &str) -> Store {
        if app == KWALLET { Store::KWallet } else { Store::Gnome }
    }

    fn label(self) -> &'static str {
        match self {
            Store::KWallet => "the KDE Wallet daemons (ksecretd, kwalletd6)",
            Store::Gnome => "gnome-keyring-daemon",
        }
    }
}

/// How to bring a stopped keyring daemon back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restart {
    /// systemd user units to start again, in order
    pub units: Vec<String>,
    /// A daemon that ran outside systemd (PAM's `auto_start`, Hyprland's `exec-once`)
    pub argv: Option<Vec<String>>,
}

/// Stopping and starting the daemons that write the keyring files. A seam, so tests can watch
/// the order without touching the keyrings of the session they run in.
pub trait KeyringDaemons {
    /// Stop everything that writes `store`'s files; Err when something still runs.
    fn stop_keyring(&self, store: Store) -> Result<Restart, String>;
    fn start_keyring(&self, store: Store, how: &Restart) -> Result<(), String>;
}

/// The keyring daemon is stopped while this lives; [`start`](Self::start) or dropping it (an
/// error, a canceled job unwinding) brings it back as it ran before.
pub struct Stopped<'a> {
    daemons: &'a dyn KeyringDaemons,
    store: Store,
    how: Option<Restart>,
}

impl<'a> Stopped<'a> {
    /// Err: the daemon still runs, and nothing may be written under it.
    pub fn stop(daemons: &'a dyn KeyringDaemons, store: Store) -> Result<Self> {
        log_info(&format!("Stopping {} so they can't write the old keyring over the restored one...", store.label()));
        match daemons.stop_keyring(store) {
            Ok(how) => Ok(Stopped { daemons, store, how: Some(how) }),
            Err(e) => {
                log_error(&format!(
                    "Could not stop {}: {e}. Nothing was restored for it; close it (or log out and restore from a TTY) and try again.",
                    store.label()
                ));
                Err(ctx::reported())
            }
        }
    }

    pub fn start(mut self) {
        self.restart();
    }

    fn restart(&mut self) {
        let Some(how) = self.how.take() else { return };
        if let Err(e) = self.daemons.start_keyring(self.store, &how) {
            log_warn(&format!("Could not start {} again ({e}); log out and back in to get it back.", self.store.label()));
        }
    }
}

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        self.restart();
    }
}

/// The KWallet processes that hold wallets, shims first (they'd start ksecretd again).
const KWALLET_DAEMONS: [(&str, &str); 4] =
    [("kwalletd6", "org.kde.kwalletd6"), ("kwalletd5", "org.kde.kwalletd5"), ("kwalletd", "org.kde.kwalletd"), ("ksecretd", "org.kde.ksecretd")];
const GNOME_UNITS: [&str; 2] = ["gnome-keyring-daemon.socket", "gnome-keyring-daemon.service"];

fn wait_gone(spec: &str, secs: u64) -> bool {
    let start = Instant::now();
    while !running_pids(spec).is_empty() {
        if start.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// gnome-keyring-daemon's argv for starting it again: never `--login` (it would wait for a
/// password on stdin), and `--start` when it doesn't say how it runs.
pub fn restart_argv(argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = argv.iter().filter(|a| !matches!(a.as_str(), "--login" | "-l")).cloned().collect();
    let modes = ["--start", "-s", "--foreground", "-f", "--daemonize", "-d", "--replace", "-r"];
    if !v.iter().skip(1).any(|a| modes.contains(&a.as_str())) {
        v.insert(1.min(v.len()), "--start".into());
    }
    v
}

/// The user's real session.
pub struct LiveDaemons;

impl KeyringDaemons for LiveDaemons {
    fn stop_keyring(&self, store: Store) -> Result<Restart, String> {
        match store {
            Store::KWallet => {
                let user = ctx().target_user.clone();
                let quit = ["kquitapp6", "kquitapp5"].into_iter().find(|q| have(q));
                for (name, service) in KWALLET_DAEMONS {
                    if super::process_running(name, Some(&user))
                        && let Some(q) = quit
                    {
                        user_ok(&[q, "--service", service]);
                    }
                }
                if !wait_gone("kwallet-daemons", 10) && !terminate_apps_with(&["kwallet-daemons".to_string()], false) {
                    return Err("ksecretd or kwalletd6 is still running".into());
                }
                Ok(Restart::default())
            }
            Store::Gnome => {
                let mut how = Restart::default();
                for unit in GNOME_UNITS {
                    if user_ok(&["systemctl", "--user", "is-active", "--quiet", unit]) {
                        how.units.push(unit.to_string());
                    }
                }
                if !how.units.is_empty() {
                    let mut args = vec!["systemctl", "--user", "stop"];
                    args.extend(GNOME_UNITS);
                    user_ok(&args);
                    wait_gone("gnome-keyring-daemon", 5);
                }
                let left = running_pids("gnome-keyring-daemon");
                if let Some(pid) = left.first() {
                    let argv = super::cmdline_argv(&fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default());
                    how.argv = (!argv.is_empty()).then(|| restart_argv(&argv));
                    if !terminate_apps_with(&["gnome-keyring-daemon".to_string()], false) {
                        // Bring back what systemd ran: the caller writes nothing
                        let _ = self.start_keyring(store, &Restart { units: how.units.clone(), argv: None });
                        return Err("gnome-keyring-daemon is still running".into());
                    }
                }
                Ok(how)
            }
        }
    }

    fn start_keyring(&self, store: Store, how: &Restart) -> Result<(), String> {
        // A daemon running now was started by D-Bus while the files went in (an app asked for
        // a secret): it may hold the old keyring and save it over the restored one when it
        // closes. SIGTERM ends it without that save; the next request starts a fresh one.
        let spec = if store == Store::KWallet { "kwallet-daemons" } else { "gnome-keyring-daemon" };
        let early = running_pids(spec);
        if !early.is_empty() {
            log_warn(&format!("{} started again during the restore; stopping it so it loads the restored files.", store.label()));
            for p in early {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGTERM);
            }
            if !wait_gone(spec, 7) {
                return Err("it did not stop; log out and back in before using the keyring".into());
            }
        }
        // KWallet: nothing to start, D-Bus activation starts ksecretd when an app asks for it
        for unit in &how.units {
            if !user_ok(&["systemctl", "--user", "start", unit]) {
                return Err(format!("systemctl --user start {unit} failed"));
            }
        }
        if let Some(argv) = &how.argv
            && running_pids("gnome-keyring-daemon").is_empty()
        {
            crate::logio::os::start_detached(argv, None, &[]).map_err(|e| format!("{}: {e}", argv.join(" ")))?;
        }
        Ok(())
    }
}

// --- Restore ------------------------------------------------------------------------------------

fn as_user() -> bool {
    let c = ctx();
    c.is_root && c.target_uid != 0
}

const FILE_MODES: &str = "--chmod=D0700,F0600";

/// The staged files of `app` that may go into the user's folder.
fn staged_files(app: &str, src: &Path) -> Vec<PathBuf> {
    let keep = if app == KWALLET { kwallet_file } else { gnome_file };
    regular_files(src).into_iter().filter(|(n, _)| keep(n)).map(|(n, _)| src.join(n)).collect()
}

/// What a restore of `app` needs from the caller.
pub struct RestoreCtx<'a> {
    pub staging: &'a Path,
    pub rollback: &'a Path,
    pub dry_run: bool,
    pub safety_backup: bool,
    pub manifest: &'a Value,
    pub daemons: &'a dyn KeyringDaemons,
}

pub fn restore(app: &str, r: &RestoreCtx) -> Result<()> {
    let kde = app == KWALLET;
    let (sub, dest, label) = if kde { ("kwalletd", kwallet_dir(), "KDE Wallet") } else { ("keyrings", gnome_dir(), "GNOME Keyring") };
    let src = r.staging.join(app).join(sub);
    let files = staged_files(app, &src);
    if files.is_empty() {
        log_warn(&format!("{label} folder missing in archive, skipping..."));
        return Ok(());
    }
    let names: Vec<String> = files.iter().filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
    if kde {
        for kwl in names.iter().filter_map(|n| n.strip_suffix(".kwl")) {
            let pbkdf2 = jq_keyrings(r.manifest, app).iter().any(|k| k["name"] == kwl && k["type"] == Kind::BlowfishPbkdf2.id());
            if pbkdf2 && !names.contains(&format!("{kwl}.salt")) {
                log_warn(&format!("The archive has no {kwl}.salt for the wallet '{kwl}': it can't be opened without it."));
            }
        }
    }
    if r.dry_run {
        log_info(&format!("  [Dry-run] Would stop {} while restoring", Store::of(app).label()));
        log_info(&format!("  [Dry-run] Would restore {} into {} (others there are kept)", names.join(", "), dest.display()));
        if kde {
            log_info(&format!("  [Dry-run] Would restore {} (Enabled=true, First Use=false)", kwalletrc_path().display()));
        }
        return Ok(());
    }
    let owner_before = secret_service_owner();
    let stopped = Stopped::stop(r.daemons, Store::of(app))?;

    // The snapshot: what a fresh install's first login may already have created (an empty
    // kdewallet or login.keyring), which is what gets replaced. Always made, empty included,
    // so an undo can put the folder back exactly as it was.
    if r.safety_backup {
        let snap = r.rollback.join(app).join(sub);
        fs::create_dir_all(&snap)?;
        fs::set_permissions(&snap, fs::Permissions::from_mode(0o700))?;
        if own_dir(&dest) {
            rsync_dir_as(false, &[FILE_MODES.to_string()], &dest, &snap)?;
        }
        if kde && kwalletrc_path().is_file() {
            rsync_as(false, &["-a"], &kwalletrc_path(), &format!("{}/", r.rollback.join(app).display()))?;
        }
    }

    crate::settings::user_mkdir_private(&dest).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dest_s = format!("{}/", dest.display());
    for f in &files {
        rsync_as(as_user(), &["-a", FILE_MODES], f, &dest_s)?;
    }
    if kde {
        let staged = r.staging.join(app).join("kwalletrc");
        let base = if staged.is_file() { fs::read_to_string(&staged)? } else { crate::settings::user_read(&kwalletrc_path()).unwrap_or_default() };
        crate::settings::user_write(&kwalletrc_path(), restored_kwalletrc(&base).as_bytes(), 0o600)?;
    }
    stopped.start();
    log_success(&format!("{label} restored ({}) with permissions 0600.", names.join(", ")));
    advise(app, r.manifest, owner_before.as_deref());
    Ok(())
}

fn jq_keyrings(manifest: &Value, app: &str) -> Vec<Value> {
    manifest["apps"][app]["keyrings"].as_array().cloned().unwrap_or_default()
}

/// What the user needs to know after a restore, for the situation at hand.
pub fn advise(app: &str, manifest: &Value, owner_before: Option<&str>) {
    let kde = app == KWALLET;
    let pam = pam_setup(&read_pam(), if kde { KWALLET_PAM } else { GNOME_PAM });
    let pkg = if kde { "kwallet-pam" } else { "gnome-keyring" };
    let installed = !have("pacman") || ok("pacman", &["-Q", pkg]);
    let keyrings = jq_keyrings(manifest, app);
    let kind_of = |name: &str| keyrings.iter().find(|k| k["name"] == name).map(|k| Kind::from_id(k["type"].as_str().unwrap_or("")));
    let (main, tool) = if kde { ("kdewallet", "KWalletManager") } else { ("login", "Seahorse (Passwords and Keys)") };
    match kind_of(main) {
        Some(Kind::BlowfishPbkdf2 | Kind::GnomeAes) => log_info(&format!(
            "Log out and back in. If your login password matches the old {} password, '{main}' unlocks automatically; otherwise you're asked for the old password once. Set it to your login password in {tool} to get the automatic unlock.",
            if kde { "wallet" } else { "keyring" }
        )),
        Some(Kind::Plaintext) => {
            log_warn(&format!("The '{main}' keyring has no password: anyone who can read your files can read its secrets. Set one in {tool}."))
        }
        Some(_) | None => log_info(&format!("Log out and back in, then open the restored {} with its old password.", if kde { "wallets" } else { "keyrings" })),
    }
    if !installed {
        log_warn(&format!(
            "{pkg} is not installed, so nothing unlocks the {} at login. Install it: sudo pacman -S {pkg}",
            if kde { "wallet" } else { "keyring" }
        ));
    } else if !pam.complete() {
        let module = if kde { KWALLET_PAM } else { GNOME_PAM };
        log_warn(&format!(
            "{module} isn't set up for login (no auth and session lines in /etc/pam.d: {}), so you'll be asked for the password after each login.",
            PAM_FILES.join(", ")
        ));
    }
    for k in keyrings.iter().filter(|k| k["type"] == Kind::Gpg.id()) {
        let name = k["name"].as_str().unwrap_or("");
        match k["gpg_key"].as_str() {
            Some(id) if have("gpg") && user_ok(&["gpg", "--batch", "--list-secret-keys", id]) => {
                log_info(&format!("The GPG wallet '{name}' needs key {id}: it is in your GPG keyring."))
            }
            Some(id) => log_warn(&format!(
                "The GPG wallet '{name}' needs the private key {id}, which isn't in your GPG keyring (gpg --list-secret-keys). Import it first."
            )),
            None => log_warn(&format!("The GPG wallet '{name}' needs its GPG private key in ~/.gnupg; kast doesn't back that up.")),
        }
    }
    let then = jq_str(manifest, &["apps", app, "secret_service_owner"], "");
    if let Some(now) = owner_before
        && !then.is_empty()
        && provider_label(&then) != provider_label(now)
    {
        log_warn(&format!(
            "When this backup was made, {} answered for the Secret Service; here it is {}. Only one can, so apps that use libsecret (Chromium and Electron apps outside KDE, e.g. on Hyprland, NetworkManager's Wi-Fi secrets, proton-drive) look in {} and won't see secrets stored in the other.",
            provider_label(&then),
            provider_label(now),
            provider_label(now)
        ));
    }
}

/// Put the snapshot of an import back (`kast undo`), with the daemon stopped like a restore.
pub fn undo(app: &str, snapshot: &Path, daemons: &dyn KeyringDaemons) -> Result<bool> {
    let kde = app == KWALLET;
    let (sub, dest) = if kde { ("kwalletd", kwallet_dir()) } else { ("keyrings", gnome_dir()) };
    let snap = snapshot.join(app).join(sub);
    if !snap.is_dir() {
        return Ok(false);
    }
    let stopped = Stopped::stop(daemons, Store::of(app))?;
    crate::settings::user_mkdir_private(&dest).map_err(|e| anyhow::anyhow!("{e}"))?;
    rsync_dir_as(as_user(), &["--delete".to_string(), FILE_MODES.to_string()], &snap, &dest)?;
    let rc = snapshot.join(app).join("kwalletrc");
    if kde && rc.is_file() {
        crate::settings::user_write(&kwalletrc_path(), &fs::read(&rc)?, 0o600)?;
    }
    stopped.start();
    Ok(true)
}

/// Archive layout check for `kast info`: the names and types, never the contents.
pub fn info_lines(m: &Value, app: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let folder = if app == KWALLET { "~/.local/share/kwalletd (+ ~/.config/kwalletrc)" } else { "~/.local/share/keyrings" };
    out.push(("Folder:      ".into(), folder.into()));
    for k in jq_keyrings(m, app) {
        let name = k["name"].as_str().unwrap_or("?");
        let kind = Kind::from_id(k["type"].as_str().unwrap_or(""));
        let mut extra = vec![kind.describe().to_string()];
        if let Some(size) = k["size"].as_u64() {
            extra.push(human(size));
        }
        if k["salt"] == false && kind == Kind::BlowfishPbkdf2 {
            extra.push("NO SALT FILE".into());
        }
        if let Some(id) = k["gpg_key"].as_str() {
            extra.push(format!("key {id}"));
        }
        out.push(("Keyring:     ".into(), format!("{name} ({})", extra.join(", "))));
    }
    let default = jq_str(m, &["apps", app, "default"], "");
    if !default.is_empty() {
        out.push(("Default:     ".into(), default));
    }
    if let Some(left) = m["apps"][app]["left_out"].as_array().filter(|l| !l.is_empty()) {
        out.push(("Left out:    ".into(), format!("{} (no password)", left.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))));
    }
    let owner = jq_str(m, &["apps", app, "secret_service_owner"], "");
    if !owner.is_empty() {
        out.push(("Secrets API: ".into(), provider_label(&owner)));
    }
    out
}

pub fn human(bytes: u64) -> String {
    if bytes < 1024 { format!("{bytes}B") } else { format!("{:.1}K", bytes as f64 / 1024.0) }
}

/// For `kast status`: the keyrings on this machine, who answers for the Secret Service, and
/// whether PAM unlocks them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyringStatus {
    pub kwallet: Vec<Found>,
    pub gnome: Vec<Found>,
    pub owner: Option<String>,
    /// `[KSecretD] Enabled` in kwalletrc: kwalletd6 uses ksecretd's wallet files
    pub kde_backend: String,
    pub pam_kwallet: bool,
    pub pam_gnome: bool,
}

pub fn status() -> KeyringStatus {
    let pam = read_pam();
    KeyringStatus {
        kwallet: scan_kwallet(&kwallet_dir()).keyrings,
        gnome: scan_gnome(&gnome_dir()).keyrings,
        owner: secret_service_owner(),
        kde_backend: kde_backend(&crate::settings::user_read(&kwalletrc_path()).unwrap_or_default()).into(),
        pam_kwallet: pam_setup(&pam, KWALLET_PAM).complete(),
        pam_gnome: pam_setup(&pam, GNOME_PAM).complete(),
    }
}

pub fn found_text(k: &Found) -> String {
    format!("{} ({}, {})", k.name, k.kind.describe(), human(k.size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kwl(cipher: u8, hash: u8) -> Vec<u8> {
        let mut v = KWALLET_MAGIC.to_vec();
        v.extend([0, 1, cipher, hash]);
        v.extend([0u8; 32]);
        v
    }

    #[test]
    fn kwallet_headers() {
        assert_eq!(kwallet_kind(&kwl(3, 2)), Kind::BlowfishPbkdf2);
        assert_eq!(kwallet_kind(&kwl(3, 0)), Kind::BlowfishLegacy);
        assert_eq!(kwallet_kind(&kwl(0, 0)), Kind::BlowfishLegacy);
        assert_eq!(kwallet_kind(&kwl(0, 2)), Kind::BlowfishLegacy);
        assert_eq!(kwallet_kind(&kwl(2, 0)), Kind::Gpg);
        for (c, h) in [(1, 0), (3, 1), (2, 2), (9, 9), (3, 3)] {
            assert_eq!(kwallet_kind(&kwl(c, h)), Kind::Unknown, "cipher {c} hash {h}");
        }
        let mut major = kwl(3, 2);
        major[12] = 1;
        assert_eq!(kwallet_kind(&major), Kind::Unknown, "unknown major version");
        assert_eq!(kwallet_kind(&kwl(3, 2)[..15]), Kind::Unknown, "truncated");
        assert_eq!(kwallet_kind(b""), Kind::Unknown);
        assert_eq!(kwallet_kind(b"garbage garbage garbage"), Kind::Unknown);
        for k in [Kind::BlowfishPbkdf2, Kind::BlowfishLegacy, Kind::Gpg, Kind::GnomeAes, Kind::Plaintext, Kind::Unknown] {
            assert_eq!(Kind::from_id(k.id()), k);
        }
    }

    fn gnome_binary(name: Option<&str>, iterations: u32) -> Vec<u8> {
        let mut v = GNOME_MAGIC.to_vec();
        v.extend([0, 0, 0, 0]);
        match name {
            Some(n) => {
                v.extend((n.len() as u32).to_be_bytes());
                v.extend(n.as_bytes());
            }
            None => v.extend(u32::MAX.to_be_bytes()),
        }
        v.extend([0u8; 16]); // times
        v.extend(0u32.to_be_bytes()); // flags
        v.extend(0u32.to_be_bytes()); // lock timeout
        v.extend(iterations.to_be_bytes());
        v.extend([7u8; 8]); // salt
        v
    }

    #[test]
    fn gnome_headers() {
        assert_eq!(gnome_kind(&gnome_binary(Some("Login"), 1975)), (Kind::GnomeAes, Some(1975)));
        assert_eq!(gnome_kind(&gnome_binary(None, 3000)), (Kind::GnomeAes, Some(3000)));
        let mut odd = gnome_binary(Some("x"), 1);
        odd[18] = 1;
        assert_eq!(gnome_kind(&odd).0, Kind::Unknown, "unknown crypto");
        assert_eq!(gnome_kind(GNOME_MAGIC), (Kind::Unknown, None), "truncated header");
        let short = gnome_binary(Some("Login"), 5);
        assert_eq!(gnome_kind(&short[..30]), (Kind::GnomeAes, None), "truncated after the header: no iterations");
        // A huge name length can't overflow or read past the end
        let mut huge = gnome_binary(Some("x"), 5);
        huge[20..24].copy_from_slice(&0xffff_fff0u32.to_be_bytes());
        assert_eq!(gnome_kind(&huge), (Kind::GnomeAes, None));
        assert_eq!(gnome_kind(b"[keyring]\ndisplay-name=Login\n").0, Kind::Plaintext);
        assert_eq!(gnome_kind(b"\n# comment\n[keyring]\n").0, Kind::Plaintext);
        assert_eq!(gnome_kind(b"[other]\n[keyring]\n").0, Kind::Unknown);
        assert_eq!(gnome_kind(b"\xff\xfe garbage").0, Kind::Unknown);
        assert_eq!(gnome_kind(b"").0, Kind::Unknown);
    }

    #[test]
    fn gpg_key_ids() {
        let mut v = kwl(2, 0);
        v.truncate(16);
        // Old-format tag 1, one length byte, version 3, key ID
        v.extend([0x84, 12, 3, 0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 1]);
        assert_eq!(gpg_key_id(&v).as_deref(), Some("DEADBEEF01020304"));
        let mut n = kwl(2, 0);
        n.truncate(16);
        n.extend([0xC1, 12, 3, 1, 2, 3, 4, 5, 6, 7, 8, 1]);
        assert_eq!(gpg_key_id(&n).as_deref(), Some("0102030405060708"));
        assert_eq!(gpg_key_id(&kwl(2, 0)), None, "zeros are no packet");
        assert_eq!(gpg_key_id(&v[..20]), None, "truncated");
        assert_eq!(gpg_key_id(b"KWALLET"), None);
    }

    #[test]
    fn kwalletrc_after_a_restore() {
        let rc = "[Wallet]\nDefault Wallet=kdewallet\nFirst Use=true\nEnabled=false\n\n[org.freedesktop.secrets]\napiEnabled=true\n";
        let out = restored_kwalletrc(rc);
        assert_eq!(out, "[Wallet]\nDefault Wallet=kdewallet\nFirst Use=false\nEnabled=true\n\n[org.freedesktop.secrets]\napiEnabled=true\n");
        assert_eq!(restored_kwalletrc(""), "[Wallet]\nEnabled=true\nFirst Use=false\n");
        let other = restored_kwalletrc("[KSecretD]\nEnabled=true\n");
        assert_eq!(ini_get(&other, "Wallet", "First Use").as_deref(), Some("false"));
        assert_eq!(ini_get(&other, "KSecretD", "Enabled").as_deref(), Some("true"));
        let partial = restored_kwalletrc("[Wallet]\nUse One Wallet=true\n\n[Migration]\nx=1\n");
        assert_eq!(partial, "[Wallet]\nUse One Wallet=true\nEnabled=true\nFirst Use=false\n\n[Migration]\nx=1\n");
        assert_eq!(kde_backend(""), "ksecretd");
        assert_eq!(kde_backend("[KSecretD]\nEnabled=false\n"), "secret-service");
        assert_eq!(ini_get("[Wallet]\nFirst Use = true\n", "Wallet", "First Use").as_deref(), Some("true"));
    }

    #[test]
    fn bus_owner_and_pam() {
        let out = "PID=1108\nPPID=1098\nComm=gnome-keyring-d\nExe=/usr/bin/gnome-keyring-daemon\n";
        assert_eq!(parse_bus_owner(out).as_deref(), Some("gnome-keyring-daemon"));
        assert_eq!(parse_bus_owner("Comm=ksecretd\n").as_deref(), Some("ksecretd"));
        assert_eq!(parse_bus_owner(""), None);
        assert_eq!(provider_label("ksecretd"), provider_label("kwalletd6"));
        assert_ne!(provider_label("ksecretd"), provider_label("gnome-keyring-daemon"));

        let f = |n: &str, t: &str| (n.to_string(), t.to_string());
        let files = [
            f(
                "plasmalogin",
                "auth include system-login\n-auth optional pam_gnome_keyring.so\n-auth optional pam_kwallet5.so\n-session optional pam_gnome_keyring.so auto_start\n-session optional pam_kwallet5.so auto_start\n",
            ),
            f("login", "#session optional pam_gnome_keyring.so auto_start\npassword optional pam_gnome_keyring.so use_authtok\n"),
        ];
        let k = pam_setup(&files, KWALLET_PAM);
        assert!(k.complete());
        assert_eq!(k.auth, ["plasmalogin"]);
        let only_auth = [f("sddm", "auth optional pam_kwallet5.so\n")];
        assert!(!pam_setup(&only_auth, KWALLET_PAM).complete(), "auth without session doesn't unlock");
        let g = pam_setup(&files[1..], GNOME_PAM);
        assert!(!g.complete(), "comments and password lines don't count: {g:?}");
    }

    #[test]
    fn gnome_keyring_comes_back_as_it_ran() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(restart_argv(&v(&["/usr/bin/gnome-keyring-daemon", "--daemonize", "--login"])), v(&["/usr/bin/gnome-keyring-daemon", "--daemonize"]));
        assert_eq!(
            restart_argv(&v(&["gnome-keyring-daemon", "--components=secrets"])),
            v(&["gnome-keyring-daemon", "--start", "--components=secrets"]),
            "Hyprland's exec-once without a mode"
        );
        assert_eq!(
            restart_argv(&v(&["gnome-keyring-daemon", "--start", "--components=secrets"])),
            v(&["gnome-keyring-daemon", "--start", "--components=secrets"])
        );
    }

    #[test]
    fn file_filters() {
        assert!(kwallet_file("kdewallet.kwl") && kwallet_file("kdewallet.salt") && kwallet_file("kdewallet_attributes.json"));
        assert!(!kwallet_file("kdewallet.kwl~") && !kwallet_file(".kdewallet.kwl.tmp") && !kwallet_file("notes.txt"));
        assert!(gnome_file("login.keyring") && gnome_file("default") && gnome_file("user.keystore"));
        assert!(!gnome_file("login.keyring~") && !gnome_file(".login.keyring.ABC123") && !gnome_file("x.lock"));
    }
}
