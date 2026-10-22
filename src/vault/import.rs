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
        other => match restore_spec(other) {
            Some(spec) => {
                let mut v = vec![format!("{cfg}/{}", spec.folder)];
                if other == "zalo" {
                    v.push(format!("{cfg}/zalo-flags.conf"));
                }
                if let Some(share) = share_folder(other) {
                    v.push(format!("{}/{share}", c.data_home.display()));
                }
                v
            }
            None => vec![],
        },
    }
}

/// What a restore of `apps` would overwrite that exists now (what its safety snapshot copies).
pub fn snapshot_paths(apps: &[String]) -> Vec<PathBuf> {
    let c = ctx();
    let mut out = Vec::new();
    for app in apps {
        match app.as_str() {
            "howdy" => out.push(PathBuf::from(HOWDY_MODELS_DIR)),
            "kde" => out.extend(crate::vault::kde_existing_paths()),
            other => {
                if let Some(spec) = restore_spec(other) {
                    out.push(c.config_home.join(spec.folder));
                }
                out.extend(share_folder(other).map(|f| c.data_home.join(f)));
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

pub fn cmd_import(o: ImportOpts) -> Result<()> {
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
        if !report::ask(&Prompt::yes_no("Are you sure you want to proceed?", false)) {
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
        let detect: [(&str, &[&str]); 14] = [
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
            ("howdy", &["howdy"]),
            ("kde", &["kde"]),
            ("hypr", &["hypr"]),
        ];
        apps = detect.iter().filter(|(_, n)| has(n)).map(|(a, _)| a.to_string()).collect();
        log_info(&format!("Auto-detected apps in archive: {C_BOLD}{}{C_RESET}", apps.join(" ")));
    }

    rehome::rehome_staging(&staging, &apps, &manifest, o.dry_run);

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

    for app in &apps {
        log_step(&format!("Restoring: {C_BOLD}{app}{C_RESET}"));
        match app.as_str() {
            "howdy" => restore_howdy(&staging, &rollback, &manifest, &o)?,
            "kde" => restore_kde(&staging, &rollback, &o)?,
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

fn restore_app_dir(app: &str, spec: &RestoreSpec, staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let c = ctx();
    restore_share_dir(app, staging, rollback, o)?;
    let Some(src) = spec.sources.iter().map(|s| staging.join(s)).find(|p| p.is_dir()) else { return Ok(()) };
    let dest = c.config_home.join(spec.folder);
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

/// The app's folder under ~/.local/share (`share_folder`), archived as `<app>-share`.
fn restore_share_dir(app: &str, staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let Some(folder) = share_folder(app) else { return Ok(()) };
    let src = staging.join(format!("{app}-share"));
    if !src.is_dir() {
        return Ok(());
    }
    let dest = ctx().data_home.join(folder);
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

fn restore_kde(staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let c = ctx();
    let kde = staging.join("kde");
    if !kde.is_dir() {
        log_warn("KDE snapshot folder missing in archive, skipping...");
        return Ok(());
    }
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore KDE config to: {}", c.config_home.display()));
        log_info(&format!("  [Dry-run] Would restore KDE share data to: {}", c.data_home.display()));
        log_info("  [Dry-run] Would restore ~/.fonts ~/.themes ~/.icons if present");
        return Ok(());
    }
    if o.safety_backup {
        kde_copy_named(&c.config_home, &rollback.join("kde/config"), false, &kde_config_entries())?;
        kde_copy_named(&c.data_home, &rollback.join("kde/share"), false, KDE_SHARE_ENTRIES)?;
        kde_copy_named(&c.target_home, &rollback.join("kde/home"), false, KDE_HOME_ENTRIES)?;
    }

    // Stop plasmashell so its in-memory state doesn't overwrite the restored files on logout
    let plasma_was_running = process_running("plasmashell", Some(&c.target_user));
    if plasma_was_running {
        log_info("Stopping plasmashell to prevent in-memory state from overwriting restored settings on logout...");
        let _ = user_ok(&["systemctl", "--user", "stop", "plasma-plasmashell.service"]) || user_ok(&["kquitapp6", "plasmashell"]);
        crate::ui::sleep_secs(1.0);
    }

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

    if plasma_was_running {
        log_info("Restarting plasmashell and reloading KWin settings...");
        if !user_ok(&["systemctl", "--user", "start", "plasma-plasmashell.service"]) {
            let _ = user_cmd(&["kstart", "plasmashell"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
        user_ok(&["qdbus6", "org.kde.KWin", "/KWin", "reconfigure"]);
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
                for (sub, dest) in [("kde/config", &c.config_home), ("kde/share", &c.data_home), ("kde/home", &c.target_home)] {
                    if snapshot.join(sub).is_dir() {
                        copy_entries(&snapshot.join(sub), dest)?;
                    }
                }
            }
            other => {
                if let Some(spec) = restore_spec(other) {
                    let src = snapshot.join(spec.folder);
                    if src.is_dir() {
                        copy(&src, &c.config_home.join(spec.folder), true)?;
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
            other => {
                if let Some(spec) = restore_spec(other) {
                    v.push(c.config_home.join(spec.folder));
                }
                if other == "zalo" {
                    v.push(c.config_home.join("zalo-flags.conf"));
                }
                v.extend(share_folder(other).map(|f| c.data_home.join(f)));
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
}
