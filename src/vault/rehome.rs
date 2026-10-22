//! Restoring on an account with another name: text configs that spell out the old home
//! folder (`/home/old/Pictures/wall.jpg`, `file:///home/old/...`) are pointed at the new one.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use super::info::jq_str;
use super::*;

/// Apps whose archived files are plain-text settings. Databases and browser or Electron
/// profiles are left alone: a byte-level edit could corrupt them.
pub const REHOME_APPS: &[&str] = &["kde", "hypr", "fcitx5", "fish", "fastfetch", "scripts"];

const MAX_FILE: u64 = 4 * 1024 * 1024;

/// The home folder the archive was made from: recorded since the manifest got `home`,
/// otherwise `/home/<user>` (a `root` user means the backup ran under sudo and says nothing).
pub fn source_home(manifest: &Value) -> Option<PathBuf> {
    let home = jq_str(manifest, &["home"], "");
    if home.starts_with('/') {
        return Some(PathBuf::from(home.trim_end_matches('/')));
    }
    let user = jq_str(manifest, &["user"], "");
    (!user.is_empty() && user != "root" && !user.contains('/')).then(|| PathBuf::from("/home").join(user))
}

/// `from` as a whole path prefix: followed by `/` or by something that can't continue a
/// folder name, so `/home/k` doesn't match `/home/kate`.
fn replace_home(text: &str, from: &str, to: &str) -> Option<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut n = 0;
    while let Some(i) = rest.find(from) {
        let after = &rest[i + from.len()..];
        let boundary = after.chars().next().is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')));
        out.push_str(&rest[..i]);
        if boundary {
            out.push_str(to);
            n += 1;
        } else {
            out.push_str(from);
        }
        rest = after;
    }
    if n == 0 {
        return None;
    }
    out.push_str(rest);
    Some((out, n))
}

/// Rewrite the old home in the staged files of `apps`. Returns the number of files changed
/// (or that would be, for a dry run).
pub fn rehome_staging(staging: &Path, apps: &[String], manifest: &Value, dry_run: bool) -> usize {
    let new_home = ctx().target_home.clone();
    let Some(old_home) = source_home(manifest) else { return 0 };
    if old_home == new_home {
        return 0;
    }
    let (from, to) = (old_home.to_string_lossy().into_owned(), new_home.to_string_lossy().into_owned());
    let mut files = 0;
    for app in apps.iter().filter(|a| REHOME_APPS.contains(&a.as_str())) {
        let root = staging.join(app);
        for e in WalkDir::new(&root).into_iter().flatten() {
            let ft = e.file_type();
            if !ft.is_file() || e.metadata().is_ok_and(|m| m.len() > MAX_FILE) {
                continue;
            }
            let Ok(text) = fs::read_to_string(e.path()) else { continue };
            let Some((new_text, n)) = replace_home(&text, &from, &to) else { continue };
            if !dry_run && fs::write(e.path(), new_text).is_err() {
                log_warn(&format!("  Could not update paths in {}", e.path().strip_prefix(staging).unwrap_or(e.path()).display()));
                continue;
            }
            log_info(&format!("  {} {n} path(s) in {}", if dry_run { "[Dry-run] Would update" } else { "Updated" }, e.path().strip_prefix(staging).unwrap_or(e.path()).display()));
            files += 1;
        }
    }
    if files > 0 {
        log_info(&format!("Home folder in the archive was {from}, this account's is {to}: {files} file(s) {}.", if dry_run { "would be adjusted" } else { "adjusted" }));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_whole_path_components_only() {
        let (t, n) = replace_home("a=/home/k/x\nb=file:///home/k\nc=/home/kate/y\nd=/home/k.old\ne=/home/k", "/home/k", "/home/new").unwrap();
        assert_eq!(t, "a=/home/new/x\nb=file:///home/new\nc=/home/kate/y\nd=/home/k.old\ne=/home/new");
        assert_eq!(n, 3);
        assert!(replace_home("nothing /home/kate", "/home/k", "/home/new").is_none());
    }

    #[test]
    fn source_home_from_manifest() {
        assert_eq!(source_home(&json!({"home": "/data/u/", "user": "x"})), Some(PathBuf::from("/data/u")));
        assert_eq!(source_home(&json!({"user": "old"})), Some(PathBuf::from("/home/old")));
        assert_eq!(source_home(&json!({"user": "root"})), None);
        assert_eq!(source_home(&json!({})), None);
    }

    #[test]
    fn staged_text_configs_are_rewritten_and_other_apps_left_alone() {
        let c = crate::ctx::init_for_tests();
        let new_home = c.target_home.to_string_lossy().into_owned();
        let st = tempfile::tempdir().unwrap();
        let conf = st.path().join("kde/config/plasma-org.kde.plasma.desktop-appletsrc");
        let other = st.path().join("zen/prefs.js");
        let bin = st.path().join("kde/config/blob");
        for p in [&conf, &other, &bin] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        fs::write(&conf, "Image=file:///home/olduser/Pictures/w.jpg\nOther=/home/oldusers/x\n").unwrap();
        fs::write(&other, "/home/olduser/p").unwrap();
        fs::write(&bin, [0xff, 0xfe, b'/', b'h']).unwrap();
        let m = json!({"user": "olduser"});
        let apps = vec!["kde".to_string(), "zen".to_string()];

        assert_eq!(rehome_staging(st.path(), &apps, &m, true), 1);
        assert!(fs::read_to_string(&conf).unwrap().contains("/home/olduser/Pictures"), "dry run writes nothing");

        assert_eq!(rehome_staging(st.path(), &apps, &m, false), 1);
        assert_eq!(fs::read_to_string(&conf).unwrap(), format!("Image=file://{new_home}/Pictures/w.jpg\nOther=/home/oldusers/x\n"));
        assert_eq!(fs::read_to_string(&other).unwrap(), "/home/olduser/p");
        assert_eq!(fs::read(&bin).unwrap(), [0xff, 0xfe, b'/', b'h']);

        assert_eq!(rehome_staging(st.path(), &apps, &json!({"home": new_home}), false), 0, "same home: nothing to do");
    }
}
