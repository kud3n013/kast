use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::ctx::{self, ctx, have, ok, ok_priv, reported, run_as_user};
use crate::report::{self, Prompt};
use crate::ui::{C_BOLD, C_RESET};

use super::archive::{extract_zip_archive, read_zip_manifest};
use super::info::jq_str;
use super::*;

#[derive(Debug, Clone, PartialEq)]
pub struct ImportOpts {
    pub file: PathBuf,
    pub app: Option<String>,
    pub safety_backup: bool,
    pub kill: bool,
    /// Restore over running apps without closing them
    pub force: bool,
    /// The caller already confirmed the overwrite: skip the final "proceed?" question
    pub confirmed: bool,
    pub dry_run: bool,
    /// Don't start the apps kast closed again afterwards
    pub no_relaunch: bool,
}

/// Where a config-directory app lives in an archive (old layouts included) and in ~/.config.
struct RestoreSpec {
    sources: &'static [&'static str],
    folder: &'static str,
    label: &'static str,
}

fn restore_spec(app: &str) -> Option<RestoreSpec> {
    let s = |sources, folder, label| RestoreSpec { sources, folder, label };
    Some(match app {
        "beeper" => s(&["BeeperTexts", "beeper/config", "beeper"], "BeeperTexts", "BeeperTexts data"),
        "zalo" => s(&["ZaloData", "zalo/config", "zalo"], "ZaloData", "ZaloData"),
        "vesktop" => s(&["vesktop", "vesktop/config"], "vesktop", "Vesktop data"),
        "zen" => s(&["zen", "zen/config"], "zen", "Zen Browser data"),
        "helium" => s(&["net.imput.helium"], "net.imput.helium", "Helium data"),
        "obsidian" => s(&["obsidian", "obsidian/config"], "obsidian", "Obsidian config"),
        "fcitx5" => s(&["fcitx5", "fcitx5/config"], "fcitx5", "Fcitx5 config"),
        "antigravity" => s(&["Antigravity", "antigravity/config", "antigravity"], "Antigravity", "Antigravity config"),
        "fdm" => s(&["Softdeluxe"], "Softdeluxe", "Free Download Manager data"),
        "fish" => s(&["fish"], "fish", "fish config"),
        "fastfetch" => s(&["fastfetch"], "fastfetch", "fastfetch config"),
        "scripts" => s(&["scripts"], ".scripts", "scripts"),
        "zotero" => s(&["Zotero"], "Zotero", "Zotero library"),
        "hypr" => s(&["hypr", "hypr/config"], "hypr", "Hyprland config"),
        _ => return None,
    })
}

/// What restoring `app` overwrites, for a confirmation dialog.
pub fn restore_destinations(app: &str) -> Vec<String> {
    let c = ctx();
    let cfg = c.config_home.display();
    match app {
        "howdy" => vec![format!("{HOWDY_MODELS_DIR}/*.dat (face models in the archive, incl. {}.dat)", c.target_user)],
        "kde" => vec![
            format!("KDE Plasma settings in {cfg} (kdeglobals, kwinrc, Plasma applets, shortcuts, ...)"),
            format!("KDE data in {} (plasma, kwin, icons, wallpapers, ...)", c.data_home.display()),
            "~/.fonts, ~/.themes, ~/.icons (if in the archive)".into(),
        ],
        "kwallet" => vec![
            format!("KDE wallets in {} (the ones in the archive; others are kept)", keyring::kwallet_dir().display()),
            keyring::kwalletrc_path().display().to_string(),
        ],
        "gnome-keyring" => vec![format!("GNOME keyrings in {} (the ones in the archive; others are kept)", keyring::gnome_dir().display())],
        other => match restore_spec(other) {
            Some(spec) => {
                let mut v = vec![restore_base(other).join(spec.folder).display().to_string()];
                if other == "zalo" {
                    v.push(format!("{cfg}/zalo-flags.conf"));
                }
                if let Some(share) = share_path(other) {
                    v.push(share.display().to_string());
                }
                v
            }
            None => vec![],
        },
    }
}

/// What a restore of `apps` would overwrite that exists now (what its safety snapshot copies).
pub fn snapshot_paths(apps: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for app in apps {
        match app.as_str() {
            "howdy" => out.push(PathBuf::from(HOWDY_MODELS_DIR)),
            "kde" => out.extend(crate::vault::kde_existing_paths()),
            "kwallet" => out.extend([keyring::kwallet_dir(), keyring::kwalletrc_path()]),
            "gnome-keyring" => out.push(keyring::gnome_dir()),
            other => {
                if let Some(spec) = restore_spec(other) {
                    out.push(restore_base(other).join(spec.folder));
                }
                out.extend(share_path(other));
            }
        }
    }
    out.retain(|p| p.exists());
    out
}

/// Total size of `paths` in bytes (`du -sbc`; runs off the UI thread).
pub fn bytes_of(paths: &[PathBuf]) -> u64 {
    if paths.is_empty() {
        return 0;
    }
    let out = Command::new("du").arg("-sbc").args(paths).stderr(Stdio::null()).output();
    out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).lines().last().and_then(|l| l.split_whitespace().next()).and_then(|n| n.parse().ok())).unwrap_or(0)
}

/// The warning for a restore without a safety snapshot.
pub fn no_snapshot_warning(apps: &[String]) -> String {
    let folders: Vec<String> = apps.iter().flat_map(|a| restore_destinations(a)).collect();
    format!("Without a snapshot this import cannot be undone: your current data in {} will be overwritten permanently.", folders.join(", "))
}

fn chmod(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn howdy_list() {
    if !have("howdy") {
        return;
    }
    report::blank();
    log_info("Verifying enrolled models with howdy list:");
    let user = &ctx().target_user;
    let Some(out) = priv_output(&["howdy", "-U", user, "list"]).or_else(|| priv_output(&["howdy", "list"])) else { return };
    // Into the log either way; on the terminal too when no full-screen UI owns it
    ctx::log_raw(&out);
    if !report::owns_terminal() {
        let _ = std::io::stdout().write_all(&out);
    }
}

/// Stdout of a command run as root (through `sudo -n` when not root), if it succeeded.
fn priv_output(args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = if ctx().is_root {
        let mut c = Command::new(args[0]);
        c.args(&args[1..]);
        c
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").args(args);
        c
    };
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// Run a shell snippet (needed for globs inside root-only /etc/howdy/models) as root.
fn sh_priv(script: &str, args: &[&str]) -> bool {
    let mut v = vec!["-c", script, "sh"];
    v.extend_from_slice(args);
    ok_priv("sh", &v)
}

fn import_dat(o: &ImportOpts) -> Result<()> {
    let c = ctx();
    let user = &c.target_user;
    let dest_file = howdy_model_path(user);
    log_step(&format!("Detected standalone Howdy face model: {C_BOLD}{}{C_RESET}", o.file.display()));
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore face model to {}", dest_file.display()));
        return Ok(());
    }
    if !o.confirmed {
        report::blank();
        log_warn(&format!("Restoring will install this face model for user '{user}' at {}.", dest_file.display()));
        if !report::ask(&Prompt::yes_no("Proceed with face model import?", false)) {
            log_info("Import operation canceled.");
            ctx::set_canceled(true);
            return Ok(());
        }
    }
    if !sudo_ready() {
        warn_sudo_needed("the face model");
        return Err(reported());
    }

    let dest_s = dest_file.to_string_lossy().into_owned();
    let mut snapshot = None;
    if o.safety_backup && (dest_file.is_file() || (!c.is_root && have("sudo") && ok("sudo", &["-n", "test", "-f", &dest_s]))) {
        let rollback = safety_backup_dir().join(format!("safety_rollback_{}", ctx::now_str("%Y-%m-%d_%H%M%S")));
        let models = rollback.join("howdy/models");
        ctx::mkdir_user(&models)?;
        ok_priv("cp", &["-p", &dest_s, &format!("{}/", models.display())]);
        log_info(&format!("Safety snapshot of existing model saved to: {}", rollback.display()));
        snapshot = Some(rollback);
    }

    let src = o.file.to_string_lossy();
    let steps: [&[&str]; 6] = [
        &["mkdir", "-p", HOWDY_MODELS_DIR],
        &["chmod", "700", HOWDY_MODELS_DIR],
        &["chown", "root:root", HOWDY_MODELS_DIR],
        &["cp", &src, &dest_s],
        &["chown", "root:root", &dest_s],
        &["chmod", "600", &dest_s],
    ];
    for step in steps {
        if !ok_priv(step[0], &step[1..]) {
            log_error(&format!("Failed: {}", step.join(" ")));
            return Err(reported());
        }
    }
    log_success(&format!("Imported Howdy face model for '{user}' from: {}", o.file.display()));
    // Without a snapshot (none taken, or there was no model before) this can't be undone
    let fp = snapshot.as_deref().and_then(crate::journal::fingerprint);
    crate::journal::import(&o.file, &["howdy".to_string()], snapshot.as_deref(), fp);
    howdy_list();
    Ok(())
}

/// The parts of the Plasma session a KDE restore stops and starts. A seam, so tests can watch
/// the order without touching the desktop they run in.
trait KdeSession {
    fn shell_running(&self) -> bool;
    fn stop_shell(&self);
    /// Start plasmashell again and have KWin reload its settings.
    fn start_shell(&self);
    /// See [`crate::plasma::reload::stop_shortcut_daemon`].
    fn stop_shortcuts(&self) -> Result<bool, String>;
    fn start_shortcuts(&self) -> Result<(), String>;
}

/// Everything a restore stops and starts in the user's session.
trait Session: KdeSession + keyring::KeyringDaemons {}

impl<T: KdeSession + keyring::KeyringDaemons> Session for T {}

/// The user's real session.
struct LiveSession;

impl keyring::KeyringDaemons for LiveSession {
    fn stop_keyring(&self, store: keyring::Store) -> Result<keyring::Restart, String> {
        keyring::LiveDaemons.stop_keyring(store)
    }

    fn start_keyring(&self, store: keyring::Store, how: &keyring::Restart) -> Result<(), String> {
        keyring::LiveDaemons.start_keyring(store, how)
    }
}

impl KdeSession for LiveSession {
    fn shell_running(&self) -> bool {
        process_running("plasmashell", Some(&ctx().target_user))
    }

    fn stop_shell(&self) {
        let _ = user_ok(&["systemctl", "--user", "stop", "plasma-plasmashell.service"]) || user_ok(&["kquitapp6", "plasmashell"]);
        crate::ui::sleep_secs(1.0);
    }

    fn start_shell(&self) {
        if !user_ok(&["systemctl", "--user", "start", "plasma-plasmashell.service"]) {
            let _ = user_cmd(&["kstart", "plasmashell"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
        user_ok(&["qdbus6", "org.kde.KWin", "/KWin", "reconfigure"]);
    }

    fn stop_shortcuts(&self) -> Result<bool, String> {
        crate::plasma::reload::stop_shortcut_daemon()
    }

    fn start_shortcuts(&self) -> Result<(), String> {
        crate::plasma::reload::start_shortcut_daemon()
    }
}

/// `kglobalacceld` is stopped while this lives. [`start`](Self::start), or dropping it (an
/// error, a canceled job unwinding), starts it again: a restore never leaves the user
/// without global shortcuts.
struct ShortcutsStopped<'a> {
    session: &'a dyn KdeSession,
    stopped: bool,
}

impl<'a> ShortcutsStopped<'a> {
    fn stop(session: &'a dyn KdeSession) -> Self {
        log_info("Stopping kglobalacceld so it can't write its old shortcuts over the restored ones...");
        let stopped = session.stop_shortcuts().unwrap_or_else(|e| {
            log_warn(&format!("Could not stop kglobalacceld ({e}); the restored shortcuts may only take effect after you log out and in."));
            // It may still be on its way out: starting it again is harmless if it isn't
            true
        });
        ShortcutsStopped { session, stopped }
    }

    fn start(mut self) {
        self.restart();
    }

    fn restart(&mut self) {
        if !std::mem::take(&mut self.stopped) {
            return;
        }
        if let Err(e) = self.session.start_shortcuts() {
            let hint = if e.contains("log out") { String::new() } else { "; log out and in to load the shortcuts".into() };
            log_warn(&format!("Could not start kglobalacceld again: {e}{hint}"));
        }
    }
}

impl Drop for ShortcutsStopped<'_> {
    fn drop(&mut self) {
        self.restart();
    }
}

pub fn cmd_import(o: ImportOpts) -> Result<()> {
    import_with(o, &LiveSession)
}

fn import_with(o: ImportOpts, session: &dyn Session) -> Result<()> {
    let c = ctx();
    ctx::set_canceled(false);
    require_vault_tools()?;
    if !o.file.is_file() {
        log_error(&format!("Archive file not found: {}", o.file.display()));
        return Err(reported());
    }
    if is_dat(&o.file) {
        return import_dat(&o);
    }

    log_step(&format!("Inspecting archive: {C_BOLD}{}{C_RESET}", o.file.display()));
    let Some(raw) = read_zip_manifest(&o.file) else {
        log_error("Archive is missing 'backup_manifest.json' or format is unrecognized.");
        return Err(reported());
    };
    let manifest: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    // jq's `keys` order: sorted
    let mut available: Vec<String> = manifest.get("apps").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default();
    available.sort();
    if available.is_empty() {
        log_warn("No applications found in manifest. Will attempt auto-detection during extraction.");
    }

    let mut apps: Vec<String> = match &o.app {
        Some(a) if normalize_app(a) == "keyrings" => {
            let found: Vec<String> = keyring::TARGETS.iter().map(|k| k.to_string()).filter(|k| available.is_empty() || available.contains(k)).collect();
            if found.is_empty() {
                log_error("The archive has no keyrings (kwallet, gnome-keyring).");
                log_info(&format!("Available in archive: {}", available.join(" ")));
                return Err(reported());
            }
            found
        }
        Some(a) => {
            let a = normalize_app(a);
            if !available.is_empty() && !available.contains(&a) {
                log_error(&format!("Application '{a}' is not listed in archive manifest."));
                log_info(&format!("Available in archive: {}", available.join(" ")));
                return Err(reported());
            }
            vec![a]
        }
        None => available.clone(),
    };
    keyrings_first(&mut apps);
    if !apps.is_empty() {
        log_info(&format!("Apps to restore: {C_BOLD}{}{C_RESET}", apps.join(" ")));
    }

    let mut running: Vec<String> = Vec::new();
    let mut running_info: Vec<String> = Vec::new();
    for app in &apps {
        let pids = running_pids(app);
        if !pids.is_empty() {
            log_warn(&format!("Application '{app}' is running (PID: {}).", pid_list(&pids)));
            running.push(app.clone());
            running_info.push(format!("{app} (PID: {})", pid_list(&pids)));
        }
    }
    let mut _relaunch = RelaunchAfter(Vec::new());
    let plan = || if o.no_relaunch || o.dry_run { Vec::new() } else { relaunch_plan(&running) };
    if !running.is_empty() {
        let list = running.join(" ");
        if o.dry_run {
            log_warn(&format!("Notice: Running applications detected ({list}). During a live restore, they will need to be closed."));
        } else if o.kill {
            _relaunch.0 = plan();
            if !terminate_apps(&running) {
                log_error("Could not terminate running apps. Restore aborted.");
                return Err(reported());
            }
        } else if o.force {
            log_warn("--force flag used: Proceeding with restoration while apps are running. This is hazardous!");
        } else {
            report::blank();
            log_warn(&format!("Target application(s) ({list}) are currently running. Overwriting open files will cause crashes or data corruption."));
            let ask = Prompt::yes_no(format!("Close/kill running application(s) ({list}) to proceed with restore?"), true);
            if report::ask(&ask.with_details(running_info)) {
                _relaunch.0 = plan();
                // Only an explicit -k may SIGKILL: a prompt answer closes them gracefully
                if !terminate_apps_with(&running, false) {
                    log_error("Could not terminate running applications. Restore aborted.");
                    return Err(reported());
                }
            } else {
                log_error("Cannot safely restore files while target applications are running. Please close them and re-run.");
                return Err(reported());
            }
        }
    }

    if !o.confirmed && !o.dry_run {
        report::blank();
        log_warn("Restoring will OVERWRITE existing application settings, login credentials, and databases!");
        let overwrites: Vec<String> = apps.iter().flat_map(|a| restore_destinations(a)).collect();
        if !report::ask(&Prompt::yes_no("Are you sure you want to proceed?", false).with_details(overwrites)) {
            log_info("Restore operation canceled.");
            ctx::set_canceled(true);
            return Ok(());
        }
    }

    // Ask for the sudo password now rather than in the middle of the restore
    if !o.dry_run && apps.iter().any(|a| a == "howdy") {
        sudo_ready();
    }

    let staging = get_safe_staging_dir("app-vault-sync-restore")?;
    ctx::set_staging(staging.clone());
    log_step("Extracting archive to temporary workspace...");
    extract_zip_archive(&o.file, &staging)?;
    prepare_staging(&staging);

    if apps.is_empty() {
        let has = |names: &[&str]| names.iter().any(|n| staging.join(n).is_dir());
        let detect: [(&str, &[&str]); 18] = [
            ("beeper", &["BeeperTexts", "beeper"]),
            ("zalo", &["ZaloData", "zalo"]),
            ("vesktop", &["vesktop"]),
            ("zen", &["zen"]),
            ("helium", &["net.imput.helium"]),
            ("obsidian", &["obsidian"]),
            ("fcitx5", &["fcitx5"]),
            ("antigravity", &["Antigravity", "antigravity"]),
            ("fdm", &["Softdeluxe", "fdm-share"]),
            ("fish", &["fish", "fish-share"]),
            ("fastfetch", &["fastfetch"]),
            ("scripts", &["scripts"]),
            ("zotero", &["Zotero", "zotero-share"]),
            ("howdy", &["howdy"]),
            ("kde", &["kde"]),
            ("hypr", &["hypr"]),
            ("kwallet", &["kwallet"]),
            ("gnome-keyring", &["gnome-keyring"]),
        ];
        apps = detect.iter().filter(|(_, n)| has(n)).map(|(a, _)| a.to_string()).collect();
        keyrings_first(&mut apps);
        log_info(&format!("Auto-detected apps in archive: {C_BOLD}{}{C_RESET}", apps.join(" ")));
    }

    rehome::rehome_staging(&staging, &apps, &manifest, o.dry_run);
    // kwalletrc is in both: the copy that came with the wallets wins (it names them)
    let kde_rc = staging.join("kde/config/kwalletrc");
    if apps.iter().any(|a| a == "kwallet") && apps.iter().any(|a| a == "kde") && staging.join("kwallet/kwalletrc").is_file() && kde_rc.is_file() {
        fs::remove_file(&kde_rc)?;
    }

    if !o.safety_backup && !o.dry_run {
        log_warn(&no_snapshot_warning(&apps));
    }
    let safety_dir = safety_backup_dir();
    let rollback = safety_dir.join(format!("safety_rollback_{}", ctx::now_str("%Y-%m-%d_%H%M%S")));
    if !o.dry_run {
        ctx::mkdir_user(&c.config_home)?;
        if o.safety_backup {
            ctx::mkdir_user(&safety_dir)?;
        }
    }

    // The order doesn't matter to the shortcuts: restore_kde has kglobalacceld running again
    // on the restored kglobalshortcutsrc before it returns, and the apps closed above are
    // started again (dropping `_relaunch`) only after every app is restored.
    for app in &apps {
        log_step(&format!("Restoring: {C_BOLD}{app}{C_RESET}"));
        match app.as_str() {
            "howdy" => restore_howdy(&staging, &rollback, &manifest, &o)?,
            "kde" => restore_kde(&staging, &rollback, &o, session)?,
            k if keyring::is_keyring(k) => {
                let r = keyring::RestoreCtx {
                    staging: &staging,
                    rollback: &rollback,
                    dry_run: o.dry_run,
                    safety_backup: o.safety_backup,
                    manifest: &manifest,
                    daemons: session,
                };
                keyring::restore(k, &r)?;
            }
            other => {
                if let Some(spec) = restore_spec(other) {
                    restore_app_dir(other, &spec, &staging, &rollback, &o)?;
                }
            }
        }
    }

    if o.dry_run {
        ctx::cleanup_vault_staging();
        report::blank();
        log_success("Dry run complete. No modifications were made.");
        return Ok(());
    }

    verify_restore(&apps, &rollback);
    ctx::cleanup_vault_staging();
    report::blank();
    log_success("Restore process finished successfully!");
    let snapshot = (o.safety_backup && rollback.is_dir()).then_some(rollback.as_path());
    crate::journal::import(&o.file, &apps, snapshot, snapshot.and_then(crate::journal::fingerprint));
    if o.safety_backup && rollback.is_dir() {
        log_info(&format!("Safety rollback snapshot created at: {}", rollback.display()));
    }
    Ok(())
}

/// The keyrings go in before everything else: an app restored and started first could ask
/// for its secret and get a freshly created, empty keyring.
fn keyrings_first(apps: &mut [String]) {
    apps.sort_by_key(|a| !keyring::is_keyring(a));
}

fn restore_app_dir(app: &str, spec: &RestoreSpec, staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let c = ctx();
    restore_share_dir(app, staging, rollback, o)?;
    let Some(src) = spec.sources.iter().map(|s| staging.join(s)).find(|p| p.is_dir()) else { return Ok(()) };
    let dest = restore_base(app).join(spec.folder);
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore {} to: {}", spec.label, dest.display()));
        return Ok(());
    }
    if o.safety_backup && dest.is_dir() {
        let snap = rollback.join(spec.folder);
        fs::create_dir_all(&snap)?;
        rsync_dir(&[], &dest, &snap)?;
    }
    ctx::mkdir_user(&dest)?;
    rsync_dir_as(as_user(), &restore_opts(), &src, &dest)?;
    chmod(&dest, 0o700)?;

    if app == "zalo" {
        let flags_dest = c.config_home.join("zalo-flags.conf");
        if let Some(f) = ["zalo-flags.conf", "zalo/extra/zalo-flags.conf"].iter().map(|f| staging.join(f)).find(|p| p.is_file())
            && !user_ok(&["cp", "-p", &f.to_string_lossy(), &flags_dest.to_string_lossy()])
        {
            bail!("copying {} failed", f.display());
        }
        user_ok(&["chmod", "u+rw", &flags_dest.to_string_lossy()]);
    }
    if app == "hypr" {
        if have("hyprctl") && process_running("Hyprland", None) {
            log_info("Hyprland is running; triggering reload via hyprctl reload...");
            let _ = user_cmd(&["hyprctl", "reload"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        log_success("Hyprland configuration restored successfully!");
    }
    Ok(())
}

/// The app's second folder (`share_path`), archived as `<app>-share`.
fn restore_share_dir(app: &str, staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let Some(dest) = share_path(app) else { return Ok(()) };
    let src = staging.join(format!("{app}-share"));
    if !src.is_dir() {
        return Ok(());
    }
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore {app} data to: {}", dest.display()));
        return Ok(());
    }
    if o.safety_backup && dest.is_dir() {
        let snap = rollback.join(format!("{app}-share"));
        fs::create_dir_all(&snap)?;
        rsync_dir(&[], &dest, &snap)?;
    }
    ctx::mkdir_user(&dest)?;
    rsync_dir_as(as_user(), &restore_opts(), &src, &dest)
}

/// Restored data is written by the target user (rsync runs through `sudo -u` when kast is
/// root), so nothing can end up root-owned, and an app restarted right after the copy
/// (plasmashell) always finds its config owned by the user.
fn as_user() -> bool {
    let c = ctx();
    c.is_root && c.target_uid != 0
}

/// rsync options for writing restored files into the home: the owner can always read and
/// write (an archived 0444 or 0400 file would otherwise stay read-only and KDE would say
/// `Configuration file ... not writable`), group and others never write.
fn restore_opts() -> Vec<String> {
    vec!["--chmod=Du+rwx,Fu+rw,go-w".to_string()]
}

/// Each entry of `src` into `dest`, leaving `dest` itself alone: `rsync -a src/ dest/` also
/// gives `dest` the owner and mode of `src`, which for ~ or ~/.config is a root-owned
/// staging folder.
fn rsync_entries(opts: &[String], src: &Path, dest: &Path) -> Result<()> {
    let mut args: Vec<&str> = vec!["-a"];
    args.extend(opts.iter().map(String::as_str));
    let dest = format!("{}/", dest.display());
    let mut entries: Vec<PathBuf> = fs::read_dir(src)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for e in entries {
        rsync_as(as_user(), &args, &e, &dest)?;
    }
    Ok(())
}

/// A command for the desktop session: through run_as_user when root.
fn user_cmd(args: &[&str]) -> Command {
    if ctx().is_root {
        run_as_user(args)
    } else {
        let mut c = Command::new(args[0]);
        c.args(&args[1..]);
        c
    }
}

fn user_ok(args: &[&str]) -> bool {
    user_cmd(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn restore_howdy(staging: &Path, rollback: &Path, manifest: &Value, o: &ImportOpts) -> Result<()> {
    let c = ctx();
    let models_src = staging.join("howdy/models");
    if !models_src.is_dir() {
        return Ok(());
    }
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore Howdy face models to: {HOWDY_MODELS_DIR}"));
        return Ok(());
    }
    if !sudo_ready() {
        warn_sudo_needed("the Howdy face models");
        return Ok(());
    }
    let mut howdy_user = jq_str(manifest, &["apps", "howdy", "user"], "");
    if howdy_user.is_empty() {
        howdy_user = c.target_user.clone();
    }
    let dest = Path::new(HOWDY_MODELS_DIR);

    if o.safety_backup && dest.is_dir() {
        let snap = rollback.join("howdy/models");
        fs::create_dir_all(&snap)?;
        sh_priv(r#"cp -p "$1"/*.dat "$2"/"#, &[HOWDY_MODELS_DIR, &snap.to_string_lossy()]);
    }

    for step in [["mkdir", "-p", HOWDY_MODELS_DIR], ["chmod", "700", HOWDY_MODELS_DIR], ["chown", "root:root", HOWDY_MODELS_DIR]] {
        if !ok_priv(step[0], &step[1..]) {
            log_error(&format!("Failed: {}", step.join(" ")));
            return Err(reported());
        }
    }
    // Only this user's model goes in, named for this account: whatever name the archive
    // used would leave a model for a user that doesn't exist here
    let own = howdy_model_path(&c.target_user);
    let mut dats: Vec<PathBuf> = fs::read_dir(&models_src)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "dat")).collect();
    dats.sort();
    let model = dats.iter().find(|p| p.file_stem().is_some_and(|s| *s == *howdy_user.as_str())).or(if dats.len() == 1 { dats.first() } else { None });
    let Some(model) = model else {
        log_warn(&format!("No face model for '{howdy_user}' in the archive; nothing installed."));
        return Ok(());
    };
    if !ok_priv("cp", &["-p", &model.to_string_lossy(), &own.to_string_lossy()]) {
        log_error(&format!("Failed to install the face model at {}", own.display()));
        return Err(reported());
    }
    let own_s = own.to_string_lossy().into_owned();
    ok_priv("chown", &["root:root", &own_s]);
    ok_priv("chmod", &["600", &own_s]);
    log_success("Howdy face models restored with permissions 0600 (root:root).");
    howdy_list();
    Ok(())
}

const SHORTCUTS_RC: &str = "kglobalshortcutsrc";

fn restore_kde(staging: &Path, rollback: &Path, o: &ImportOpts, session: &dyn KdeSession) -> Result<()> {
    let c = ctx();
    let kde = staging.join("kde");
    if !kde.is_dir() {
        log_warn("KDE snapshot folder missing in archive, skipping...");
        return Ok(());
    }
    let has_shortcuts = kde.join("config").join(SHORTCUTS_RC).is_file();
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore KDE config to: {}", c.config_home.display()));
        log_info(&format!("  [Dry-run] Would restore KDE share data to: {}", c.data_home.display()));
        log_info("  [Dry-run] Would restore ~/.fonts ~/.themes ~/.icons if present");
        if has_shortcuts {
            log_info("  [Dry-run] Would stop kglobalacceld while restoring shortcuts");
        }
        return Ok(());
    }
    if o.safety_backup {
        kde_copy_named(&c.config_home, &rollback.join("kde/config"), false, &kde_config_entries())?;
        kde_copy_named(&c.data_home, &rollback.join("kde/share"), false, KDE_SHARE_ENTRIES)?;
        kde_copy_named(&c.target_home, &rollback.join("kde/home"), false, KDE_HOME_ENTRIES)?;
    }

    // Stop plasmashell so its in-memory state doesn't overwrite the restored files on logout
    let plasma_was_running = session.shell_running();
    if plasma_was_running {
        log_info("Stopping plasmashell to prevent in-memory state from overwriting restored settings on logout...");
        session.stop_shell();
    }
    // kglobalacceld owns the global shortcuts and writes them from memory over
    // kglobalshortcutsrc whenever an app registers shortcuts with it (the apps kast starts
    // again after the restore do) and when it exits. Stopped after plasmashell, which can
    // still talk to it while it quits (that writes only the old file over itself), and
    // before the files go in. Started again below as soon as they are in, before
    // plasmashell, the other apps and the relaunch: everything that registers shortcuts
    // after this talks to a daemon that loaded the restored file. Like `kast plasma apply`.
    let shortcuts = has_shortcuts.then(|| ShortcutsStopped::stop(session));

    let opts = restore_opts();
    if kde.join("config").is_dir() {
        ctx::mkdir_user(&c.config_home)?;
        rsync_entries(&opts, &kde.join("config"), &c.config_home)?;
    }
    if kde.join("share").is_dir() {
        ctx::mkdir_user(&c.data_home)?;
        rsync_entries(&opts, &kde.join("share"), &c.data_home)?;
    }
    if kde.join("home").is_dir() {
        rsync_entries(&opts, &kde.join("home"), &c.target_home)?;
    }
    if let Some(s) = shortcuts {
        log_info("Starting kglobalacceld with the restored shortcuts...");
        s.start();
    }

    if plasma_was_running {
        log_info("Restarting plasmashell and reloading KWin settings...");
        session.start_shell();
    }
    log_success("KDE Plasma settings restored successfully!");
    log_info("Note: For a complete refresh of all panel widgets and icons, logging out and back in is recommended.");
    Ok(())
}

/// Undo a restore from its safety snapshot. App data is copied back as the user, so a
/// snapshot someone tampered with can only write where the user could anyway; the face
/// models go back to /etc/howdy as root only while the snapshot still has the fingerprint
/// the journal recorded.
pub fn undo_restore(snapshot: &Path, apps: &[String], fingerprint: Option<&str>) -> Result<()> {
    undo_with(snapshot, apps, fingerprint, &LiveSession)
}

fn undo_with(snapshot: &Path, apps: &[String], fingerprint: Option<&str>, session: &dyn Session) -> Result<()> {
    let c = ctx();
    if !snapshot.is_dir() {
        bail!("the snapshot {} is gone", snapshot.display());
    }
    let intact = fingerprint.is_some() && crate::journal::fingerprint(snapshot).as_deref() == fingerprint;
    let rsync_back = |src: String, dest: String, delete: bool| -> Result<()> {
        let mut args = vec!["rsync", "-a", "--chmod=Du+rwx,Fu+rw"];
        if delete {
            args.push("--delete");
        }
        args.extend([src.as_str(), dest.as_str()]);
        if user_cmd(&args).stdin(Stdio::null()).status().is_ok_and(|st| st.success()) { Ok(()) } else { bail!("copying {src} back failed") }
    };
    let copy = |src: &Path, dest: &Path, delete: bool| rsync_back(format!("{}/", src.display()), format!("{}/", dest.display()), delete);
    // Entry by entry, so ~ and ~/.config keep their own mode (see rsync_entries)
    let copy_entries = |src: &Path, dest: &Path| -> Result<()> {
        let mut entries: Vec<PathBuf> = fs::read_dir(src)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            rsync_back(e.display().to_string(), format!("{}/", dest.display()), false)?;
        }
        Ok(())
    };
    for app in apps {
        match app.as_str() {
            "howdy" => {
                let src = snapshot.join("howdy/models");
                if !src.is_dir() {
                    continue;
                }
                if !intact {
                    log_warn("The snapshot changed since the restore: the face models in it are not put back.");
                    continue;
                }
                sh_priv(r#"cp -p "$1"/*.dat "$2"/ && chown root:root "$2"/*.dat && chmod 600 "$2"/*.dat"#, &[&src.to_string_lossy(), HOWDY_MODELS_DIR]);
            }
            "kde" => {
                // As in restore_kde: the daemon would write its shortcuts over the old ones
                let shortcuts = snapshot.join("kde/config").join(SHORTCUTS_RC).is_file().then(|| ShortcutsStopped::stop(session));
                for (sub, dest) in [("kde/config", &c.config_home), ("kde/share", &c.data_home), ("kde/home", &c.target_home)] {
                    if snapshot.join(sub).is_dir() {
                        copy_entries(&snapshot.join(sub), dest)?;
                    }
                }
                if let Some(s) = shortcuts {
                    s.start();
                }
            }
            k if keyring::is_keyring(k) => {
                if !keyring::undo(k, snapshot, session)? {
                    continue;
                }
            }
            other => {
                if let Some(spec) = restore_spec(other) {
                    let src = snapshot.join(spec.folder);
                    if src.is_dir() {
                        copy(&src, &restore_base(other).join(spec.folder), true)?;
                    }
                }
                if let Some(dest) = share_path(other) {
                    let src = snapshot.join(format!("{other}-share"));
                    if src.is_dir() {
                        copy(&src, &dest, true)?;
                    }
                }
            }
        }
        log_success(&format!("Put back {app} from {}", snapshot.display()));
    }
    Ok(())
}

/// The files and folders restoring `apps` writes (Howdy's models live in /etc and are not
/// among them). Also what `kast fix-permissions` checks.
pub fn managed_paths(apps: &[String]) -> Vec<PathBuf> {
    let c = ctx();
    let mut v = Vec::new();
    for app in apps {
        match app.as_str() {
            "kde" => {
                v.extend(kde_config_entries().iter().map(|e| c.config_home.join(e)));
                v.extend(KDE_SHARE_ENTRIES.iter().map(|e| c.data_home.join(e)));
                v.extend(KDE_HOME_ENTRIES.iter().map(|e| c.target_home.join(e)));
            }
            "kwallet" => v.extend([keyring::kwallet_dir(), keyring::kwalletrc_path()]),
            "gnome-keyring" => v.push(keyring::gnome_dir()),
            other => {
                if let Some(spec) = restore_spec(other) {
                    v.push(restore_base(other).join(spec.folder));
                }
                if other == "zalo" {
                    v.push(c.config_home.join("zalo-flags.conf"));
                }
                v.extend(share_path(other));
            }
        }
    }
    v.retain(|p| p.symlink_metadata().is_ok());
    v
}

/// The folders a restore writes into. Their own owner and mode must stay the user's: an old
/// `rsync -a src/ dest/` copied a root-owned staging folder's owner onto them, after which
/// nothing new could be created in `~/.config`.
pub fn destination_folders() -> Vec<PathBuf> {
    let c = ctx();
    let mut v = vec![c.target_home.clone(), c.config_home.clone(), c.data_home.clone()];
    v.extend(c.data_home.parent().map(Path::to_path_buf));
    v.sort();
    v.dedup();
    v.retain(|p| p.starts_with(&c.target_home) && p.symlink_metadata().is_ok());
    v
}

/// The folders a repair may look at: the user's home and their XDG folders.
pub fn repair_jails() -> Vec<PathBuf> {
    let c = ctx();
    vec![c.target_home.clone(), c.config_home.clone(), c.data_home.clone()]
}

/// Make the extracted archive usable by the user the restore writes as: everything except
/// the Howdy face models is handed to them, and the staging folder can be entered but not
/// listed. The models stay root-only (the archive can hold every user's).
fn prepare_staging(staging: &Path) {
    if !as_user() {
        return;
    }
    let c = ctx();
    let report = |what: &str, e: &dyn std::fmt::Display| ctx::add_issue(format!("Could not prepare the extracted files ({what}): {e}"));
    if let Err(e) = chmod(staging, 0o711) {
        report("staging folder", &e);
    }
    let Ok(rd) = fs::read_dir(staging) else { return };
    for e in rd.flatten() {
        if e.file_name() == "howdy" {
            if let Err(err) = chmod(&e.path(), 0o700) {
                report("howdy", &err);
            }
            continue;
        }
        for (path, err) in crate::perms::fix_tree(&e.path(), c.target_uid, c.target_gid) {
            report(&path.display().to_string(), &err);
        }
    }
}

/// After a restore: every restored entry must belong to the user and be writable by them.
/// Fixes what it can (as root, chown), and records everything left as an issue with the repair
/// command, so a `Configuration file ... not writable` never goes unexplained.
fn verify_restore(apps: &[String], rollback: &Path) {
    let c = ctx();
    let (uid, gid) = (c.target_uid, c.target_gid);
    let jails = repair_jails();
    let mut findings = Vec::new();
    for p in destination_folders() {
        match crate::perms::scan_shallow(&jails, &p, uid, gid) {
            Ok(f) => findings.extend(f),
            Err(e) => ctx::add_issue(format!("Not checked: {e:#}")),
        }
    }
    for p in managed_paths(apps) {
        match crate::perms::scan(&jails, &p, uid, gid) {
            Ok(f) => findings.extend(f),
            Err(e) => ctx::add_issue(format!("Not checked: {e:#}")),
        }
    }
    if !findings.is_empty() {
        log_warn(&format!("{} restored file(s) are not owned by {} or not writable; repairing...", findings.len(), c.target_user));
        let r = crate::perms::repair(&findings, uid, gid, false);
        log_info(&format!("Repaired {} file(s).", r.fixed));
        for (path, why) in r.failed.iter().take(20) {
            ctx::add_issue(format!("{} is still not writable by {} ({why}). Fix: sudo kast fix-permissions", path.display(), c.target_user));
        }
        if r.failed.len() > 20 {
            ctx::add_issue(format!("... and {} more; sudo kast fix-permissions lists and fixes them all", r.failed.len() - 20));
        }
    }
    let safety = safety_backup_dir();
    if safety.is_dir() {
        if c.is_root {
            for (path, why) in crate::perms::fix_tree(&safety, uid, gid) {
                ctx::add_issue(format!("Safety snapshot {}: {why}", path.display()));
            }
        }
        // Model snapshots can contain every user's face model; keep them root-only (all of
        // them: handing the folder to the user above covered earlier snapshots too)
        let snapshots = fs::read_dir(&safety).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>()).unwrap_or_default();
        for howdy in snapshots.iter().map(|p| p.join("howdy")).chain([rollback.join("howdy")]).filter(|p| p.is_dir()) {
            for (path, why) in lock_down(&howdy) {
                ctx::add_issue(format!(
                    "Face model snapshot {} could not be locked down ({why}): run: sudo chown -R root:root {0:?} && sudo chmod -R go-rwx {0:?}",
                    path.display()
                ));
            }
        }
    }
}

/// root:root and no group/other access for a whole tree (the Howdy snapshots). Failures are
/// returned, never dropped.
fn lock_down(root: &Path) -> Vec<(PathBuf, String)> {
    use std::os::unix::fs::PermissionsExt;
    let mut failed = Vec::new();
    if !ctx().is_root {
        return failed;
    }
    for e in walkdir::WalkDir::new(root).follow_links(false).follow_root_links(false).into_iter().flatten() {
        if let Err(err) = crate::perms::lchown(e.path(), 0, 0) {
            failed.push((e.path().to_path_buf(), format!("chown: {err}")));
        }
        if !e.file_type().is_symlink()
            && let Ok(m) = e.path().metadata()
            && let Err(err) = fs::set_permissions(e.path(), fs::Permissions::from_mode(m.permissions().mode() & !0o077))
        {
            failed.push((e.path().to_path_buf(), format!("chmod: {err}")));
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restoring_entries_leaves_the_destination_folder_alone() {
        if !have("rsync") {
            eprintln!("skipped: needs rsync");
            return;
        }
        crate::ctx::init_for_tests();
        let t = tempfile::tempdir().unwrap();
        let (src, dest) = (t.path().join("src"), t.path().join("dest"));
        fs::create_dir_all(src.join("plasma")).unwrap();
        fs::write(src.join("plasmashellrc"), "[Updates]\n").unwrap();
        fs::write(src.join("plasma/x"), "x").unwrap();
        fs::create_dir(&dest).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o751)).unwrap();
        rsync_entries(&restore_opts(), &src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("plasmashellrc")).unwrap(), "[Updates]\n");
        assert!(dest.join("plasma/x").is_file());
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o751, "dest keeps its own mode");
    }

    #[test]
    fn a_read_only_archived_file_is_restored_writable_for_the_owner() {
        if !have("rsync") {
            eprintln!("skipped: needs rsync");
            return;
        }
        crate::ctx::init_for_tests();
        let t = tempfile::tempdir().unwrap();
        let (src, dest) = (t.path().join("src"), t.path().join("dest"));
        fs::create_dir_all(src.join("kxmlgui6")).unwrap();
        fs::write(src.join("spectaclerc"), "[General]\n").unwrap();
        fs::write(src.join("kxmlgui6/x"), "x").unwrap();
        fs::set_permissions(src.join("spectaclerc"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(src.join("kxmlgui6/x"), fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(src.join("kxmlgui6"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::create_dir(&dest).unwrap();
        // the old option left them read-only for their owner
        let old = t.path().join("old");
        fs::create_dir(&old).unwrap();
        rsync_entries(&["--chmod=go-w".into()], &src, &old).unwrap();
        assert_eq!(fs::metadata(old.join("spectaclerc")).unwrap().permissions().mode() & 0o200, 0);
        rsync_entries(&restore_opts(), &src, &dest).unwrap();
        let mode = |p: &str| fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("spectaclerc") & 0o600, 0o600, "{:o}", mode("spectaclerc"));
        assert_eq!(mode("spectaclerc") & 0o022, 0, "group/other never write");
        assert_eq!(mode("kxmlgui6/x") & 0o600, 0o600);
        assert_eq!(mode("kxmlgui6") & 0o700, 0o700);
    }

    #[test]
    fn verify_restore_repairs_a_read_only_config_and_reports_nothing_when_fixed() {
        let _g = crate::perms::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::ctx::init_for_tests();
        let c = ctx();
        fs::create_dir_all(&c.config_home).unwrap();
        let rc = c.config_home.join("spectaclerc");
        fs::write(&rc, "x").unwrap();
        fs::set_permissions(&rc, fs::Permissions::from_mode(0o444)).unwrap();
        ctx::clear_issues();
        verify_restore(&["kde".to_string()], &c.state_home.join("no-rollback"));
        assert_eq!(fs::metadata(&rc).unwrap().permissions().mode() & 0o600, 0o600);
        assert!(ctx::take_issues().is_empty());
        assert!(managed_paths(&["kde".to_string()]).contains(&rc));
        assert!(managed_paths(&["howdy".to_string()]).is_empty(), "face models are not user files");
    }

    #[test]
    fn home_and_share_folders_come_back_where_they_were() {
        if !have("rsync") {
            eprintln!("skipped: needs rsync");
            return;
        }
        let _g = crate::perms::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _staging = ctx::staging_test_lock();
        let c = crate::ctx::init_for_tests();
        let (scripts, history) = (c.target_home.join(".scripts"), c.data_home.join("fish/fish_history"));
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("hello.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(scripts.join("hello.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(c.config_home.join("fish")).unwrap();
        fs::write(c.config_home.join("fish/config.fish"), "set -x A 1\n").unwrap();
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, "- cmd: ls\n").unwrap();

        let t = tempfile::tempdir().unwrap();
        for app in ["scripts", "fish"] {
            let zip = t.path().join(format!("{app}.zip"));
            let o = super::super::backup::BackupOpts { target: app.into(), output: Some(zip.clone()), force: true, ..Default::default() };
            super::super::backup::cmd_backup(o).unwrap();
            assert_eq!(snapshot_paths(&[app.to_string()]).len(), if app == "fish" { 2 } else { 1 });
            for p in snapshot_paths(&[app.to_string()]) {
                fs::remove_dir_all(p).unwrap();
            }
            let o = ImportOpts { file: zip, app: None, safety_backup: false, kill: false, force: true, confirmed: true, dry_run: false, no_relaunch: true };
            cmd_import(o).unwrap();
        }
        assert_eq!(fs::metadata(scripts.join("hello.sh")).unwrap().permissions().mode() & 0o755, 0o755, "still executable");
        assert_eq!(fs::read_to_string(c.config_home.join("fish/config.fish")).unwrap(), "set -x A 1\n");
        assert_eq!(fs::read_to_string(&history).unwrap(), "- cmd: ls\n");
    }

    #[test]
    fn zotero_library_and_profile_come_back_without_attachments_on_no_media() {
        if !have("rsync") {
            eprintln!("skipped: needs rsync");
            return;
        }
        let _g = crate::perms::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _staging = ctx::staging_test_lock();
        let c = crate::ctx::init_for_tests();
        let (lib, prof) = (c.target_home.join("Zotero"), c.target_home.join(".zotero/zotero/abc.default"));
        for d in [lib.join("storage/KEY1"), lib.join("logs"), prof.join("storage/default")] {
            fs::create_dir_all(d).unwrap();
        }
        for (p, s) in [
            (lib.join("zotero.sqlite"), "db"),
            (lib.join("zotero.sqlite.1.bak"), "old"),
            (lib.join("storage/KEY1/paper.pdf"), "pdf"),
            (lib.join("logs/debug.txt"), "log"),
            (prof.join("prefs.js"), "user_pref"),
            (prof.join("storage/default/x"), "web"),
            (prof.join("lock"), ""),
        ] {
            fs::write(p, s).unwrap();
        }

        let t = tempfile::tempdir().unwrap();
        let zip = t.path().join("zotero.zip");
        let o = super::super::backup::BackupOpts { target: "zotero".into(), output: Some(zip.clone()), force: true, ..Default::default() };
        super::super::backup::cmd_backup(o).unwrap();
        assert_eq!(snapshot_paths(&["zotero".to_string()]), [lib.clone(), c.target_home.join(".zotero")]);
        for p in snapshot_paths(&["zotero".to_string()]) {
            fs::remove_dir_all(p).unwrap();
        }
        let o = ImportOpts { file: zip, app: None, safety_backup: false, kill: false, force: true, confirmed: true, dry_run: false, no_relaunch: true };
        cmd_import(o).unwrap();

        assert_eq!(fs::read_to_string(lib.join("zotero.sqlite")).unwrap(), "db");
        assert_eq!(fs::read_to_string(prof.join("prefs.js")).unwrap(), "user_pref");
        assert_eq!(fs::read_to_string(prof.join("storage/default/x")).unwrap(), "web", "only the library's storage is media");
        for gone in [lib.join("storage"), lib.join("zotero.sqlite.1.bak"), lib.join("logs"), prof.join("lock")] {
            assert!(!gone.exists(), "{} should not be backed up", gone.display());
        }
    }

    /// Records what a KDE restore does to the session, with the shortcut file as the daemon
    /// would load it and whether the scripts (restored after `kde`) are back yet.
    struct FakeSession {
        events: std::cell::RefCell<Vec<String>>,
        start_fails: bool,
        /// The keyring daemons won't stop
        keyring_stuck: bool,
        /// How the fake gnome-keyring "ran" before it was stopped
        gnome_ran: keyring::Restart,
    }

    impl FakeSession {
        fn new() -> FakeSession {
            FakeSession { events: Default::default(), start_fails: false, keyring_stuck: false, gnome_ran: keyring::Restart::default() }
        }

        /// A keyring event, with the wallet and keyring as a daemon would find them on disk.
        fn note_keyring(&self, what: &str) {
            let read = |p: PathBuf| fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_else(|_| "-".into());
            let kwl = read(keyring::kwallet_dir().join("kdewallet.kwl")).replace(KWL_HEAD, "");
            let login = read(keyring::gnome_dir().join("login.keyring")).replace(GKR_HEAD, "");
            let scripts = ctx().target_home.join(".scripts/hello.sh").is_file();
            self.events.borrow_mut().push(format!("{what} kwl={kwl} login={login} scripts={scripts}"));
        }

        fn note(&self, what: &str) {
            let c = ctx();
            let rc = fs::read_to_string(c.config_home.join(SHORTCUTS_RC)).unwrap_or_default();
            let scripts = c.target_home.join(".scripts/hello.sh").is_file();
            self.events.borrow_mut().push(format!("{what} rc={} scripts={scripts}", rc.trim()));
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }
    }

    impl KdeSession for FakeSession {
        fn shell_running(&self) -> bool {
            true
        }
        fn stop_shell(&self) {
            self.note("stop shell");
        }
        fn start_shell(&self) {
            self.note("start shell");
        }
        fn stop_shortcuts(&self) -> Result<bool, String> {
            self.note("stop shortcuts");
            Ok(true)
        }
        fn start_shortcuts(&self) -> Result<(), String> {
            self.note("start shortcuts");
            if self.start_fails { Err("could not start kglobalacceld; log out and in to load the shortcuts".into()) } else { Ok(()) }
        }
    }

    impl keyring::KeyringDaemons for FakeSession {
        fn stop_keyring(&self, store: keyring::Store) -> Result<keyring::Restart, String> {
            self.note_keyring(&format!("stop {store:?}"));
            if self.keyring_stuck {
                return Err("still running".into());
            }
            Ok(if store == keyring::Store::Gnome { self.gnome_ran.clone() } else { keyring::Restart::default() })
        }

        fn start_keyring(&self, store: keyring::Store, how: &keyring::Restart) -> Result<(), String> {
            self.note_keyring(&format!("start {store:?} units={:?} argv={:?}", how.units, how.argv));
            Ok(())
        }
    }

    const SAVED: &str = "[kwin]\nLock Session=Meta+Shift+L,Meta+L,Lock Session";
    const DEFAULT: &str = "[kwin]\nLock Session=Meta+L,Meta+L,Lock Session";

    /// A backup of `kde` (with shortcuts) and `scripts`, which comes after `kde`, taken in the
    /// test home; the home then holds KDE's default shortcuts and no scripts. Removes what it
    /// wrote when dropped.
    struct KdeArchive {
        zip: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl KdeArchive {
        fn new() -> KdeArchive {
            let c = ctx();
            let rc = c.config_home.join(SHORTCUTS_RC);
            fs::create_dir_all(&c.config_home).unwrap();
            fs::write(c.config_home.join("kdeglobals"), "[General]\n").unwrap();
            fs::write(&rc, SAVED).unwrap();
            let scripts = c.target_home.join(".scripts");
            fs::create_dir_all(&scripts).unwrap();
            fs::write(scripts.join("hello.sh"), "echo hi\n").unwrap();
            let dir = tempfile::tempdir().unwrap();
            let zip = dir.path().join("all.zip");
            let o = super::super::backup::BackupOpts {
                target: "all".into(),
                output: Some(zip.clone()),
                force: true,
                no_howdy: true,
                no_relaunch: true,
                running: super::super::backup::RunningPolicy::Skip,
                ..Default::default()
            };
            super::super::backup::cmd_backup(o).unwrap();
            fs::write(&rc, DEFAULT).unwrap();
            fs::remove_dir_all(&scripts).unwrap();
            KdeArchive { zip, _dir: dir }
        }

        fn opts(&self, app: Option<&str>) -> ImportOpts {
            ImportOpts {
                file: self.zip.clone(),
                app: app.map(String::from),
                safety_backup: false,
                kill: false,
                force: true,
                confirmed: true,
                dry_run: false,
                no_relaunch: true,
            }
        }

        fn import(&self, app: Option<&str>, session: &FakeSession) -> Result<()> {
            import_with(self.opts(app), session)
        }
    }

    impl Drop for KdeArchive {
        fn drop(&mut self) {
            let c = ctx();
            for f in [SHORTCUTS_RC, "kdeglobals"] {
                let p = c.config_home.join(f);
                let _ = fs::remove_file(&p).or_else(|_| fs::remove_dir_all(&p));
            }
            let p = c.target_home.join(".scripts");
            let _ = fs::remove_dir_all(&p).or_else(|_| fs::remove_file(&p));
        }
    }

    fn kde_test_lock() -> Option<(std::sync::MutexGuard<'static, ()>, std::sync::MutexGuard<'static, ()>)> {
        if !have("rsync") {
            eprintln!("skipped: needs rsync");
            return None;
        }
        let home = crate::perms::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let staging = ctx::staging_test_lock();
        crate::ctx::init_for_tests();
        Some((home, staging))
    }

    #[test]
    fn the_shortcut_daemon_is_stopped_before_the_files_go_in_and_started_before_later_apps() {
        let Some(_locks) = kde_test_lock() else { return };
        let archive = KdeArchive::new();
        let session = FakeSession::new();
        archive.import(None, &session).unwrap();
        let (saved, default) = (SAVED, DEFAULT);
        assert_eq!(
            session.events(),
            [
                format!("stop shell rc={default} scripts=false"),
                format!("stop shortcuts rc={default} scripts=false"),
                // the file is in, the later app (and so the relaunch, after every app) is not
                format!("start shortcuts rc={saved} scripts=false"),
                format!("start shell rc={saved} scripts=false"),
            ]
        );
        assert!(ctx().target_home.join(".scripts/hello.sh").is_file());
    }

    #[test]
    fn all_and_kde_only_restore_the_same_shortcuts() {
        let Some(_locks) = kde_test_lock() else { return };
        let archive = KdeArchive::new();
        let rc = ctx().config_home.join(SHORTCUTS_RC);
        let mut got = Vec::new();
        for app in [None, Some("kde")] {
            fs::write(&rc, DEFAULT).unwrap();
            let session = FakeSession::new();
            archive.import(app, &session).unwrap();
            assert_eq!(session.events().iter().filter(|e| e.contains("shortcuts")).count(), 2, "{app:?}: {:?}", session.events());
            got.push(fs::read_to_string(&rc).unwrap());
        }
        assert_eq!(got[0], got[1]);
        assert_eq!(got[0], SAVED);
    }

    #[test]
    fn the_shortcut_daemon_runs_again_when_a_later_app_fails() {
        let Some(_locks) = kde_test_lock() else { return };
        let archive = KdeArchive::new();
        // a file where the scripts folder goes: restoring `scripts` fails (even as root)
        fs::write(ctx().target_home.join(".scripts"), "").unwrap();
        let session = FakeSession::new();
        assert!(archive.import(None, &session).is_err());
        let events = session.events();
        assert_eq!(events.iter().filter(|e| e.starts_with("start shortcuts")).count(), 1, "{events:?}");
        assert!(events.iter().any(|e| e.starts_with("start shortcuts") && e.contains(SAVED)), "{events:?}");
    }

    #[test]
    fn the_shortcut_daemon_runs_again_when_the_kde_restore_itself_fails() {
        let Some(_locks) = kde_test_lock() else { return };
        let archive = KdeArchive::new();
        // a non-empty folder where the shortcut file goes: rsync can't replace it
        let rc = ctx().config_home.join(SHORTCUTS_RC);
        fs::remove_file(&rc).unwrap();
        fs::create_dir_all(rc.join("x")).unwrap();
        let session = FakeSession { start_fails: true, ..FakeSession::new() };
        assert!(archive.import(Some("kde"), &session).is_err());
        let events = session.events();
        assert!(events.iter().any(|e| e.starts_with("stop shortcuts")), "{events:?}");
        assert_eq!(events.iter().filter(|e| e.starts_with("start shortcuts")).count(), 1, "started once, by the guard: {events:?}");
    }

    #[test]
    fn a_kde_restore_without_shortcuts_leaves_the_daemon_alone() {
        let Some(_locks) = kde_test_lock() else { return };
        let c = ctx();
        let archive = KdeArchive::new();
        // the same archive, minus the shortcut file
        let t = tempfile::tempdir().unwrap();
        extract_zip_archive(&archive.zip, t.path()).unwrap();
        fs::remove_file(t.path().join("kde/config").join(SHORTCUTS_RC)).unwrap();
        let session = FakeSession::new();
        restore_kde(t.path(), &c.state_home.join("no-rollback"), &archive.opts(Some("kde")), &session).unwrap();
        assert!(session.events().iter().all(|e| !e.contains("shortcuts")), "{:?}", session.events());
    }

    // A wallet and a keyring as their headers make them look (the rest is a marker the
    // tests follow; never real secrets)
    const KWL_HEAD: &str = "KWALLET\n\r\0\r\n\0\u{1}\u{3}\u{2}";
    const GKR_HEAD: &str = "GnomeKeyring\n\r\0\n\0\0\0\0";

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Wallets and keyrings in the test home; removed when dropped.
    struct Keyrings;

    impl Keyrings {
        fn new() -> Keyrings {
            let (kw, gk) = (keyring::kwallet_dir(), keyring::gnome_dir());
            for d in [&kw, &gk] {
                let _ = fs::remove_dir_all(d);
                fs::create_dir_all(d).unwrap();
            }
            fs::set_permissions(&kw, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(kw.join("kdewallet.kwl"), format!("{KWL_HEAD}SAVED-WALLET")).unwrap();
            fs::write(kw.join("kdewallet.salt"), "S".repeat(56)).unwrap();
            fs::write(kw.join("kdewallet_attributes.json"), "{}").unwrap();
            fs::write(kw.join("kdewallet.kwl~"), "old copy").unwrap();
            fs::write(gk.join("login.keyring"), format!("{GKR_HEAD}SAVED-LOGIN")).unwrap();
            fs::write(gk.join("default"), "login").unwrap();
            fs::write(gk.join("user.keystore"), "").unwrap();
            fs::write(gk.join("Plain.keyring"), "[keyring]\ndisplay-name=Plain\n\n[1]\nsecret=PLAINTEXT-SECRET\n").unwrap();
            std::os::unix::fs::symlink("/etc/passwd", gk.join("evil.keyring")).unwrap();
            for d in [&kw, &gk] {
                for e in fs::read_dir(d).unwrap().flatten().filter(|e| !e.path().is_symlink()) {
                    fs::set_permissions(e.path(), fs::Permissions::from_mode(0o600)).unwrap();
                }
            }
            fs::create_dir_all(&ctx().config_home).unwrap();
            fs::write(keyring::kwalletrc_path(), "[Wallet]\nDefault Wallet=kdewallet\nFirst Use=false\nEnabled=true\n").unwrap();
            Keyrings
        }

        fn backup(&self, target: &str, o: super::super::backup::BackupOpts) -> (PathBuf, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let zip = dir.path().join("k.zip");
            let o = super::super::backup::BackupOpts { target: target.into(), output: Some(zip.clone()), force: true, no_relaunch: true, ..o };
            super::super::backup::cmd_backup(o).unwrap();
            (zip, dir)
        }
    }

    impl Drop for Keyrings {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(keyring::kwallet_dir());
            let _ = fs::remove_dir_all(keyring::gnome_dir());
            let _ = fs::remove_file(keyring::kwalletrc_path());
            let _ = fs::remove_dir_all(ctx().target_home.join(".scripts"));
        }
    }

    fn import_opts(zip: &Path, app: Option<&str>, safety_backup: bool) -> ImportOpts {
        ImportOpts {
            file: zip.to_path_buf(),
            app: app.map(String::from),
            safety_backup,
            kill: false,
            force: true,
            confirmed: true,
            dry_run: false,
            no_relaunch: true,
        }
    }

    #[test]
    fn keyrings_back_up_exactly_their_files_and_no_contents_in_the_manifest() {
        let Some(_locks) = kde_test_lock() else { return };
        let _k = Keyrings::new();
        ctx::clear_issues();
        let (zip, _d) = _k.backup("keyrings", Default::default());
        let issues = ctx::take_issues();
        assert!(issues.iter().any(|i| i.contains("'Plain'") && i.contains("--allow-plaintext-keyring")), "{issues:?}");
        let t = tempfile::tempdir().unwrap();
        extract_zip_archive(&zip, t.path()).unwrap();
        let mut got: Vec<String> = walkdir::WalkDir::new(t.path())
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().strip_prefix(t.path()).unwrap().display().to_string())
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "backup_manifest.json",
                "gnome-keyring/keyrings/default",
                "gnome-keyring/keyrings/login.keyring",
                "gnome-keyring/keyrings/user.keystore",
                "kwallet/kwalletd/kdewallet.kwl",
                "kwallet/kwalletd/kdewallet.salt",
                "kwallet/kwalletd/kdewallet_attributes.json",
                "kwallet/kwalletrc",
            ]
        );
        for f in got.iter().filter(|f| f.contains('/')) {
            assert_eq!(mode(&t.path().join(f)), 0o600, "{f}");
        }
        for d in ["kwallet", "kwallet/kwalletd", "gnome-keyring/keyrings"] {
            assert_eq!(mode(&t.path().join(d)), 0o700, "{d}");
        }
        let m = fs::read_to_string(t.path().join("backup_manifest.json")).unwrap();
        for secret in ["SAVED-WALLET", "SAVED-LOGIN", "PLAINTEXT-SECRET", "SSSS"] {
            assert!(!m.contains(secret), "the manifest holds {secret}: {m}");
        }
        let v: Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["apps"]["kwallet"]["keyrings"][0]["type"], "blowfish-pbkdf2");
        assert_eq!(v["apps"]["kwallet"]["keyrings"][0]["salt"], true);
        assert_eq!(v["apps"]["gnome-keyring"]["keyrings"][0]["name"], "login");
        assert_eq!(v["apps"]["gnome-keyring"]["default"], "login");
        assert_eq!(v["apps"]["gnome-keyring"]["left_out"], serde_json::json!(["Plain.keyring"]));
    }

    #[test]
    fn plaintext_keyrings_need_the_flag_and_never_go_up() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let has_plain = |zip: &Path| {
            let t = tempfile::tempdir().unwrap();
            extract_zip_archive(zip, t.path()).unwrap();
            t.path().join("gnome-keyring/keyrings/Plain.keyring").is_file()
        };
        let allowed = super::super::backup::BackupOpts { allow_plaintext_keyring: true, ..Default::default() };
        let (zip, _d) = k.backup("gnome-keyring", allowed.clone());
        assert!(has_plain(&zip), "a local backup with --allow-plaintext-keyring carries it");
        // Built for Proton Drive (or the schedule): never, flag or not
        let (zip, _d2) = k.backup("gnome-keyring", super::super::backup::BackupOpts { upload: true, ..allowed });
        assert!(!has_plain(&zip));
        let (zip, _d3) = k.backup("gnome-keyring", Default::default());
        assert!(!has_plain(&zip));
    }

    #[test]
    fn all_takes_the_keyrings_only_when_asked() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let apps = |zip: &Path| -> Vec<String> {
            let v: Value = serde_json::from_str(&read_zip_manifest(zip).unwrap()).unwrap();
            v["apps"].as_object().unwrap().keys().cloned().collect()
        };
        let base = super::super::backup::BackupOpts { no_howdy: true, running: super::super::backup::RunningPolicy::Skip, ..Default::default() };
        let (zip, _d) = k.backup("all", base.clone());
        assert!(!apps(&zip).iter().any(|a| keyring::is_keyring(a)), "{:?}", apps(&zip));
        let (zip, _d2) = k.backup("all", super::super::backup::BackupOpts { with_keyrings: true, ..base.clone() });
        assert!(apps(&zip).contains(&"kwallet".to_string()) && apps(&zip).contains(&"gnome-keyring".to_string()));
        // The setting does the same, for `all` (and so the schedule)
        crate::settings::write_own(&crate::settings::path(), b"[vault]\nkeyrings = true\n", 0o600).unwrap();
        let (zip, _d3) = k.backup("all", base);
        let _ = fs::remove_file(crate::settings::path());
        assert!(apps(&zip).contains(&"kwallet".to_string()));
    }

    #[test]
    fn keyrings_go_in_first_with_their_daemon_stopped() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let c = ctx();
        let scripts = c.target_home.join(".scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("hello.sh"), "echo hi\n").unwrap();
        // `scripts` sorts before both keyrings, and is restored after them
        let (keys, _d) = k.backup("keyrings", Default::default());
        let (script_zip, _d2) = k.backup("scripts", Default::default());
        let t = tempfile::tempdir().unwrap();
        extract_zip_archive(&keys, t.path()).unwrap();
        extract_zip_archive(&script_zip, t.path()).unwrap();
        let mut m: Value = serde_json::from_str(&read_zip_manifest(&keys).unwrap()).unwrap();
        let sm: Value = serde_json::from_str(&read_zip_manifest(&script_zip).unwrap()).unwrap();
        m["apps"]["scripts"] = sm["apps"]["scripts"].clone();
        fs::write(t.path().join(super::super::archive::MANIFEST), m.to_string()).unwrap();
        let both = t.path().join("both.zip");
        let src = tempfile::tempdir().unwrap();
        crate::vault::rsync_dir(&[], t.path(), src.path()).unwrap();
        super::super::archive::create_zip_archive(src.path(), &both).unwrap();

        // A fresh install: PAM's first login made an empty wallet, no login keyring yet, and
        // the first-run wizard is armed. A wallet of the new system's own stays.
        let (kw, gk) = (keyring::kwallet_dir(), keyring::gnome_dir());
        fs::write(kw.join("kdewallet.kwl"), format!("{KWL_HEAD}FRESH")).unwrap();
        fs::write(kw.join("work.kwl"), format!("{KWL_HEAD}WORK")).unwrap();
        fs::remove_file(gk.join("login.keyring")).unwrap();
        fs::write(keyring::kwalletrc_path(), "[Wallet]\nFirst Use=true\n").unwrap();
        fs::remove_dir_all(&scripts).unwrap();

        let session = FakeSession {
            gnome_ran: keyring::Restart { units: vec!["gnome-keyring-daemon.socket".into(), "gnome-keyring-daemon.service".into()], argv: None },
            ..FakeSession::new()
        };
        import_with(import_opts(&both, None, true), &session).unwrap();
        let events = session.events();
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(events[0].starts_with("stop Gnome kwl=FRESH login=- scripts=false"), "{events:?}");
        assert!(
            events[1].starts_with("start Gnome units=[\"gnome-keyring-daemon.socket\", \"gnome-keyring-daemon.service\"] argv=None")
                && events[1].contains("login=SAVED-LOGIN"),
            "{events:?}"
        );
        assert!(events[2].starts_with("stop KWallet") && events[2].contains("kwl=FRESH"), "{events:?}");
        assert!(events[3].starts_with("start KWallet") && events[3].contains("kwl=SAVED-WALLET") && events[3].ends_with("scripts=false"), "{events:?}");
        assert!(scripts.join("hello.sh").is_file(), "scripts came after");

        assert_eq!(fs::read_to_string(kw.join("work.kwl")).unwrap(), format!("{KWL_HEAD}WORK"), "unrelated wallets stay");
        for f in ["kdewallet.kwl", "kdewallet.salt", "kdewallet_attributes.json"] {
            assert_eq!(mode(&kw.join(f)), 0o600, "{f}");
        }
        assert_eq!(mode(&gk.join("login.keyring")), 0o600);
        assert_eq!((mode(&kw), mode(&gk)), (0o700, 0o700));
        assert!(
            !gk.join("Plain.keyring").exists() || fs::read_to_string(gk.join("Plain.keyring")).unwrap().contains("PLAINTEXT"),
            "never added from the archive"
        );
        let rc = fs::read_to_string(keyring::kwalletrc_path()).unwrap();
        assert_eq!(keyring::ini_get(&rc, "Wallet", "First Use").as_deref(), Some("false"), "{rc}");
        assert_eq!(keyring::ini_get(&rc, "Wallet", "Enabled").as_deref(), Some("true"));
        assert_eq!(keyring::ini_get(&rc, "Wallet", "Default Wallet").as_deref(), Some("kdewallet"), "the archive's kwalletrc");

        // The safety snapshot holds what was replaced; undo puts exactly that back
        let snap = fs::read_dir(safety_backup_dir()).unwrap().flatten().map(|e| e.path()).max().unwrap();
        assert_eq!(fs::read_to_string(snap.join("kwallet/kwalletd/kdewallet.kwl")).unwrap(), format!("{KWL_HEAD}FRESH"));
        assert!(snap.join("kwallet/kwalletrc").is_file());
        let undo = FakeSession::new();
        undo_with(&snap, &["gnome-keyring".to_string(), "kwallet".to_string()], None, &undo).unwrap();
        assert_eq!(fs::read_to_string(kw.join("kdewallet.kwl")).unwrap(), format!("{KWL_HEAD}FRESH"));
        assert!(!gk.join("login.keyring").exists(), "the restore added it; undo takes it out again");
        assert_eq!(fs::read_to_string(keyring::kwalletrc_path()).unwrap(), "[Wallet]\nFirst Use=true\n");
        let ev = undo.events();
        assert_eq!(ev.len(), 4, "{ev:?}");
        assert!(ev[0].starts_with("stop Gnome") && ev[0].contains("login=SAVED-LOGIN"), "stopped before the copy: {ev:?}");
        assert!(ev[2].starts_with("stop KWallet") && ev[2].contains("kwl=SAVED-WALLET"), "{ev:?}");
        assert!(ev[3].starts_with("start KWallet") && ev[3].contains("kwl=FRESH"), "{ev:?}");
        let _ = fs::remove_dir_all(safety_backup_dir());
    }

    #[test]
    fn a_keyring_daemon_that_wont_stop_aborts_before_any_write() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let (zip, _d) = k.backup("kwallet", Default::default());
        fs::write(keyring::kwallet_dir().join("kdewallet.kwl"), format!("{KWL_HEAD}FRESH")).unwrap();
        let session = FakeSession { keyring_stuck: true, ..FakeSession::new() };
        assert!(import_with(import_opts(&zip, None, false), &session).is_err());
        assert_eq!(fs::read_to_string(keyring::kwallet_dir().join("kdewallet.kwl")).unwrap(), format!("{KWL_HEAD}FRESH"), "nothing written under it");
        assert_eq!(session.events().len(), 1, "never started: it never stopped {:?}", session.events());
    }

    #[test]
    fn a_dry_run_leaves_the_keyring_daemons_alone() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let (zip, _d) = k.backup("keyrings", Default::default());
        fs::write(keyring::kwallet_dir().join("kdewallet.kwl"), "FRESH").unwrap();
        let session = FakeSession::new();
        import_with(ImportOpts { dry_run: true, ..import_opts(&zip, Some("keyrings"), true) }, &session).unwrap();
        assert!(session.events().is_empty(), "{:?}", session.events());
        assert_eq!(fs::read_to_string(keyring::kwallet_dir().join("kdewallet.kwl")).unwrap(), "FRESH");
    }

    #[test]
    fn the_wallets_kwalletrc_wins_over_the_kde_copy() {
        let Some(_locks) = kde_test_lock() else { return };
        let k = Keyrings::new();
        let c = ctx();
        fs::write(c.config_home.join("kdeglobals"), "[General]\n").unwrap();
        let (zip, _d) = k.backup("all", super::super::backup::BackupOpts { with_keyrings: true, no_howdy: true, ..Default::default() });
        // The kde copy says First Use=true: it must not end up in the home
        let t = tempfile::tempdir().unwrap();
        extract_zip_archive(&zip, t.path()).unwrap();
        fs::write(t.path().join("kde/config/kwalletrc"), "[Wallet]\nFirst Use=true\nDefault Wallet=other\n").unwrap();
        let fixed = t.path().join("x.zip");
        let src = tempfile::tempdir().unwrap();
        crate::vault::rsync_dir(&[], t.path(), src.path()).unwrap();
        super::super::archive::create_zip_archive(src.path(), &fixed).unwrap();
        import_with(import_opts(&fixed, None, false), &FakeSession::new()).unwrap();
        let rc = fs::read_to_string(keyring::kwalletrc_path()).unwrap();
        assert_eq!(keyring::ini_get(&rc, "Wallet", "Default Wallet").as_deref(), Some("kdewallet"), "{rc}");
        assert_eq!(keyring::ini_get(&rc, "Wallet", "First Use").as_deref(), Some("false"));
        let _ = fs::remove_file(c.config_home.join("kdeglobals"));
    }

    #[test]
    fn keyrings_are_restored_before_the_apps_that_use_them() {
        let mut apps: Vec<String> = ["beeper", "fdm", "gnome-keyring", "helium", "kde", "kwallet", "zalo"].map(String::from).to_vec();
        keyrings_first(&mut apps);
        assert_eq!(apps, ["gnome-keyring", "kwallet", "beeper", "fdm", "helium", "kde", "zalo"]);
        assert!(restore_destinations("kwallet").iter().any(|d| d.contains("kwalletd")), "the overwrite confirmation names them");
        assert!(restore_destinations("gnome-keyring").iter().any(|d| d.contains("keyrings")));
    }

    #[test]
    fn undoing_a_kde_restore_stops_the_shortcut_daemon_around_the_copy() {
        let Some(_locks) = kde_test_lock() else { return };
        let c = ctx();
        let _archive = KdeArchive::new();
        let snap = tempfile::tempdir().unwrap();
        fs::create_dir_all(snap.path().join("kde/config")).unwrap();
        fs::write(snap.path().join("kde/config").join(SHORTCUTS_RC), SAVED).unwrap();
        let session = FakeSession::new();
        undo_with(snap.path(), &["kde".to_string()], None, &session).unwrap();
        assert_eq!(fs::read_to_string(c.config_home.join(SHORTCUTS_RC)).unwrap(), SAVED);
        let events = session.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].starts_with("stop shortcuts") && events[0].contains(DEFAULT), "{events:?}");
        assert!(events[1].starts_with("start shortcuts") && events[1].contains(SAVED), "{events:?}");
    }
}
