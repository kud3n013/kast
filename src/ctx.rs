//! Process-wide context: who we are acting for, where we log, and the temporary state
//! that must be undone on every exit path (staging dirs, the yay sudoers rule, the TUI).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nix::unistd::{Gid, Group, Uid, User};

pub struct Ctx {
    pub is_root: bool,
    pub target_user: String,
    pub target_uid: u32,
    pub target_gid: u32,
    pub target_home: PathBuf,
    pub sudo_user: Option<String>,
    pub log_file: PathBuf,
    /// The vault works on the target user's folders through these. HOME/XDG_* are
    /// deliberately never exported to the user's home: everything run as root (pacman
    /// hooks, package scriptlets, howdy...) would otherwise write root-owned files there.
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
    pub cache_home: PathBuf,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

pub fn ctx() -> &'static Ctx {
    CTX.get().expect("context not initialized")
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// First human account (1000 <= UID < 65534) in /etc/passwd.
fn first_human_user() -> Option<String> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        let uid: u32 = f.get(2)?.parse().ok()?;
        ((1000..65534).contains(&uid)).then(|| f[0].to_string())
    })
}

pub fn init() {
    let is_root = Uid::effective().is_root();
    let sudo_user = env_nonempty("SUDO_USER");

    let mut target: Option<String> = if !is_root {
        // Unprivileged vault commands always act on the invoking user
        User::from_uid(Uid::effective()).ok().flatten().map(|u| u.name)
    } else if let Some(su) = sudo_user.clone().filter(|u| u != "root") {
        Some(su)
    } else if let Some(pk) = env_nonempty("PKEXEC_UID") {
        pk.parse::<u32>().ok().and_then(|uid| User::from_uid(Uid::from_raw(uid)).ok().flatten()).map(|u| u.name)
    } else {
        None
    };
    if target.as_deref().is_none_or(|t| t == "root") {
        target = first_human_user();
    }
    if target.as_deref().is_none_or(|t| t == "root") {
        target = env_nonempty("LOGNAME").or_else(|| env_nonempty("USER"));
    }
    let target_user = target.unwrap_or_else(|| "root".into());

    let pw = User::from_name(&target_user).ok().flatten();
    let target_uid = pw.as_ref().map(|u| u.uid.as_raw()).unwrap_or(1000);
    let target_gid = pw.as_ref().map(|u| u.gid.as_raw()).unwrap_or(1000);
    let target_home = pw.as_ref().map(|u| u.dir.clone()).filter(|d| d.is_dir()).unwrap_or_else(|| PathBuf::from(format!("/home/{target_user}")));

    let log_file = if is_root { PathBuf::from("/var/log/kast.log") } else { target_home.join(".local/state/kast.log") };
    if let Some(dir) = log_file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = OpenOptions::new().create(true).append(true).open(&log_file);
    let _ = fs::set_permissions(&log_file, fs::Permissions::from_mode(0o644));

    let xdg = |key: &str, default: &str| -> PathBuf {
        if is_root && target_user != "root" {
            target_home.join(default)
        } else {
            env_nonempty(key).map(PathBuf::from).unwrap_or_else(|| target_home.join(default))
        }
    };
    let config_home = xdg("XDG_CONFIG_HOME", ".config");
    let data_home = xdg("XDG_DATA_HOME", ".local/share");
    let state_home = xdg("XDG_STATE_HOME", ".local/state");
    let cache_home = xdg("XDG_CACHE_HOME", ".cache");

    let _ = CTX.set(Ctx { is_root, target_user, target_uid, target_gid, target_home, sudo_user, log_file, config_home, data_home, state_home, cache_home });
}

/// The vault's staging folder is one per process, and every job's end wipes it: tests that
/// run a backup and tests that run jobs take turns.
#[cfg(test)]
pub fn staging_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A context for tests that need one: the current user, with a throwaway home (kept for the
/// whole test run) for config, state, cache and the log.
#[cfg(test)]
pub fn init_for_tests() -> &'static Ctx {
    CTX.get_or_init(|| {
        let home = tempfile::tempdir().unwrap().keep();
        let me = User::from_uid(Uid::effective()).ok().flatten();
        Ctx {
            is_root: false,
            target_user: me.as_ref().map(|u| u.name.clone()).unwrap_or_else(|| "tester".into()),
            target_uid: Uid::effective().as_raw(),
            target_gid: Gid::effective().as_raw(),
            target_home: home.clone(),
            sudo_user: None,
            log_file: home.join("kast.log"),
            config_home: home.join(".config"),
            data_home: home.join(".local/share"),
            state_home: home.join(".local/state"),
            cache_home: home.join(".cache"),
        }
    })
}

pub fn now_str(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

pub fn log(level: &str, msg: &str) {
    log_file_only(level, msg);
    crate::report::file_logged(level, msg);
}

/// Append to the log file without telling the reporter (it already showed the message).
pub fn log_file_only(level: &str, msg: &str) {
    let Some(c) = CTX.get() else { return };
    if let Ok(mut f) = OpenOptions::new().append(true).open(&c.log_file) {
        let _ = writeln!(f, "[{}] [{level}] {msg}", now_str("%Y-%m-%d %H:%M:%S"));
    }
}

/// Append raw bytes (a child's output) to the log file.
pub fn log_raw(bytes: &[u8]) {
    let Some(c) = CTX.get() else { return };
    if let Ok(mut f) = OpenOptions::new().append(true).open(&c.log_file) {
        let _ = f.write_all(bytes);
    }
}

/// Open the log file for a child's stdout/stderr.
pub fn log_stdio() -> Stdio {
    OpenOptions::new().create(true).append(true).open(&ctx().log_file).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}

// --- Small process helpers -------------------------------------------------------

pub fn have(tool: &str) -> bool {
    which(tool).is_some()
}

pub fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(tool)).find(|p| fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// Run quietly, true on exit status 0.
pub fn ok(prog: &str, args: &[&str]) -> bool {
    Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Run with output appended to the log, true on exit status 0.
pub fn ok_logged(prog: &str, args: &[&str]) -> bool {
    Command::new(prog).args(args).stdin(Stdio::null()).stdout(log_stdio()).stderr(log_stdio()).status().map(|s| s.success()).unwrap_or(false)
}

/// Run and capture stdout (stderr discarded); None if it failed to start or exited non-zero.
pub fn output(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like `ok`, but through `sudo -n` when not root (for /etc/howdy access). Never prompts:
/// vault commands ask for the password up front (`vault::sudo_ready`).
pub fn ok_priv(prog: &str, args: &[&str]) -> bool {
    if ctx().is_root {
        ok(prog, args)
    } else {
        let mut v = vec!["-n", prog];
        v.extend_from_slice(args);
        ok("sudo", &v)
    }
}

/// The target user's session: where their desktop programs find the display and the bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserEnv {
    pub user: String,
    pub uid: u32,
    pub home: String,
    pub wayland: String,
    pub display: String,
    pub xauthority: String,
    pub dbus: String,
}

impl UserEnv {
    pub fn runtime_dir(&self) -> String {
        format!("/run/user/{}", self.uid)
    }

    /// `KEY=value` pairs for a program run as the user (as `run_as_user` passes them to `env`).
    pub fn vars(&self) -> Vec<(String, String)> {
        let home = &self.home;
        let mut v: Vec<(String, String)> = [
            ("HOME", home.clone()),
            ("USER", self.user.clone()),
            ("LOGNAME", self.user.clone()),
            ("XDG_CONFIG_HOME", format!("{home}/.config")),
            ("XDG_DATA_HOME", format!("{home}/.local/share")),
            ("XDG_STATE_HOME", format!("{home}/.local/state")),
            ("XDG_CACHE_HOME", format!("{home}/.cache")),
            ("XDG_RUNTIME_DIR", self.runtime_dir()),
            ("WAYLAND_DISPLAY", self.wayland.clone()),
            ("DISPLAY", self.display.clone()),
            ("DBUS_SESSION_BUS_ADDRESS", self.dbus.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // An empty XAUTHORITY would hide ~/.Xauthority from X clients: only pass a real one
        if !self.xauthority.is_empty() {
            v.push(("XAUTHORITY".into(), self.xauthority.clone()));
        }
        v
    }
}

/// The Wayland socket to use: `$WAYLAND_DISPLAY`, else `wayland-1`, `wayland-0` or any
/// other `wayland-*` socket among `sockets` (the names in /run/user/<uid>).
pub fn wayland_socket(env: &str, sockets: &[String]) -> String {
    if !env.is_empty() {
        return env.to_string();
    }
    let is_socket = |n: &&String| n.starts_with("wayland-") && !n.ends_with(".lock");
    ["wayland-1", "wayland-0"]
        .into_iter()
        .find(|w| sockets.iter().any(|s| s == w))
        .map(String::from)
        .or_else(|| sockets.iter().filter(is_socket).min().cloned())
        .unwrap_or_default()
}

/// The target user's session, resolved from our environment and their runtime dir.
pub fn user_env() -> UserEnv {
    let c = ctx();
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let runtime = format!("/run/user/{}", c.target_uid);
    let sockets: Vec<String> = fs::read_dir(&runtime).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    let mut dbus = var("DBUS_SESSION_BUS_ADDRESS");
    if dbus.is_empty() && Path::new(&format!("{runtime}/bus")).exists() {
        dbus = format!("unix:path={runtime}/bus");
    }
    UserEnv {
        user: c.target_user.clone(),
        uid: c.target_uid,
        home: c.target_home.display().to_string(),
        wayland: wayland_socket(&var("WAYLAND_DISPLAY"), &sockets),
        display: var("DISPLAY"),
        xauthority: var("XAUTHORITY"),
        dbus,
    }
}

/// `sudo -u <target> env HOME=... XDG_*=... <args>` so user programs see the user's
/// environment (only these children get it).
pub fn run_as_user(args: &[&str]) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.args(["-u", &ctx().target_user, "env"]).args(user_env().vars().into_iter().map(|(k, v)| format!("{k}={v}"))).args(args);
    cmd
}

pub fn chown_path(path: &Path, uid: u32, gid: u32) -> bool {
    nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).is_ok()
}

/// Change the owner of a symlink itself (`chown -h`).
pub fn lchown_path(path: &Path, uid: u32, gid: u32) -> bool {
    nix::unistd::fchownat(nix::fcntl::AT_FDCWD, path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok()
}

/// `chown -R uid:gid path`, errors ignored.
pub fn chown_r(path: &Path, owner: &str) {
    let _ = ok("chown", &["-R", owner, &path.to_string_lossy()]);
}

/// `mkdir -p` that hands every directory it creates to the target user when running as
/// root, so e.g. a missing ~/.local or ~/.cache never ends up root-owned.
pub fn mkdir_user(dir: &Path) -> std::io::Result<()> {
    let mut created = Vec::new();
    let mut p = dir.to_path_buf();
    while !p.as_os_str().is_empty() && p != Path::new("/") && p != Path::new(".") && !p.exists() && p.symlink_metadata().is_err() {
        created.push(p.clone());
        match p.parent() {
            Some(parent) => p = parent.to_path_buf(),
            None => break,
        }
    }
    fs::create_dir_all(dir)?;
    let c = ctx();
    if c.is_root && c.target_uid != 0 {
        for d in &created {
            chown_path(d, c.target_uid, c.target_gid);
        }
    }
    Ok(())
}

/// Replace a system file the way `sed -i` does: write a sibling temp file, give it the
/// original's mode and owner, then rename it over. An interrupted run or a full disk never
/// leaves a truncated /etc/fstab or PAM file behind.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let dir = path.parent().unwrap_or(Path::new("/"));
    let Ok(mut tmp) = tempfile::NamedTempFile::new_in(dir) else {
        // Directory not writable (only the file is, e.g. bind-mounted): plain in-place write
        return fs::write(path, data);
    };
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    let (mode, owner) = match fs::metadata(path) {
        Ok(m) => (m.mode() & 0o7777, Some((m.uid(), m.gid()))),
        Err(_) => (0o644, None),
    };
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    if let Some((uid, gid)) = owner {
        chown_path(tmp.path(), uid, gid);
    }
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        // A bind-mounted file (containers, sandboxes) can't be renamed over
        Err(e) if matches!(e.error.raw_os_error(), Some(libc::EBUSY) | Some(libc::EXDEV)) => fs::write(path, data),
        Err(e) => Err(e.error),
    }
}

pub fn is_writable(p: &Path) -> bool {
    nix::unistd::access(p, nix::unistd::AccessFlags::W_OK).is_ok()
}

pub fn user_in_group(user: &str, group: &str) -> bool {
    let Some(pw) = User::from_name(user).ok().flatten() else { return false };
    if let Ok(Some(g)) = Group::from_gid(pw.gid)
        && g.name == group
    {
        return true;
    }
    Group::from_name(group).ok().flatten().is_some_and(|g| g.mem.iter().any(|m| m == user))
}

// --- Issues collected during a run (listed on the final summary screen) -----------

static ISSUES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn add_issue(msg: impl Into<String>) {
    let msg = msg.into();
    log("WARN", &format!("Issue recorded: {msg}"));
    ISSUES.lock().unwrap().push(msg);
}

pub fn take_issues() -> Vec<String> {
    std::mem::take(&mut *ISSUES.lock().unwrap())
}

pub fn clear_issues() {
    ISSUES.lock().unwrap().clear();
}

// --- Vault cancel flag: lets callers tell "canceled" from "finished" ----------------

static CANCELED: AtomicBool = AtomicBool::new(false);

pub fn set_canceled(v: bool) {
    CANCELED.store(v, Ordering::SeqCst);
}

pub fn canceled() -> bool {
    CANCELED.load(Ordering::SeqCst)
}

// --- pacman lock left behind by a stopped pacman --------------------------------------

pub const PACMAN_LOCK: &str = "/var/lib/pacman/db.lck";

/// Set while a pacman that kast started (directly or through yay) is running or was
/// interrupted, and the lock was free when it started: a lock found then is that pacman's.
/// Cleared when the command returns normally, so a later cancel never touches the lock of
/// another tool (Discover / PackageKit, pamac hold it without a `pacman` process).
static LOCK_WAS_FREE: AtomicBool = AtomicBool::new(false);

/// Call right before kast starts a command that runs pacman.
pub fn note_pacman_start() {
    LOCK_WAS_FREE.store(!Path::new(PACMAN_LOCK).exists(), Ordering::SeqCst);
}

/// Call when that command returned (it wasn't interrupted by a cancel).
pub fn note_pacman_done() {
    LOCK_WAS_FREE.store(false, Ordering::SeqCst);
}

/// Read this first in a signal handler: the main thread may clear it once pacman exits.
pub fn pacman_interrupted() -> bool {
    LOCK_WAS_FREE.load(Ordering::SeqCst)
}

/// A process named `pacman` is running (any user, any terminal).
fn pacman_running() -> bool {
    fs::read_dir("/proc").is_ok_and(|rd| rd.flatten().any(|e| fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim_end() == "pacman")))
}

/// The pacman lock is still there although the pacman kast started is gone. Waits up to
/// `wait` for a pacman that was just stopped to exit. Never true for a lock that existed
/// before kast started pacman, or while any pacman runs, so another pacman's lock is safe.
pub fn stale_pacman_lock(interrupted: bool, wait: std::time::Duration) -> bool {
    lock_is_stale(Path::new(PACMAN_LOCK), interrupted, wait, pacman_running)
}

fn lock_is_stale(lock: &Path, was_free: bool, wait: std::time::Duration, running: impl Fn() -> bool) -> bool {
    if !was_free || !lock.exists() {
        return false;
    }
    let end = std::time::Instant::now() + wait;
    while running() {
        if std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    lock.exists()
}

/// Remove the lock if it is (still) stale. True when it was removed.
pub fn remove_stale_pacman_lock() -> bool {
    let removed = stale_pacman_lock(pacman_interrupted(), std::time::Duration::ZERO) && fs::remove_file(PACMAN_LOCK).is_ok();
    note_pacman_done();
    removed
}

// --- Temporary state undone on exit -------------------------------------------------

static STAGING: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_staging(p: PathBuf) {
    *STAGING.lock().unwrap() = Some(p);
}

pub fn cleanup_vault_staging() {
    if let Some(p) = STAGING.lock().unwrap().take() {
        let _ = fs::remove_dir_all(p);
    }
}

pub const YAY_SUDOERS_FILE: &str = "/etc/sudoers.d/zz-kast-yay";
/// The rule's name before the rename to kast. A crash in an older kas build could have left
/// it behind, so revoking removes it too and an old grant never lingers.
const LEGACY_YAY_SUDOERS_FILE: &str = "/etc/sudoers.d/zz-kas-yay";

/// yay builds as the target user and then runs `sudo pacman` itself. Behind the dashboard
/// that password prompt is invisible and hangs forever, so for the duration of a yay call
/// let the (already sudo-capable) user run pacman without a password.
pub fn grant_yay_sudo() -> bool {
    let c = ctx();
    if !c.is_root {
        return false;
    }
    // Never hand pacman to a user who couldn't already sudo
    if c.sudo_user.as_deref() != Some(c.target_user.as_str()) && !user_in_group(&c.target_user, "wheel") {
        log("WARN", &format!("{} is not a sudo user; skipping yay fallback.", c.target_user));
        return false;
    }
    let Some(pacman) = which("pacman") else { return false };
    let Ok(mut tmp) = tempfile::NamedTempFile::new() else { return false };
    let rule = format!("{} ALL=(root) NOPASSWD: {}\n", c.target_user, pacman.display());
    let tmp_path = tmp.path().to_string_lossy().into_owned();
    if tmp.write_all(rule.as_bytes()).is_ok()
        && tmp.flush().is_ok()
        && ok("visudo", &["-cf", &tmp_path])
        && ok("install", &["-m", "0440", "-o", "root", "-g", "root", &tmp_path, YAY_SUDOERS_FILE])
    {
        return true;
    }
    log("WARN", "Could not install temporary sudoers rule; skipping yay fallback.");
    false
}

pub fn revoke_yay_sudo() {
    remove_existing(&[Path::new(YAY_SUDOERS_FILE), Path::new(LEGACY_YAY_SUDOERS_FILE)]);
}

fn remove_existing(paths: &[&Path]) {
    for p in paths {
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Private folders of the user's that hold backup data for a moment (Proton Drive staging):
/// removed by whoever made them, or here on the way out.
static PRIVATE_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn track_private_dir(p: &Path) {
    PRIVATE_DIRS.lock().unwrap().push(p.to_path_buf());
}

pub fn untrack_private_dir(p: &Path) {
    PRIVATE_DIRS.lock().unwrap().retain(|d| d != p);
}

pub fn cleanup_private_dirs() {
    let dirs = std::mem::take(&mut *PRIVATE_DIRS.lock().unwrap());
    for d in dirs {
        let _ = crate::settings::user_remove_dir(&d);
    }
}

/// On the way out of kast (exit, signal, panic): also stop a sign-in still waiting and wipe
/// the Proton Drive staging folders.
pub fn cleanup_on_exit() {
    crate::proton::kill_login();
    cleanup_private_dirs();
    cleanup_temp_state();
}

/// After a job: what that job may have left behind.
pub fn cleanup_temp_state() {
    cleanup_vault_staging();
    revoke_yay_sudo();
    crate::partition::apply::cleanup_probes();
}

/// Error already shown to the user; callers only need the failure status.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation failed")
    }
}

impl std::error::Error for Reported {}

pub fn reported() -> anyhow::Error {
    anyhow::Error::new(Reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_socket_prefers_env_then_known_names() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(wayland_socket("wayland-7", &s(&["wayland-0"])), "wayland-7");
        assert_eq!(wayland_socket("", &s(&["bus", "wayland-0", "wayland-0.lock", "wayland-1"])), "wayland-1");
        assert_eq!(wayland_socket("", &s(&["wayland-2.lock", "wayland-2"])), "wayland-2");
        assert_eq!(wayland_socket("", &s(&["bus", "pipewire-0"])), "");
    }

    #[test]
    fn user_env_vars_skip_empty_xauthority() {
        let mut u = UserEnv { user: "p3ter".into(), uid: 1000, home: "/home/p3ter".into(), ..Default::default() };
        let keys = |u: &UserEnv| u.vars().into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(keys(&u).len(), 11);
        assert!(u.vars().contains(&("XDG_RUNTIME_DIR".into(), "/run/user/1000".into())));
        u.xauthority = "/run/user/1000/xauth_x".into();
        assert_eq!(keys(&u).last().unwrap(), "XAUTHORITY");
    }

    #[test]
    fn atomic_write_keeps_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pacman.conf");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn stale_lock_only_when_ours_and_no_pacman() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("db.lck");
        // No lock: nothing to do
        assert!(!lock_is_stale(&lock, true, Duration::ZERO, || false));
        fs::write(&lock, "").unwrap();
        assert!(lock_is_stale(&lock, true, Duration::ZERO, || false));
        // The lock was already there before kast started pacman: someone else's
        assert!(!lock_is_stale(&lock, false, Duration::ZERO, || false));
        // A pacman still runs (another terminal, or ours not gone yet)
        assert!(!lock_is_stale(&lock, true, Duration::from_millis(250), || true));
        // Ours exits while we wait
        let n = std::sync::atomic::AtomicU32::new(0);
        assert!(lock_is_stale(&lock, true, Duration::from_secs(2), || n.fetch_add(1, Ordering::SeqCst) < 2));
    }

    #[test]
    fn revoke_removes_current_and_legacy_rules() {
        assert_eq!(YAY_SUDOERS_FILE, "/etc/sudoers.d/zz-kast-yay");
        assert_eq!(LEGACY_YAY_SUDOERS_FILE, "/etc/sudoers.d/zz-kas-yay");
        let dir = tempfile::tempdir().unwrap();
        let (cur, legacy, missing) = (dir.path().join("zz-kast-yay"), dir.path().join("zz-kas-yay"), dir.path().join("none"));
        fs::write(&cur, "").unwrap();
        fs::write(&legacy, "").unwrap();
        remove_existing(&[&cur, &legacy, &missing]);
        assert!(!cur.exists() && !legacy.exists());
    }
}
