//! App Vault: backup, export, import & status of app data, KDE Plasma, Hyprland and
//! Howdy face models.

pub mod archive;
pub mod backup;
pub mod import;
pub mod info;
pub mod status;

use std::fs;
use std::path::{Path, PathBuf};
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
    let mut cmd = std::process::Command::new("rsync");
    cmd.args(opts).arg(src).arg(dest);
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
    let mut opts: Vec<&str> = vec!["-a"];
    opts.extend(extra.iter().map(String::as_str));
    rsync(&opts, Path::new(&format!("{}/", src.display())), &format!("{}/", dest.display()))
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
