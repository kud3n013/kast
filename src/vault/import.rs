use std::fs;
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

#[derive(Debug, Clone)]
pub struct ImportOpts {
    pub file: PathBuf,
    pub app: Option<String>,
    pub safety_backup: bool,
    pub kill: bool,
    pub force: bool,
    pub dry_run: bool,
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
        "obsidian" => s(&["obsidian", "obsidian/config"], "obsidian", "Obsidian config"),
        "fcitx5" => s(&["fcitx5", "fcitx5/config"], "fcitx5", "Fcitx5 config"),
        "antigravity" => s(&["Antigravity", "antigravity/config", "antigravity"], "Antigravity", "Antigravity config"),
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
                v
            }
            None => vec![],
        },
    }
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
    let _ = ok_priv_inherit(&["howdy", "-U", user, "list"]) || ok_priv_inherit(&["howdy", "list"]);
}

/// Like ok_priv, but the output goes to the terminal (to the log while a TUI owns it).
fn ok_priv_inherit(args: &[&str]) -> bool {
    let mut cmd = if ctx().is_root {
        let mut c = Command::new(args[0]);
        c.args(&args[1..]);
        c
    } else {
        let mut c = Command::new("sudo");
        c.args(args);
        c
    };
    if report::owns_terminal() {
        cmd.stdin(Stdio::null()).stdout(ctx::log_stdio());
    }
    cmd.stderr(Stdio::null()).status().is_ok_and(|s| s.success())
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
    if !o.force {
        report::blank();
        log_warn(&format!("Restoring will install this face model for user '{user}' at {}.", dest_file.display()));
        if !report::ask(&Prompt::yes_no("Proceed with face model import?", false)) {
            log_info("Import operation canceled.");
            ctx::set_canceled(true);
            return Ok(());
        }
    }

    let dest_s = dest_file.to_string_lossy().into_owned();
    if o.safety_backup && (dest_file.is_file() || (!c.is_root && have("sudo") && ok("sudo", &["test", "-f", &dest_s]))) {
        let rollback = safety_backup_dir().join(format!("safety_rollback_{}", ctx::now_str("%Y-%m-%d_%H%M%S")));
        let models = rollback.join("howdy/models");
        ctx::mkdir_user(&models)?;
        ok_priv("cp", &["-p", &dest_s, &format!("{}/", models.display())]);
        log_info(&format!("Safety snapshot of existing model saved to: {}", rollback.display()));
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
    if !running.is_empty() {
        let list = running.join(" ");
        if o.dry_run {
            log_warn(&format!("Notice: Running applications detected ({list}). During a live restore, they will need to be closed."));
        } else if o.kill {
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
                if !terminate_apps(&running) {
                    log_error("Could not terminate running applications. Restore aborted.");
                    return Err(reported());
                }
            } else {
                log_error("Cannot safely restore files while target applications are running. Please close them and re-run.");
                return Err(reported());
            }
        }
    }

    if !o.force && !o.dry_run {
        report::blank();
        log_warn("Restoring will OVERWRITE existing application settings, login credentials, and databases!");
        if !report::ask(&Prompt::yes_no("Are you sure you want to proceed?", false)) {
            log_info("Restore operation canceled.");
            ctx::set_canceled(true);
            return Ok(());
        }
    }

    let staging = get_safe_staging_dir("app-vault-sync-restore")?;
    ctx::set_staging(staging.clone());
    log_step("Extracting archive to temporary workspace...");
    extract_zip_archive(&o.file, &staging)?;

    if apps.is_empty() {
        let has = |names: &[&str]| names.iter().any(|n| staging.join(n).is_dir());
        let detect: [(&str, &[&str]); 10] = [
            ("beeper", &["BeeperTexts", "beeper"]),
            ("zalo", &["ZaloData", "zalo"]),
            ("vesktop", &["vesktop"]),
            ("zen", &["zen"]),
            ("obsidian", &["obsidian"]),
            ("fcitx5", &["fcitx5"]),
            ("antigravity", &["Antigravity", "antigravity"]),
            ("howdy", &["howdy"]),
            ("kde", &["kde"]),
            ("hypr", &["hypr"]),
        ];
        apps = detect.iter().filter(|(_, n)| has(n)).map(|(a, _)| a.to_string()).collect();
        log_info(&format!("Auto-detected apps in archive: {C_BOLD}{}{C_RESET}", apps.join(" ")));
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

    hand_back_ownership(&apps, &rollback);
    ctx::cleanup_vault_staging();
    report::blank();
    log_success("Restore process finished successfully!");
    if o.safety_backup && rollback.is_dir() {
        log_info(&format!("Safety rollback snapshot created at: {}", rollback.display()));
    }
    Ok(())
}

fn restore_app_dir(app: &str, spec: &RestoreSpec, staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let c = ctx();
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
    fs::create_dir_all(&dest)?;
    rsync_dir(&["--chmod=go-w".into()], &src, &dest)?;
    chmod(&dest, 0o700)?;

    if app == "zalo" {
        let flags_dest = c.config_home.join("zalo-flags.conf");
        if let Some(f) = ["zalo-flags.conf", "zalo/extra/zalo-flags.conf"].iter().map(|f| staging.join(f)).find(|p| p.is_file())
            && !ok("cp", &["-p", &f.to_string_lossy(), &flags_dest.to_string_lossy()])
        {
            bail!("copying {} failed", f.display());
        }
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
    let mut howdy_user = jq_str(manifest, &["apps", "howdy", "user"], "");
    if howdy_user.is_empty() {
        howdy_user = c.target_user.clone();
    }
    let dest = Path::new(HOWDY_MODELS_DIR);
    let src_s = models_src.to_string_lossy().into_owned();

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
    sh_priv(r#"cp -p "$1"/*.dat "$2"/"#, &[&src_s, HOWDY_MODELS_DIR]);
    // A model saved under another user name is installed for the current target user
    let renamed = models_src.join(format!("{howdy_user}.dat"));
    let own = howdy_model_path(&c.target_user);
    if renamed.is_file() && (!c.is_root || !own.is_file()) {
        ok_priv("cp", &["-p", &renamed.to_string_lossy(), &own.to_string_lossy()]);
    }
    sh_priv(r#"chown root:root "$1"/*.dat; chmod 600 "$1"/*.dat"#, &[HOWDY_MODELS_DIR]);
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

    let go_w = ["--chmod=go-w".to_string()];
    if kde.join("config").is_dir() {
        ctx::mkdir_user(&c.config_home)?;
        rsync_dir(&go_w, &kde.join("config"), &c.config_home)?;
    }
    if kde.join("share").is_dir() {
        ctx::mkdir_user(&c.data_home)?;
        rsync_dir(&go_w, &kde.join("share"), &c.data_home)?;
    }
    if kde.join("home").is_dir() {
        rsync_dir(&go_w, &kde.join("home"), &c.target_home)?;
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

/// Restored files were written as root; give them (and the safety snapshots) back to the user.
fn hand_back_ownership(apps: &[String], rollback: &Path) {
    let c = ctx();
    let env_id = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (mut uid, mut gid) = (env_id("SUDO_UID"), env_id("SUDO_GID"));
    if uid.is_none() && c.target_uid != 0 {
        uid = Some(c.target_uid.to_string());
        gid = Some(c.target_gid.to_string());
    }
    if uid.as_deref().is_none_or(|u| u == "0")
        && c.target_user != "root"
        && let Ok(Some(u)) = nix::unistd::User::from_name(&c.target_user)
    {
        uid = Some(u.uid.to_string());
        gid = Some(u.gid.to_string());
    }
    let Some(uid) = uid.filter(|u| u != "0") else { return };
    let owner = format!("{uid}:{}", gid.unwrap_or_default());
    let chown_if = |p: PathBuf| {
        if p.exists() {
            ctx::chown_r(&p, &owner);
        }
    };
    for app in apps {
        match app.as_str() {
            "zalo" => {
                chown_if(c.config_home.join("ZaloData"));
                let flags = c.config_home.join("zalo-flags.conf");
                if flags.is_file() {
                    let _ = ok("chown", &[&owner, &flags.to_string_lossy()]);
                }
            }
            "kde" => {
                for e in kde_config_entries() {
                    chown_if(c.config_home.join(e));
                }
                for e in KDE_SHARE_ENTRIES {
                    chown_if(c.data_home.join(e));
                }
                for e in KDE_HOME_ENTRIES {
                    chown_if(c.target_home.join(e));
                }
            }
            other => {
                if let Some(spec) = restore_spec(other) {
                    chown_if(c.config_home.join(spec.folder));
                }
            }
        }
    }
    let safety = safety_backup_dir();
    if safety.is_dir() {
        ctx::chown_r(&safety, &owner);
        // Model snapshots can contain every user's face model; keep them root-only (all of
        // them: the chown above just handed earlier snapshots to the user too)
        let snapshots = fs::read_dir(&safety).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>()).unwrap_or_default();
        for howdy in snapshots.iter().map(|p| p.join("howdy")).chain([rollback.join("howdy")]).filter(|p| p.is_dir()) {
            ctx::chown_r(&howdy, "root:root");
            let _ = ok("chmod", &["-R", "go-rwx", &howdy.to_string_lossy()]);
        }
    }
}
