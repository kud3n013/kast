//! App Vault: backup, export, import & status of app data, KDE Plasma, Hyprland and
//! Howdy face models.

pub mod archive;
pub mod backup;
pub mod fix;
pub mod import;
pub mod info;
pub mod status;

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::ctx::{self, ctx, have, ok, ok_logged, output};
use crate::report::{self, Level};

pub fn log_info(msg: &str) {
    report::log(Level::Info, msg);
}
pub fn log_success(msg: &str) {
    report::log(Level::Success, msg);
}
pub fn log_warn(msg: &str) {
    report::log(Level::Warn, msg);
}
pub fn log_error(msg: &str) {
    report::log(Level::Error, msg);
}
pub fn log_step(msg: &str) {
    report::log(Level::Step, msg);
}

/// Archives are read and written natively; only rsync is still needed. Installed on the
/// fly when running as root so the vault works on a fresh system.
pub fn require_vault_tools() -> Result<()> {
    if !have("rsync") && ctx().is_root && have("pacman") {
        log_info("Installing missing vault dependencies: rsync");
        ok_logged("pacman", &["-S", "--needed", "--noconfirm", "rsync"]);
    }
    if !have("rsync") {
        log_error("Required tool(s) not installed: rsync");
        return Err(ctx::reported());
    }
    Ok(())
}

pub const ALL_APPS: [&str; 10] = ["beeper", "zalo", "vesktop", "zen", "obsidian", "fcitx5", "antigravity", "howdy", "kde", "hypr"];

pub fn normalize_app(app: &str) -> String {
    match app {
        "beepertexts" => "beeper",
        "zalodata" => "zalo",
        "plasma" | "kde-plasma" => "kde",
        "hyprland" => "hypr",
        a => a,
    }
    .to_string()
}

pub fn default_backup_dir() -> PathBuf {
    ctx().data_home.join("app-vault-sync/backups")
}

pub fn safety_backup_dir() -> PathBuf {
    ctx().state_home.join("app-vault-sync/safety-rollbacks")
}

fn first_dir(base: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| base.join(n)).find(|p| p.is_dir())
}

/// Config directory of an app, if present.
pub fn find_config(app: &str) -> Option<PathBuf> {
    let c = ctx();
    let cfg = &c.config_home;
    match app {
        "beeper" => first_dir(cfg, &["BeeperTexts", "Beeper", "beeper"]),
        "zalo" => first_dir(cfg, &["ZaloData", "Zalo"]),
        "vesktop" | "obsidian" | "fcitx5" | "hypr" => first_dir(cfg, &[app]),
        "zen" => first_dir(cfg, &["zen"]).or_else(|| first_dir(&c.target_home, &[".zen"])),
        "antigravity" => first_dir(cfg, &["Antigravity", "antigravity"]),
        _ => None,
    }
}

pub fn find_zalo_flags() -> Option<PathBuf> {
    Some(ctx().config_home.join("zalo-flags.conf")).filter(|p| p.is_file())
}

pub const HOWDY_MODELS_DIR: &str = "/etc/howdy/models";

pub fn howdy_model_path(user: &str) -> PathBuf {
    PathBuf::from(format!("{HOWDY_MODELS_DIR}/{user}.dat"))
}

/// Root access for /etc/howdy/models when kast runs as a normal user. sudo asks for the
/// password here, once, before anything starts, and the credentials are kept fresh
/// afterwards, so no prompt shows up part-way through a backup or restore. False when sudo
/// would need a password and there's no terminal to type it in (the answer is remembered).
pub fn sudo_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    if ctx().is_root {
        return true;
    }
    *READY.get_or_init(|| {
        if !have("sudo") {
            return false;
        }
        if !ok("sudo", &["-n", "true"]) {
            if !std::io::stdin().is_terminal() || report::owns_terminal() {
                return false;
            }
            log_info("The Howdy face model in /etc/howdy/models needs root access.");
            if !std::process::Command::new("sudo").arg("-v").status().is_ok_and(|s| s.success()) {
                return false;
            }
        }
        // Refresh the timestamp so a long backup doesn't outlive it
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(60));
                if !ok("sudo", &["-n", "-v"]) {
                    break;
                }
            }
        });
        true
    })
}

/// Howdy is set up on this machine (its models folder is visible even without root).
pub fn howdy_installed() -> bool {
    have("howdy") || Path::new(HOWDY_MODELS_DIR).is_dir()
}

/// Warning for a face model that can't be read or written without a password prompt.
pub fn warn_sudo_needed(what: &str) {
    log_warn(&format!("Skipping {what}: /etc/howdy/models needs root and sudo would ask for a password here. Run the command with sudo to include it."));
}

/// /etc/howdy/models is root-only; as a normal user, ask sudo without prompting.
pub fn priv_test(flag: &str, path: &Path) -> bool {
    let exists = if flag == "-d" { path.is_dir() } else { path.is_file() };
    exists || (!ctx().is_root && have("sudo") && ok("sudo", &["-n", "test", flag, &path.to_string_lossy()]))
}

pub fn find_howdy_model(user: &str) -> Option<PathBuf> {
    let p = howdy_model_path(user);
    priv_test("-f", &p).then_some(p)
}

// --- KDE Plasma settings (XDG config + share) ------------------------------------------
// Curated from Konsave conf_kde.yaml, the ArchWiki and KDE Plasma 6 docs. Leaves out caches,
// Akonadi/Baloo databases and non-KDE application configs.

pub const KDE_CONFIG_ENTRIES: &[&str] = &[
    "gtk-2.0",
    "gtk-3.0",
    "gtk-4.0",
    "gtkrc",
    "gtkrc-2.0",
    "xsettingsd",
    "kate",
    "Kvantum",
    "latte",
    "fontconfig",
    "dolphinrc",
    "katerc",
    "konsolerc",
    "konsolesshconfig",
    "kcminputrc",
    "kdeglobals",
    "kglobalshortcutsrc",
    "kxkbrc",
    "touchpadrc",
    "klipperrc",
    "krunnerrc",
    "kscreenlockerrc",
    "ksmserverrc",
    "kwinrc",
    "kwinrulesrc",
    "kwinoutputconfig.json",
    "plasma-org.kde.plasma.desktop-appletsrc",
    "plasmarc",
    "plasmashellrc",
    "lattedockrc",
    "breezerc",
    "oxygenrc",
    "lightlyrc",
    "ksplashrc",
    "khotkeysrc",
    "systemsettingsrc",
    "kdedrc",
    "kded5rc",
    "kded6rc",
    "kconf_updaterc",
    "kscreenrc",
    "plasma-systemmonitorrc",
    "spectaclerc",
    "plasmanotifyrc",
    "plasma-localerc",
    "plasma_calendar_holiday_regions",
    "plasma_calendar_alternatecalendar",
    "plasmacamerarc",
    "plasma-welcomerc",
    "xdg-desktop-portal-kderc",
    "kactivitymanagerdrc",
    "kactivitymanagerd-statsrc",
    "kactivitymanagerd",
    "baloofilerc",
    "mimeapps.list",
    "filetypesrc",
    "Trolltech.conf",
    "powermanagementprofilesrc",
    "powerdevilrc",
    "kcmdisplayrc",
    "plasmawindowedrc",
    "bluedevilglobalrc",
    "kiorc",
    "kioworkerrc",
    "kmenueditrc",
    "kwalletrc",
    "arkrc",
    "gwenviewrc",
    "kaccessrc",
    "klaunchrc",
    "kalarmrc",
    "kclockrc",
    "kclockdrc",
    "trashrc",
    "kdedefaults",
    "plasma-workspace",
    "autostart",
    "session",
];
pub const KDE_SHARE_ENTRIES: &[&str] = &[
    "plasma",
    "kwin",
    "konsole",
    "fonts",
    "color-schemes",
    "aurorae",
    "icons",
    "wallpapers",
    "yakuake",
    "kxmlgui5",
    "kxmlgui6",
    "knewstuff3",
    "kactivitymanagerd",
    "palettes",
    "iconthemes",
    "user-places.xbel",
    "krunnerstaterc",
    "dolphin",
    "plasma-systemmonitor",
    "gwenview",
    "haruna",
    "kate",
    "themes",
];
pub const KDE_HOME_ENTRIES: &[&str] = &[".fonts", ".themes", ".icons"];

pub fn kde_has_settings() -> bool {
    let cfg = &ctx().config_home;
    ["kdeglobals", "kwinrc", "plasmashellrc", "plasma-org.kde.plasma.desktop-appletsrc"].iter().any(|f| cfg.join(f).is_file())
}

/// Curated entries that exist, plus any plasma*, k*rc and *rc file in the config root.
pub fn kde_config_entries() -> Vec<String> {
    let cfg = &ctx().config_home;
    let mut entries: Vec<String> = Vec::new();
    for e in KDE_CONFIG_ENTRIES {
        if cfg.join(e).exists() && !entries.iter().any(|x| x == e) {
            entries.push(e.to_string());
        }
    }
    let mut names: Vec<String> = fs::read_dir(cfg).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    let globs: [&dyn Fn(&str) -> bool; 3] = [&|n| n.starts_with("plasma"), &|n| n.starts_with('k') && n.ends_with("rc"), &|n| n.ends_with("rc")];
    for glob in globs {
        // Like a shell glob: no dotfiles
        for n in names.iter().filter(|n| !n.starts_with('.') && glob(n) && cfg.join(n).is_file()) {
            if entries.contains(n) {
                continue;
            }
            if matches!(n.as_str(), "bashrc" | "zshrc" | "vimrc" | "wgetrc" | "nanorc" | "inputrc" | "screenrc" | "tmux.conf" | "geminirc") {
                continue;
            }
            entries.push(n.clone());
        }
    }
    entries
}

pub fn kde_existing_paths() -> Vec<PathBuf> {
    let c = ctx();
    let mut v: Vec<PathBuf> = kde_config_entries().iter().map(|e| c.config_home.join(e)).filter(|p| p.exists()).collect();
    v.extend(KDE_SHARE_ENTRIES.iter().map(|e| c.data_home.join(e)).filter(|p| p.exists()));
    v.extend(KDE_HOME_ENTRIES.iter().map(|e| c.target_home.join(e)).filter(|p| p.exists()));
    v
}

pub fn kde_format_total_size() -> String {
    let paths = kde_existing_paths();
    if paths.is_empty() {
        return "0B".into();
    }
    let mut args = vec!["-sch".to_string()];
    args.extend(paths.iter().map(|p| p.to_string_lossy().into_owned()));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // du exits non-zero on unreadable files but still prints the total
    let out = std::process::Command::new("du").args(&argv).stderr(std::process::Stdio::null()).output();
    out.ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().last().and_then(|l| l.split('\t').next()).map(str::to_string))
        .unwrap_or_else(|| "0B".into())
}

/// rsync -a each existing named entry from src_root into dest_root.
pub fn kde_copy_named(src_root: &Path, dest_root: &Path, dry_run: bool, names: &[impl AsRef<str>]) -> Result<()> {
    if !dry_run {
        fs::create_dir_all(dest_root)?;
    }
    for e in names {
        let src = src_root.join(e.as_ref());
        if !src.exists() {
            continue;
        }
        if dry_run {
            log_info(&format!("  [Dry-run] Would sync: {} -> {}/", src.display(), dest_root.display()));
        } else {
            rsync(&["-a"], &src, &format!("{}/", dest_root.display()))?;
        }
    }
    Ok(())
}

/// `rsync <opts> <src> <dest>`; fails on a non-zero exit.
pub fn rsync(opts: &[&str], src: &Path, dest: &str) -> Result<()> {
    rsync_as(false, opts, src, dest)
}

/// Like [`rsync`], run as the target user when `as_user` and kast is root (through sudo -u),
/// so everything it creates belongs to that user from the start.
pub fn rsync_as(as_user: bool, opts: &[&str], src: &Path, dest: &str) -> Result<()> {
    let mut cmd = if as_user && ctx().is_root {
        let src = src.to_string_lossy();
        let mut args = vec!["rsync"];
        args.extend_from_slice(opts);
        args.extend([src.as_ref(), dest]);
        ctx::run_as_user(&args)
    } else {
        let mut c = std::process::Command::new("rsync");
        c.args(opts).arg(src).arg(dest);
        c
    };
    if report::owns_terminal() {
        cmd.stdin(std::process::Stdio::null()).stdout(ctx::log_stdio()).stderr(ctx::log_stdio());
    }
    let status = report::run_tracked(&mut cmd).context("running rsync")?;
    if !status.success() {
        bail!("rsync {} -> {dest} failed ({status})", src.display());
    }
    Ok(())
}

/// `rsync -a <opts> src/ dest/`
pub fn rsync_dir(extra: &[String], src: &Path, dest: &Path) -> Result<()> {
    rsync_dir_as(false, extra, src, dest)
}

pub fn rsync_dir_as(as_user: bool, extra: &[String], src: &Path, dest: &Path) -> Result<()> {
    let mut opts: Vec<&str> = vec!["-a"];
    opts.extend(extra.iter().map(String::as_str));
    rsync_as(as_user, &opts, Path::new(&format!("{}/", src.display())), &format!("{}/", dest.display()))
}

// --- Processes ---------------------------------------------------------------------

/// pgrep invocations tried in order; the first that finds something wins.
fn pgrep_specs(app: &str) -> Vec<Vec<&'static str>> {
    match app {
        "beeper" | "beepertexts" => vec![vec!["-x", "beeper"], vec!["-x", "beepertexts"], vec!["-f", "/opt/beeper/beepertexts"]],
        "zalo" | "zalodata" => vec![vec!["-x", "zalo"], vec!["-f", "/opt/zalo-for-linux/zalo"]],
        // -f patterns use [^ ]* so a match stays inside one /usr or /opt path instead of
        // spanning arguments (".*zen" also matched e.g. "vim /usr/share/doc/citizen.txt").
        "vesktop" => vec![vec!["-x", "vesktop"], vec!["-f", "/(usr|opt)/[^ ]*/vesktop(/| |$)"]],
        "zen" => vec![vec!["-x", "zen-bin"], vec!["-x", "zen"], vec!["-x", "zen-browser"], vec!["-f", "/(usr|opt)/([^ ]*/)?zen[^ /]*/zen[^ /]*( |$)"]],
        "obsidian" => vec![vec!["-x", "obsidian"], vec!["-f", "/(usr|opt)/(lib|bin|share)/obsidian"]],
        "fcitx5" => vec![vec!["-x", "fcitx5"], vec!["-x", "fcitx5-daemon"]],
        "antigravity" => vec![vec!["-x", "antigravity"], vec!["-f", "/(usr|opt)/[^ ]*/antigravity(/| |$)"]],
        "howdy" => vec![vec!["-x", "howdy"], vec!["-x", "howdy_compare"]],
        // Never kill the Plasma or Hyprland session
        _ => vec![],
    }
}

/// PIDs of a running app. Only the target user's processes (as root, -k must never kill
/// other users' apps), except Howdy, which runs as root from PAM.
pub fn running_pids(app: &str) -> Vec<i32> {
    let user = ctx().target_user.clone();
    let me = std::process::id() as i32;
    for spec in pgrep_specs(app) {
        let mut args: Vec<&str> = Vec::new();
        if app != "howdy" {
            args.extend(["-u", user.as_str()]);
        }
        args.extend(spec);
        if let Some(out) = output("pgrep", &args) {
            let pids: Vec<i32> = out.split_whitespace().filter_map(|p| p.parse().ok()).filter(|&p| p != me).collect();
            return pids;
        }
    }
    Vec::new()
}

pub fn pid_list(pids: &[i32]) -> String {
    pids.iter().map(i32::to_string).collect::<Vec<_>>().join(" ")
}

pub fn process_running(name: &str, user: Option<&str>) -> bool {
    match user {
        Some(u) => ok("pgrep", &["-u", u, "-x", name]),
        None => ok("pgrep", &["-x", name]),
    }
}

/// An app kast closed for a backup, and how to start it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaunch {
    pub app: String,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// The processes among `procs` (pid, parent pid) whose parent isn't one of them: the ones
/// that started the app (an Electron app's or Firefox's helpers are their children).
pub fn top_processes(procs: &[(i32, i32)]) -> Vec<i32> {
    procs.iter().filter(|(_, ppid)| !procs.iter().any(|(p, _)| p == ppid)).map(|(p, _)| *p).collect()
}

/// The parent pid from `/proc/<pid>/stat` ("pid (comm) state ppid ..."; comm may hold spaces).
pub fn stat_ppid(stat: &str) -> Option<i32> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// `/proc/<pid>/cmdline` as arguments.
pub fn cmdline_argv(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

/// How to start `apps` again after closing them (read before they're closed). The face model
/// isn't an app, and the desktop session is never touched.
pub fn relaunch_plan(apps: &[String]) -> Vec<Relaunch> {
    let mut out: Vec<Relaunch> = Vec::new();
    for app in apps.iter().filter(|a| !matches!(a.as_str(), "howdy" | "kde" | "hypr")) {
        let procs: Vec<(i32, i32)> = running_pids(app)
            .into_iter()
            .filter_map(|p| fs::read_to_string(format!("/proc/{p}/stat")).ok().and_then(|s| stat_ppid(&s)).map(|pp| (p, pp)))
            .collect();
        for pid in top_processes(&procs) {
            let argv = cmdline_argv(&fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default());
            if argv.is_empty() || out.iter().any(|r| r.argv == argv) {
                continue;
            }
            out.push(Relaunch { app: app.clone(), argv, cwd: fs::read_link(format!("/proc/{pid}/cwd")).ok() });
        }
    }
    out
}

/// The desktop the user runs, for starting apps in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    /// Plasma: `kstart --iconify` starts a window minimized
    Kde,
    /// Hyprland (no minimizing): a hidden special workspace, without focus
    Hyprland,
    Other,
}

pub fn desktop_kind(current_desktop: &str) -> Desktop {
    let d = current_desktop.to_ascii_lowercase();
    if d.split(':').any(|p| p == "kde") {
        Desktop::Kde
    } else if d.split(':').any(|p| p == "hyprland") {
        Desktop::Hyprland
    } else {
        Desktop::Other
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// How to start a closed app again in the background: in its tray when the app can do that
/// itself (Vesktop), else minimized (Plasma) or on a hidden workspace (Hyprland). An app
/// without a window (fcitx5), or another desktop: as it was started.
pub fn background_argv(app: &str, argv: &[String], desktop: Desktop, kstart: Option<&str>) -> Vec<String> {
    if app == "fcitx5" {
        return argv.to_vec();
    }
    if app == "vesktop" {
        let mut v = argv.to_vec();
        if !v.iter().any(|a| a == "--start-minimized") {
            v.push("--start-minimized".into());
        }
        return v;
    }
    match (desktop, kstart) {
        (Desktop::Kde, Some(k)) => [k, "--iconify", "--"].iter().map(|s| s.to_string()).chain(argv.iter().cloned()).collect(),
        (Desktop::Hyprland, _) => {
            let cmd: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
            vec!["hyprctl".into(), "dispatch".into(), "exec".into(), format!("[workspace special:kast silent] {}", cmd.join(" "))]
        }
        _ => argv.to_vec(),
    }
}

/// Start the closed apps again, as the user, in the background (see `background_argv`).
pub fn relaunch(list: &[Relaunch]) {
    let (desktop, extra) = crate::logio::os::desktop_kind();
    let kstart = ["kstart", "kstart6", "kstart5"].into_iter().find(|k| crate::ctx::have(k));
    for r in list {
        let argv = background_argv(&r.app, &r.argv, desktop, kstart);
        let started = crate::logio::os::start_detached(&argv, r.cwd.as_deref(), &extra)
            // The wrapper (kstart, hyprctl) didn't work out: start it plainly
            .or_else(|e| if argv == r.argv { Err(e) } else { crate::logio::os::start_detached(&r.argv, r.cwd.as_deref(), &extra) });
        match started {
            Ok(()) => log_info(&format!("Started {} again in the background.", r.app)),
            Err(e) => log_warn(&format!("Could not start {} again ({e}); start it yourself.", r.app)),
        }
    }
}

/// SIGTERM (so databases commit), wait up to 7s, then SIGKILL. False if any survived.
pub fn terminate_apps(apps: &[String]) -> bool {
    let mut failed = false;
    for app in apps {
        let mut pids = running_pids(app);
        if pids.is_empty() {
            continue;
        }
        log_step(&format!("Closing {app} gracefully (PID: {})...", pid_list(&pids)));
        for &p in &pids {
            let _ = kill(Pid::from_raw(p), Signal::SIGTERM);
        }
        let mut stopped = false;
        for _ in 0..14 {
            std::thread::sleep(Duration::from_millis(500));
            pids = running_pids(app);
            if pids.is_empty() {
                stopped = true;
                break;
            }
        }
        if !stopped {
            log_warn(&format!("{app} did not shut down gracefully within 7 seconds. Forcing termination (SIGKILL)..."));
            for &p in &pids {
                let _ = kill(Pid::from_raw(p), Signal::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let left = running_pids(app);
        if left.is_empty() {
            log_success(&format!("{app} terminated successfully."));
        } else {
            log_error(&format!("Failed to stop {app} (PID: {}).", pid_list(&left)));
            failed = true;
        }
    }
    !failed
}

/// `du -sh` of a path, "0B" if missing.
pub fn format_bytes(path: &Path) -> String {
    if !path.exists() {
        return "0B".into();
    }
    let out = std::process::Command::new("du").arg("-sh").arg(path).stderr(std::process::Stdio::null()).output();
    out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).split('\t').next().map(str::to_string)).filter(|s| !s.is_empty()).unwrap_or_else(|| "0B".into())
}

// --- Staging directories (kept off tmpfs/RAM) ----------------------------------------------

const STAGING_PREFIXES: [&str; 2] = ["app-vault-sync-backup-", "app-vault-sync-restore-"];

fn cleanup_stale_staging() {
    let cutoff = SystemTime::now() - Duration::from_secs(15 * 60);
    let dirs = [PathBuf::from("/tmp"), PathBuf::from("/var/tmp"), PathBuf::from("/media/data/.vault-tmp"), ctx().cache_home.join("app-vault-sync")];
    for d in dirs.iter().filter(|d| d.is_dir() && ctx::is_writable(d)) {
        let Ok(rd) = fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !STAGING_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let Ok(meta) = e.path().symlink_metadata() else { continue };
            let empty = if meta.is_dir() { fs::read_dir(e.path()).is_ok_and(|mut r| r.next().is_none()) } else { meta.len() == 0 };
            let old = meta.modified().is_ok_and(|m| m < cutoff);
            if empty || old {
                let _ = if meta.is_dir() { fs::remove_dir_all(e.path()) } else { fs::remove_file(e.path()) };
            }
        }
    }
}

fn mktemp_dir(parent: &Path, prefix: &str) -> Result<PathBuf> {
    Ok(tempfile::Builder::new()
        .prefix(&format!("{prefix}-"))
        .tempdir_in(parent)
        .with_context(|| format!("creating staging dir in {}", parent.display()))?
        .keep())
}

pub fn get_safe_staging_dir(prefix: &str) -> Result<PathBuf> {
    cleanup_stale_staging();
    let c = ctx();

    // 1. /media/data (large disk) - only if it keeps Unix permissions. On the NTFS data
    //    partition (umask=000) every staged file becomes 0777, which would be baked into
    //    the archive and restored into ~/.config that way.
    let data = Path::new("/media/data");
    let fstype = output("findmnt", &["-no", "FSTYPE", "--target", "/media/data"]).unwrap_or_default();
    let fstype = fstype.trim();
    if data.is_dir() && ctx::is_writable(data) && !matches!(fstype, "ntfs" | "ntfs3" | "fuseblk" | "vfat" | "exfat" | "msdos") {
        let p = data.join(".vault-tmp");
        let _ = fs::create_dir_all(&p);
        if c.is_root && c.target_uid != 0 {
            ctx::chown_path(&p, c.target_uid, c.target_gid);
        }
        if p.is_dir() && ctx::is_writable(&p) {
            return mktemp_dir(&p, prefix);
        }
    }
    // 2. /var/tmp (persistent disk, not tmpfs)
    let vt = Path::new("/var/tmp");
    if vt.is_dir() && ctx::is_writable(vt) {
        return mktemp_dir(vt, prefix);
    }
    // 3. User cache directory
    let cache = c.cache_home.join("app-vault-sync");
    let _ = ctx::mkdir_user(&cache);
    if cache.is_dir() && ctx::is_writable(&cache) {
        return mktemp_dir(&cache, prefix);
    }
    // 4. /tmp
    mktemp_dir(Path::new("/tmp"), prefix)
}

/// Owner for files handed back to the invoking user (SUDO_UID, else the target user).
pub fn file_owner() -> (u32, u32) {
    let c = ctx();
    let env_id = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    (env_id("SUDO_UID").unwrap_or(c.target_uid), env_id("SUDO_GID").unwrap_or(c.target_gid))
}

pub fn is_dat(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".dat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_reads_the_top_process() {
        // beeper (100, started by plasmashell 50) and its helpers; a second window process
        let procs = [(100, 50), (101, 100), (102, 101), (300, 1)];
        assert_eq!(top_processes(&procs), vec![100, 300]);
        assert_eq!(stat_ppid("4242 (Web Content (x)) S 4100 4242 ..."), Some(4100));
        assert_eq!(stat_ppid("garbage"), None);
        assert_eq!(cmdline_argv(b"/opt/beeper/beepertexts\0--no-sandbox\0\0"), vec!["/opt/beeper/beepertexts".to_string(), "--no-sandbox".into()]);
        assert!(cmdline_argv(b"").is_empty());
    }

    #[test]
    fn apps_come_back_in_the_background() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let beeper = argv(&["/opt/beeper/beepertexts", "--no-sandbox"]);
        assert_eq!(desktop_kind("KDE"), Desktop::Kde);
        assert_eq!(desktop_kind("Hyprland"), Desktop::Hyprland);
        assert_eq!(desktop_kind("ubuntu:GNOME"), Desktop::Other);
        assert_eq!(
            background_argv("beeper", &beeper, Desktop::Kde, Some("kstart")),
            argv(&["kstart", "--iconify", "--", "/opt/beeper/beepertexts", "--no-sandbox"])
        );
        assert_eq!(background_argv("beeper", &beeper, Desktop::Kde, None), beeper);
        assert_eq!(
            background_argv("zen", &argv(&["/opt/zen/zen-bin", "it's"]), Desktop::Hyprland, None),
            argv(&["hyprctl", "dispatch", "exec", "[workspace special:kast silent] '/opt/zen/zen-bin' 'it'\\''s'"])
        );
        assert_eq!(background_argv("vesktop", &argv(&["vesktop"]), Desktop::Kde, Some("kstart")), argv(&["vesktop", "--start-minimized"]));
        assert_eq!(background_argv("fcitx5", &argv(&["fcitx5", "-d"]), Desktop::Kde, Some("kstart")), argv(&["fcitx5", "-d"]));
        assert_eq!(background_argv("obsidian", &argv(&["obsidian"]), Desktop::Other, Some("kstart")), argv(&["obsidian"]));
    }
}
