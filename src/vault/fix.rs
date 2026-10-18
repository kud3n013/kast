//! `kast fix-permissions`: find and repair what an older restore left root-owned or
//! read-only (`Configuration file "~/.config/spectaclerc" not writable`).

use anyhow::Result;

use crate::ctx::{ctx, reported};
use crate::perms::{self, Finding};
use crate::report::{self, Prompt};

use super::import::{destination_folders, managed_paths, repair_jails};
use super::{ALL_APPS, log_error, log_info, log_step, log_success, log_warn};

/// The entries kast manages that are not as the target user needs them.
pub fn find_problems() -> (Vec<Finding>, Vec<String>) {
    let c = ctx();
    let apps: Vec<String> = ALL_APPS.iter().filter(|a| **a != "howdy").map(|a| a.to_string()).collect();
    let mut paths = managed_paths(&apps);
    for extra in [c.config_home.join("kast"), c.data_home.join("kast")] {
        if extra.symlink_metadata().is_ok() {
            paths.push(extra);
        }
    }
    let jails = repair_jails();
    let (mut found, mut errors) = (Vec::new(), Vec::new());
    for p in destination_folders() {
        match perms::scan_shallow(&jails, &p, c.target_uid, c.target_gid) {
            Ok(f) => found.extend(f),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    for p in paths {
        match perms::scan(&jails, &p, c.target_uid, c.target_gid) {
            Ok(f) => found.extend(f),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (found, errors)
}

pub fn cmd_fix_permissions(dry_run: bool, yes: bool) -> Result<()> {
    let c = ctx();
    log_step(&format!("Checking kast-managed settings of {} ({}:{})", c.target_user, c.target_uid, c.target_gid));
    let (found, errors) = find_problems();
    for e in &errors {
        log_warn(&format!("Not checked: {e}"));
    }
    if found.is_empty() {
        log_success("Everything is owned by you and writable.");
        return Ok(());
    }
    let unowned = found.iter().filter(|f| !f.owned).count();
    let readonly = found.iter().filter(|f| !f.usable).count();
    log_warn(&format!("{} entries need repair ({unowned} not owned by {}, {readonly} without owner write):", found.len(), c.target_user));
    for f in found.iter().take(40) {
        log_info(&format!("  {}", f.describe(c.target_uid, c.target_gid)));
    }
    if found.len() > 40 {
        log_info(&format!("  ... and {} more", found.len() - 40));
    }
    if dry_run {
        log_info("Dry run: nothing was changed.");
        return Ok(());
    }
    if !yes && !report::ask(&Prompt::yes_no("Fix these now?", true)) {
        log_info("Nothing changed.");
        return Ok(());
    }
    let r = perms::repair(&found, c.target_uid, c.target_gid, !c.is_root);
    log_success(&format!("Repaired {} entries.", r.fixed));
    if r.failed.is_empty() {
        return Ok(());
    }
    for (p, why) in r.failed.iter().take(20) {
        log_error(&format!("{}: {why}", p.display()));
    }
    log_error(&format!(
        "{} entries could not be repaired. Run as root: sudo kast fix-permissions   (or: sudo chown -R {}:{} <path>)",
        r.failed.len(),
        c.target_uid,
        c.target_gid
    ));
    Err(reported())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_context_has_no_problems_and_never_looks_outside_the_home() {
        let _g = crate::perms::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::ctx::init_for_tests();
        let c = ctx();
        std::fs::create_dir_all(&c.config_home).unwrap();
        std::fs::write(c.config_home.join("spectaclerc"), "x").unwrap();
        let (found, errors) = find_problems();
        assert!(found.is_empty() && errors.is_empty(), "{found:?} {errors:?}");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(c.config_home.join("spectaclerc"), std::fs::Permissions::from_mode(0o444)).unwrap();
        // spectaclerc is not in the managed list unless it is a KDE entry: it is (spectaclerc)
        let (found, _) = find_problems();
        assert!(found.iter().any(|f| f.path.ends_with("spectaclerc") && f.owned && !f.usable), "{found:?}");
        cmd_fix_permissions(false, true).unwrap();
        assert!(find_problems().0.is_empty());
    }
}
