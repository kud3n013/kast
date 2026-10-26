use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::ctx::{self, ctx, have, ok, ok_priv, reported};
use crate::report::{self, Prompt};
use crate::ui::{C_BOLD, C_RESET};

use super::archive::{MANIFEST, create_zip_archive};
use super::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackupOpts {
    pub target: String,
    pub output: Option<PathBuf>,
    pub include_media: bool,
    pub kill: bool,
    pub force: bool,
    pub dry_run: bool,
    /// Running apps: ask (or kill with `kill`), or leave them out without a word of prompt
    pub running: RunningPolicy,
    /// Leave the Howdy face model out of "all" (it needs sudo; scheduled backups skip it)
    pub no_howdy: bool,
    /// Built into a staging folder for an upload: not recorded as a local backup
    pub staged: bool,
    /// Don't start the apps kast closed again afterwards
    pub no_relaunch: bool,
    /// `all` includes the keyrings (also when `[vault] keyrings = true` in the settings)
    pub with_keyrings: bool,
    /// A local backup may carry a GNOME keyring that has no password
    pub allow_plaintext_keyring: bool,
    /// Built for Proton Drive or a scheduled run: a plain-text keyring never goes in
    pub upload: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunningPolicy {
    /// What kast always did: ask to close them (or kill with -k, go on with -f)
    #[default]
    Ask,
    /// Back up everything else and report the running ones (scheduled backups: no prompts)
    Skip,
}

/// Apps the last backup left out because they were running (`RunningPolicy::Skip`).
static SKIPPED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn take_skipped() -> Vec<String> {
    std::mem::take(&mut *SKIPPED.lock().unwrap())
}

const COMMON_EXCLUDES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "Crashpad",
    "blob_storage",
    "SingletonCookie",
    "SingletonLock",
    "SingletonSocket",
    "*.log",
    "dumps",
    "shader-cache",
    "startupCache",
];

/// How one config-directory app is backed up.
struct AppSpec {
    folder: &'static str,
    label: &'static str,
    missing: &'static str,
    extra_excludes: &'static [&'static str],
    /// Excluded with --no-media; also records include_media in the manifest
    media_exclude: Option<&'static str>,
}

fn app_spec(app: &str) -> Option<AppSpec> {
    let s = |folder, label, missing, extra_excludes, media_exclude| AppSpec { folder, label, missing, extra_excludes, media_exclude };
    Some(match app {
        "beeper" => s("BeeperTexts", "BeeperTexts (~/.config/BeeperTexts)", "BeeperTexts config not found", &[], Some("media")),
        "zalo" => s("ZaloData", "ZaloData (~/.config/ZaloData)", "ZaloData not found", &["resp_cache", "zcall-wine", "zcall-wine-validate"], Some("media")),
        "vesktop" => s("vesktop", "Vesktop (~/.config/vesktop)", "Vesktop config not found", &[], None),
        "zen" => s(
            "zen",
            "Zen Browser (~/.config/zen)",
            "Zen Browser config not found",
            &["lock", ".parentlock", "crashes", "datareporting", "shader-cache", "startupCache", "storage/temporary"],
            Some("storage/default/https+++*"),
        ),
        "helium" => s(
            "net.imput.helium",
            "Helium (~/.config/net.imput.helium)",
            "Helium config not found",
            &[
                "GPUPersistentCache",
                "GrShaderCache",
                "ShaderCache",
                "BrowserMetrics*",
                "Crash Reports",
                "component_crx_cache",
                "segmentation_platform",
                "optimization_guide*",
                "Service Worker/CacheStorage",
            ],
            None,
        ),
        // The vault in ~/Documents comes along as the second folder. Sign-in and Sync's link to the
        // vault are in the config folder (keyed by the vault's id); the sign-in token is encrypted
        // with the desktop keyring's key, so it needs the keyring too
        "obsidian" => s("obsidian", "Obsidian (~/.config/obsidian + ~/Documents/Obsidian Vault)", "Obsidian config not found", &[], None),
        "fcitx5" => s("fcitx5", "Fcitx5 (~/.config/fcitx5)", "Fcitx5 config not found", &[], None),
        "antigravity" => s("Antigravity", "Antigravity (~/.config/Antigravity)", "Antigravity config not found", &["crashes", "DevToolsActivePort"], None),
        "fdm" => s("Softdeluxe", "Free Download Manager (~/.config/Softdeluxe)", "Free Download Manager config not found", &["logs", "cache", "backup"], None),
        "fish" => s("fish", "fish (~/.config/fish)", "fish config not found", &[], None),
        "scripts" => s("scripts", "Scripts (~/.scripts)", "~/.scripts not found", &[], None),
        // ~/Zotero is the library (zotero.sqlite, attachments in storage/); the profile in
        // ~/.zotero comes along as the second folder
        "zotero" => s(
            "Zotero",
            "Zotero (~/Zotero + ~/.zotero)",
            "Zotero data directory (~/Zotero) not found",
            &["lock", ".parentlock", "crashes", "minidumps", "datareporting", "logs", "zotero.sqlite.*.bak"],
            Some("/storage"),
        ),
        // Conversations, memory, settings, skills and plugins; not the sign-in token
        // (.credentials.json: sign in again after a restore) or what Claude Code rebuilds
        "claude" => s(
            "claude",
            "Claude Code (~/.claude + ~/.claude.json)",
            "Claude Code folder (~/.claude) not found",
            &[".credentials.json", "/cache", "/paste-cache", "/shell-snapshots", "/session-env", "/sessions", "/statsig", "/telemetry", "/debug", "/ide"],
            None,
        ),
        "copilot" => s("copilot", "GitHub Copilot CLI (~/.copilot)", "Copilot CLI folder (~/.copilot) not found", &["/logs"], None),
        "fastfetch" => s("fastfetch", "fastfetch (~/.config/fastfetch)", "fastfetch config not found", &[], None),
        "hypr" => s("hypr", "Hyprland (~/.config/hypr)", "Hyprland config directory (~/.config/hypr) not found", &[], None),
        _ => return None,
    })
}

/// `realpath -m`: absolute and normalized, the target need not exist.
pub fn absolutize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::from("/");
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            _ => {}
        }
    }
    out.canonicalize().unwrap_or(out)
}

fn chown_to_invoker(path: &Path) {
    let (uid, gid) = file_owner();
    if uid == 0 {
        return;
    }
    if ctx().is_root {
        ctx::chown_path(path, uid, gid);
    } else {
        let _ = ok("sudo", &["-n", "chown", &format!("{uid}:{gid}"), &path.to_string_lossy()]);
    }
}

fn export_howdy_dat(output: &Path, dry_run: bool) -> Result<()> {
    let c = ctx();
    log_step("Exporting Howdy Face Model (.dat)...");
    if !dry_run && !c.is_root && howdy_installed() && !sudo_ready() {
        warn_sudo_needed("the face model");
        return Err(reported());
    }
    let model = find_howdy_model(&c.target_user).or_else(|| dry_run.then(|| howdy_model_path(&c.target_user)));
    let Some(model) = model else {
        log_error(&format!("No enrolled face model found for user '{0}' at /etc/howdy/models/{0}.dat", c.target_user));
        return Err(reported());
    };
    if dry_run {
        log_info(&format!("  [Dry-run] Would export face model: {} -> {}", model.display(), output.display()));
        return Ok(());
    }
    ctx::mkdir_user(output.parent().unwrap_or(Path::new("/")))?;
    if !ok_priv("cp", &["-p", &model.to_string_lossy(), &output.to_string_lossy()]) {
        log_error(&format!("Could not copy {} to {}", model.display(), output.display()));
        return Err(reported());
    }
    chown_to_invoker(output);
    log_success(&format!("Exported Howdy face model for '{}' to: {}", c.target_user, output.display()));
    crate::journal::export(output);
    Ok(())
}

fn howdy_available(dry_run: bool) -> bool {
    let c = ctx();
    let dir = Path::new(HOWDY_MODELS_DIR);
    let file = howdy_model_path(&c.target_user);
    let (dir_s, file_s) = (dir.to_string_lossy(), file.to_string_lossy());
    let sudo_n = || ok("sudo", &["-n", "test", "-d", &dir_s]) || ok("sudo", &["-n", "test", "-f", &file_s]);
    if dir.is_dir() || file.is_file() {
        true
    } else if dry_run {
        ok("pacman", &["-Q", "howdy-next"]) || ok("pacman", &["-Q", "howdy-next-bin"]) || have("howdy") || sudo_n()
    } else if !c.is_root && have("sudo") {
        // sudo_ready asked for the password before the backup started, if it could
        sudo_n()
    } else {
        false
    }
}

pub fn cmd_backup(o: BackupOpts) -> Result<()> {
    let c = ctx();
    ctx::set_canceled(false);
    require_vault_tools()?;
    let target = normalize_app(&o.target);

    let output = match &o.output {
        Some(p) => p.clone(),
        None => {
            if !o.dry_run {
                ctx::mkdir_user(&default_backup_dir())?;
            }
            default_backup_dir().join(format!("{target}_backup_{}.zip", ctx::now_str("%Y-%m-%d_%H%M%S")))
        }
    };
    let output = absolutize(&output);

    if target == "howdy" && is_dat(&output) {
        return export_howdy_dat(&output, o.dry_run);
    }

    log_step(&format!("Preparing backup for target: {C_BOLD}{target}{C_RESET}"));
    log_info("Archive format:       ZIP (built-in)");
    log_info(&format!("Output path:          {}", output.display()));
    log_info(&format!("Include media:        {}", o.include_media));
    if o.dry_run {
        log_warn("DRY-RUN MODE ENABLED: No files will be archived.");
    }

    let with_keyrings = o.with_keyrings || (target == "all" && crate::settings::load().vault.keyrings);
    let mut apps: Vec<String> = expand_target(&target, with_keyrings);
    if o.no_howdy && target == "all" {
        apps.retain(|a| a != "howdy");
    }

    let mut running: Vec<String> = Vec::new();
    let mut running_info: Vec<String> = Vec::new();
    for app in &apps {
        let pids = running_pids(app);
        if !pids.is_empty() {
            log_warn(&format!("Application '{app}' is currently running (PID: {}).", pid_list(&pids)));
            running.push(app.clone());
            running_info.push(format!("{app} (PID: {})", pid_list(&pids)));
        }
    }
    let mut _relaunch = RelaunchAfter(Vec::new());
    let plan = || if o.no_relaunch || o.dry_run { Vec::new() } else { relaunch_plan(&running) };
    if !running.is_empty() && o.running == RunningPolicy::Skip {
        // Never close an app the user is working in: back up the rest, say what was left out
        for app in &running {
            log_warn(&format!("Skipping '{app}': it's running, and this backup doesn't close apps."));
        }
        apps.retain(|a| !running.contains(a));
        *SKIPPED.lock().unwrap() = running.clone();
        if apps.is_empty() {
            log_warn("Nothing left to back up: every app of this backup is running.");
            return Ok(());
        }
    } else if !running.is_empty() && (!o.force || o.kill) {
        let list = running.join(" ");
        if o.dry_run {
            log_warn(&format!(
                "Notice: Running applications detected ({list}). During a live backup, you will be prompted to close/kill them or use -k/--kill."
            ));
        } else if o.kill {
            _relaunch.0 = plan();
            if !terminate_apps(&running) {
                log_error("Could not terminate all running applications. Backup aborted to protect data.");
                return Err(reported());
            }
        } else {
            report::blank();
            log_warn("Backing up applications while active can produce inconsistent or corrupted database snapshots (SQLite WAL / LevelDB locks).");
            let ask = Prompt::yes_no(format!("Close/kill active application(s) ({list}) to run backup safely?"), true);
            if report::ask(&ask.with_details(running_info)) {
                _relaunch.0 = plan();
                if !terminate_apps(&running) {
                    log_error("Could not terminate all running applications. Backup aborted to protect data.");
                    return Err(reported());
                }
            } else {
                if !report::ask(&Prompt::yes_no("Proceed anyway with applications running (risks inconsistent data)?", false)) {
                    log_info("Backup aborted by user. Please close running applications and retry.");
                    ctx::set_canceled(true);
                    return Ok(());
                }
                log_warn("Proceeding with live backup as requested. Note: open database files might be dirty.");
            }
        }
    }

    // Ask for the sudo password now rather than in the middle of the backup
    if !o.dry_run && apps.iter().any(|a| a == "howdy") && !c.is_root && howdy_installed() {
        sudo_ready();
    }

    let staging = get_safe_staging_dir("app-vault-sync-backup")?;
    ctx::set_staging(staging.clone());

    let hostname = nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|_| "unknown".into());
    let whoami = nix::unistd::User::from_uid(nix::unistd::Uid::effective()).ok().flatten().map(|u| u.name).unwrap_or_default();
    let mut manifest = json!({
        "version": "2.0",
        "created_at": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "hostname": hostname,
        "user": whoami,
        "home": c.target_home,
        "target": target,
        "include_media": o.include_media,
        "apps": {}
    });
    let mut app_entries = Map::new();

    for app in &apps {
        match app.as_str() {
            "howdy" => {
                if let Some(v) = backup_howdy(&staging, o.dry_run)? {
                    app_entries.insert("howdy".into(), v);
                }
            }
            "kde" => {
                if let Some(v) = backup_kde(&staging, o.dry_run)? {
                    app_entries.insert("kde".into(), v);
                }
            }
            "ollama" => {
                if let Some(v) = ollama::backup(&staging, &o)? {
                    app_entries.insert("ollama".into(), v);
                }
            }
            k if keyring::is_keyring(k) => {
                let policy =
                    keyring::Policy { dry_run: o.dry_run, upload: o.upload, allow_plaintext: o.allow_plaintext_keyring, quiet_missing: target == "all" };
                if let Some(v) = keyring::backup(k, &staging, &policy)? {
                    app_entries.insert(k.to_string(), v);
                }
            }
            other => {
                let Some(spec) = app_spec(other) else { continue };
                if let Some(v) = backup_app_dir(other, &spec, &staging, &o)? {
                    app_entries.insert(other.to_string(), v);
                }
            }
        }
    }
    manifest["apps"] = Value::Object(app_entries);
    fs::write(staging.join(MANIFEST), serde_json::to_string_pretty(&manifest)? + "\n").context("writing manifest")?;

    if o.dry_run {
        ctx::cleanup_vault_staging();
        report::blank();
        log_success("Dry run complete. No files were written.");
        return Ok(());
    }

    log_step(&format!("Compressing archive to: {}", output.display()));
    ctx::mkdir_user(output.parent().unwrap_or(Path::new("/")))?;
    create_zip_archive(&staging, &output)?;
    // Only the archive itself: the parent may be any directory (-o /tmp/x.zip), and
    // directories we created were already handed over by mkdir_user
    let (uid, gid) = file_owner();
    if uid != 0 && c.is_root {
        ctx::chown_path(&output, uid, gid);
    }
    ctx::cleanup_vault_staging();

    report::blank();
    log_success("Backup completed successfully!");
    if !o.staged {
        crate::journal::export(&output);
    }
    log_info(&format!("Archive:    {}", output.display()));
    log_info(&format!("Total Size: {}", format_bytes(&output)));
    Ok(())
}

fn backup_app_dir(app: &str, spec: &AppSpec, staging: &Path, o: &BackupOpts) -> Result<Option<Value>> {
    let Some(cfg) = find_config(app) else {
        log_warn(&format!("{}, skipping...", spec.missing));
        return Ok(None);
    };
    log_step(&format!("Processing {}...", spec.label));
    let excludes: Vec<String> = COMMON_EXCLUDES.iter().chain(spec.extra_excludes).map(|e| format!("--exclude={e}")).collect();
    // The media folder is the main folder's only, never the second one's
    let mut main_excludes = excludes.clone();
    if let (false, Some(m)) = (o.include_media, spec.media_exclude) {
        main_excludes.push(format!("--exclude={m}"));
    }
    let dest = staging.join(spec.folder);
    fs::create_dir_all(&dest)?;
    if o.dry_run {
        let what = if app == "antigravity" { "Would sync config" } else { "Would sync" };
        log_info(&format!("  [Dry-run] {what}: {} -> {}", cfg.display(), spec.folder));
    } else {
        rsync_dir(&main_excludes, &cfg, &dest)?;
    }

    let mut entry = json!({"included": true, "folder": spec.folder, "config_path": cfg.to_string_lossy()});
    if let Some(share) = share_dir(app) {
        let staged = format!("{app}-share");
        if o.dry_run {
            log_info(&format!("  [Dry-run] Would sync: {} -> {staged}", share.display()));
        } else {
            let dest = staging.join(&staged);
            fs::create_dir_all(&dest)?;
            // Notes are the user's own files: a folder called Cache or a .log note stays in
            let excludes = if app == "obsidian" { &[][..] } else { &excludes[..] };
            rsync_dir(excludes, &share, &dest)?;
        }
        entry["share_path"] = json!(share.to_string_lossy());
        if app == "obsidian"
            && let Some(id) = obsidian_vault_id(&cfg, &share)
        {
            entry["vault_id"] = json!(id);
        }
    }
    if let Some((path, staged)) = side_file(app) {
        let file = Some(path).filter(|p| p.is_file());
        if let Some(f) = &file {
            if o.dry_run {
                log_info(&format!("  [Dry-run] Would sync: {} -> {staged}", f.display()));
            } else if !ok("cp", &["-p", &f.to_string_lossy(), &staging.join(staged).to_string_lossy()]) {
                anyhow::bail!("copying {} failed", f.display());
            }
        }
        let key = if app == "zalo" { "flags_path" } else { "file_path" };
        entry[key] = json!(file.map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    }
    if spec.media_exclude.is_some() {
        entry["include_media"] = json!(o.include_media);
    }
    Ok(Some(entry))
}

fn backup_howdy(staging: &Path, dry_run: bool) -> Result<Option<Value>> {
    let c = ctx();
    let file = howdy_model_path(&c.target_user);
    // Without root the model can't even be looked for: say so instead of "not found"
    if !dry_run && !c.is_root && howdy_installed() && !sudo_ready() {
        warn_sudo_needed("the Howdy face model");
        return Ok(None);
    }
    if !howdy_available(dry_run) {
        log_warn(&format!("Howdy face model not found ({}), skipping...", file.display()));
        return Ok(None);
    }
    log_step("Processing Howdy Face Recognition Model...");
    let models = staging.join("howdy/models");
    fs::create_dir_all(&models)?;
    if dry_run {
        log_info(&format!("  [Dry-run] Would sync: {HOWDY_MODELS_DIR} -> howdy/models"));
    } else {
        // Only this user's model: the archive ends up owned by the user, and other users'
        // biometric data must not leak into it
        let (src, dst) = (file.to_string_lossy(), format!("{}/", models.display()));
        if c.is_root {
            if file.is_file() {
                ok("cp", &["-p", &src, &dst]);
            }
        } else {
            ok("sudo", &["-n", "cp", "-p", &src, &dst]);
            let me = format!("{}:{}", nix::unistd::getuid(), nix::unistd::getgid());
            ok("sudo", &["-n", "chown", "-R", &me, &staging.join("howdy").to_string_lossy()]);
        }
    }
    Ok(Some(json!({
        "included": true,
        "user": c.target_user,
        "models_dir": HOWDY_MODELS_DIR,
        "model_file": format!("{}.dat", c.target_user),
    })))
}

fn backup_kde(staging: &Path, dry_run: bool) -> Result<Option<Value>> {
    let c = ctx();
    if !kde_has_settings() {
        log_warn("KDE Plasma settings not found (no kdeglobals/kwinrc/plasmashellrc), skipping...");
        return Ok(None);
    }
    log_step("Processing KDE Plasma (curated ~/.config + ~/.local/share)...");
    if process_running("plasmashell", None) {
        log_warn("plasmashell is running. Settings are still copied; log out once after restore so Plasma reloads them cleanly.");
    }
    for sub in ["config", "share", "home"] {
        fs::create_dir_all(staging.join("kde").join(sub))?;
    }
    kde_copy_named(&c.config_home, &staging.join("kde/config"), dry_run, &kde_config_entries())?;
    kde_copy_named(&c.data_home, &staging.join("kde/share"), dry_run, KDE_SHARE_ENTRIES)?;
    kde_copy_named(&c.target_home, &staging.join("kde/home"), dry_run, KDE_HOME_ENTRIES)?;
    Ok(Some(json!({
        "included": true,
        "folder": "kde",
        "config_path": c.config_home.to_string_lossy(),
        "share_path": c.data_home.to_string_lossy(),
        "kind": "plasma",
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_normalizes() {
        assert_eq!(absolutize(Path::new("/nonexistent-x/a/../b.zip")), PathBuf::from("/nonexistent-x/b.zip"));
        assert!(absolutize(Path::new("rel.zip")).is_absolute());
    }
}
