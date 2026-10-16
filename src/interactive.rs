//! Menu-driven import/export screens.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::ctx::{self, ctx, log, reported};
use crate::ui::{self, C_BLUE, C_BOLD, C_CYAN, C_GRAY, C_GREEN, C_RESET, C_YELLOW, read_choice};
use crate::vault::archive::read_zip_manifest;
use crate::vault::backup::{BackupOpts, cmd_backup};
use crate::vault::import::{ImportOpts, cmd_import};
use crate::vault::{format_bytes, info::cmd_info};

fn glob_match(name: &str, pattern: &str) -> bool {
    // Patterns here are only "*a*.ext", "a_*.ext" or a literal
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return name == pattern;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Backup archives and face models in the usual places (maxdepth 2), newest first.
pub fn find_candidates() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let c = ctx();
    let dirs = [
        PathBuf::from("/media/data/backups"),
        PathBuf::from("/media/data"),
        c.target_home.join(".local/share/app-vault-sync/backups"),
        c.target_home.join("Downloads"),
    ];
    let zip_pats = ["*backup*.zip", "all_*.zip", "*vault*.zip", "*kde*.zip", "*plasma*.zip", "*hypr*.zip"];
    let user_dat = format!("{}.dat", c.target_user);
    let dat_pats = ["*face*.dat", "*howdy*.dat", user_dat.as_str()];
    let (mut zips, mut models) = (Vec::new(), Vec::new());
    for d in dirs.iter().filter(|d| d.is_dir()) {
        for e in WalkDir::new(d).min_depth(1).max_depth(2).sort_by_file_name().into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy();
            let p = e.path().to_path_buf();
            if zip_pats.iter().any(|pat| glob_match(&name, pat)) && !zips.contains(&p) {
                zips.push(p);
            } else if dat_pats.iter().any(|pat| glob_match(&name, pat)) && !models.contains(&p) {
                models.push(p);
            }
        }
    }
    // Newest first: the backups folder accumulates over time
    let newest_first = |v: &mut Vec<PathBuf>| {
        v.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    };
    newest_first(&mut zips);
    newest_first(&mut models);
    (zips, models)
}

/// Type label of a backup file, from its file name (the folder it sits in doesn't count).
pub fn candidate_tag(p: &Path) -> &'static str {
    let s = file_name(p).to_lowercase();
    if s.ends_with(".dat") {
        "[Howdy Face Model]"
    } else if s.contains("kde") || s.contains("plasma") {
        "[KDE Plasma Backup]"
    } else if s.contains("hypr") {
        "[Hyprland Backup]"
    } else if s.contains("all") {
        "[Full System Backup]"
    } else {
        "[Archive]"
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Interactive restore screen (menu option 6 / `kast import` without arguments).
pub fn import_data() -> Result<()> {
    let c = ctx();
    log("INFO", "Starting App Vault & Howdy Face Model import...");
    let (zips, models) = find_candidates();

    ui::restore_terminal_interactive();
    ui::clear_screen();
    println!("{C_CYAN}{C_BOLD}=== Import / Restore Application Data, Desktop Settings & Face Models ==={C_RESET}\n");
    let all: Vec<PathBuf> = zips.into_iter().chain(models).collect();
    let target: PathBuf = if !all.is_empty() {
        println!("{C_BOLD}Found backup archives & face models:{C_RESET}");
        for (i, item) in all.iter().enumerate() {
            let s = item.to_string_lossy();
            let tag = candidate_tag(item);
            println!("  [{}] {s} ({}) {C_GRAY}{tag}{C_RESET}", i + 1, format_bytes(item));
        }
        println!("  [c] Enter custom path (.zip or .dat)");
        println!("  [q] Cancel and return to menu");
        println!();
        let choice = read_choice(&format!("Select file [1-{}, c, q]: ", all.len()));
        match choice.parse::<usize>() {
            Ok(n) if (1..=all.len()).contains(&n) => all[n - 1].clone(),
            _ if choice.eq_ignore_ascii_case("c") => PathBuf::from(read_choice("Enter full path to backup .zip or .dat: ")),
            _ => {
                println!("Import canceled.");
                ui::sleep_secs(1.0);
                return Ok(());
            }
        }
    } else {
        let p = read_choice("Enter full path to backup .zip archive or .dat face model (or 'q' to cancel): ");
        if p.is_empty() || p.eq_ignore_ascii_case("q") {
            println!("Import canceled.");
            ui::sleep_secs(1.0);
            return Ok(());
        }
        PathBuf::from(p)
    };

    if !target.is_file() {
        log("ERROR", &format!("File not found: {}", target.display()));
        println!("File not found: {}", target.display());
        ui::sleep_secs(2.0);
        return Err(reported());
    }

    log("INFO", &format!("Executing restore directly for file {}...", target.display()));
    ui::restore_terminal_interactive();
    let opts = |app: Option<String>, force: bool, kill: bool| ImportOpts {
        file: target.clone(),
        app,
        safety_backup: true,
        kill,
        force,
        confirmed: force,
        dry_run: false,
    };

    if target.to_string_lossy().ends_with(".dat") {
        println!("\n{C_BLUE}[INFO]{C_RESET} Standalone Howdy Face Model: {}", file_name(&target));
        println!("  Target User: {C_BOLD}{}{C_RESET}", c.target_user);
        println!("  Destination: /etc/howdy/models/{}.dat\n", c.target_user);
        if read_choice("Proceed with face model restoration? (y/N): ").eq_ignore_ascii_case("y") {
            println!("{C_CYAN}==>{C_RESET} Restoring face model as root for {C_BOLD}{}{C_RESET}...", c.target_user);
            cmd_import(opts(None, true, false))?;
            if ctx::canceled() {
                println!("\n{C_YELLOW}[CANCELED]{C_RESET} Face model was not restored.");
            } else {
                println!("\n{C_GREEN}[SUCCESS]{C_RESET} Face model restoration complete!");
            }
            read_choice("Press Enter to return to menu...");
        } else {
            println!("Restoration aborted.");
            ui::sleep_secs(1.0);
        }
        return Ok(());
    }

    println!("\n{C_BLUE}[INFO]{C_RESET} Inspecting archive manifest:");
    let _ = cmd_info(Some(&target));
    println!();
    let manifest: Value = read_zip_manifest(&target).and_then(|r| serde_json::from_str(&r).ok()).unwrap_or(Value::Null);
    let apps = manifest.get("apps").and_then(Value::as_object);
    let has = |k: &str| apps.is_some_and(|a| a.get(k).is_some_and(|v| !v.is_null() && v != &Value::Bool(false)));
    let app_count = apps.map_or(1, |a| a.len());

    let mut chosen_app: Option<String> = None;
    if app_count > 1 {
        println!("{C_BOLD}Restore options for archive:{C_RESET}");
        println!("  1) Restore all applications & desktop configurations [Recommended]");
        let mut n = 2;
        let (mut kde_opt, mut hypr_opt) = (None, None);
        if has("kde") {
            println!("  {n}) Restore KDE Plasma settings only");
            kde_opt = Some(n.to_string());
            n += 1;
        }
        if has("hypr") {
            println!("  {n}) Restore Hyprland settings only");
            hypr_opt = Some(n.to_string());
            n += 1;
        }
        println!("  {n}) Restore specific application");
        let choice = read_choice(&format!("Choose option [1-{n}, default: 1]: "));
        if kde_opt.as_deref() == Some(choice.as_str()) {
            chosen_app = Some("kde".into());
        } else if hypr_opt.as_deref() == Some(choice.as_str()) {
            chosen_app = Some("hypr".into());
        } else if choice == n.to_string() {
            let a = read_choice("Enter application name to restore (e.g. beeper, obsidian, hypr, kde): ");
            chosen_app = (!a.is_empty()).then_some(a);
        }
    }

    if read_choice("Proceed with restoration? (y/N): ").eq_ignore_ascii_case("y") {
        println!("{C_CYAN}==>{C_RESET} Restoring files as {C_BOLD}{}{C_RESET}...", c.target_user);
        cmd_import(opts(chosen_app, false, true))?;
        if ctx::canceled() {
            println!("\n{C_YELLOW}[CANCELED]{C_RESET} Nothing was restored.");
        } else {
            println!("\n{C_GREEN}[SUCCESS]{C_RESET} Restoration complete!");
        }
        read_choice("Press Enter to return to menu...");
    } else {
        println!("Restoration aborted.");
        ui::sleep_secs(1.0);
    }
    Ok(())
}

pub fn export_data() -> Result<()> {
    let c = ctx();
    ui::restore_terminal_interactive();
    ui::clear_screen();
    println!("{C_CYAN}{C_BOLD}=== Export / Backup Application Data & Face Models ==={C_RESET}\n");
    println!("Targets:");
    let targets = [
        ("all", "All detected applications & Howdy face model (Recommended)"),
        ("beeper", "BeeperTexts (~/.config/BeeperTexts)"),
        ("zalo", "ZaloData (~/.config/ZaloData)"),
        ("vesktop", "Vesktop (~/.config/vesktop)"),
        ("zen", "Zen Browser (~/.config/zen)"),
        ("obsidian", "Obsidian (~/.config/obsidian)"),
        ("fcitx5", "Fcitx5 (~/.config/fcitx5)"),
        ("antigravity", "Antigravity (~/.config/Antigravity)"),
        ("howdy", "Howdy Face Recognition Model (/etc/howdy/models)"),
        ("kde", "KDE Plasma settings (curated ~/.config + ~/.local/share)"),
        ("hypr", "Hyprland settings (~/.config/hypr)"),
    ];
    for (i, (_, label)) in targets.iter().enumerate() {
        println!("  {}) {label}", i + 1);
    }
    println!("  q) Cancel\n");
    let choice = read_choice("Choose target [1-11, q]: ");
    let target = match choice.parse::<usize>() {
        Ok(n) if (1..=targets.len()).contains(&n) => targets[n - 1].0,
        _ if choice.is_empty() || choice.eq_ignore_ascii_case("q") => {
            println!("Backup canceled.");
            ui::sleep_secs(1.0);
            return Ok(());
        }
        _ => {
            println!("Invalid choice: '{choice}'");
            ui::sleep_secs(1.0);
            return Ok(());
        }
    };

    let mut is_dat = false;
    if target == "howdy" {
        println!("\nExport Format for Howdy:");
        println!("  1) Standalone .dat file (compatible with setup_howdy.sh & manual import) [Recommended]");
        println!("  2) App Vault .zip archive (standard container with manifest)");
        is_dat = read_choice("Choose format [1-2, default: 1]: ") != "2";
    }
    let mut include_media = true;
    if !matches!(target, "howdy" | "kde" | "hypr") {
        include_media = !read_choice("Include large media/attachments? (Y/n): ").eq_ignore_ascii_case("n");
    }

    let def_dest =
        if Path::new("/media/data").is_dir() { PathBuf::from("/media/data/backups") } else { c.target_home.join(".local/share/app-vault-sync/backups") };
    ctx::mkdir_user(&def_dest)?;
    let ts = ctx::now_str("%Y-%m-%d_%H%M%S");
    let out = if is_dat { def_dest.join(format!("{}_face_model_{ts}.dat", c.target_user)) } else { def_dest.join(format!("{target}_backup_{ts}.zip")) };

    println!("\nDestination: {C_BOLD}{}{C_RESET}", out.display());
    read_choice("Press Enter to start backup (or Ctrl+C to abort)...");
    println!("\n{C_CYAN}==>{C_RESET} Executing backup as {C_BOLD}{}{C_RESET}...", c.target_user);
    cmd_backup(BackupOpts { target: target.into(), output: Some(out.clone()), include_media, kill: true, force: false, dry_run: false })?;
    if ctx::canceled() {
        println!("\n{C_YELLOW}[CANCELED]{C_RESET} No backup was created.");
    } else {
        println!("\n{C_GREEN}[SUCCESS]{C_RESET} Backup completed: {}", out.display());
    }
    read_choice("Press Enter to return to menu...");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{candidate_tag, glob_match};
    use std::path::Path;

    #[test]
    fn globs() {
        assert!(glob_match("all_backup_2026.zip", "*backup*.zip"));
        assert!(glob_match("all_x.zip", "all_*.zip"));
        assert!(!glob_match("small_x.zip", "all_*.zip"));
        assert!(glob_match("p3ter.dat", "p3ter.dat"));
        assert!(!glob_match("backup.zip.txt", "*backup*.zip"));
        assert!(glob_match("backup.zip", "*backup*.zip"));
    }

    #[test]
    fn tag_ignores_folders() {
        assert_eq!(candidate_tag(Path::new("/home/sally/backup.zip")), "[Archive]");
        assert_eq!(candidate_tag(Path::new("/media/data/install/kde_backup.zip")), "[KDE Plasma Backup]");
        assert_eq!(candidate_tag(Path::new("/home/p3ter/hypr/all_2026.zip")), "[Full System Backup]");
        assert_eq!(candidate_tag(Path::new("/home/p3ter/Hyprland.zip")), "[Hyprland Backup]");
        assert_eq!(candidate_tag(Path::new("/media/data/all/p3ter.dat")), "[Howdy Face Model]");
    }
}
