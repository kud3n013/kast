// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! `config-check` (the fork's idea): the list against what is on disk in `~/.config` and
//! `~/.local/share/plasma`. The fork only compared sections whose location was exactly
//! `$CONFIG_DIR`; this works on resolved paths, so every section counts.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::config::Config;
use super::paths::{Roots, entry_covers, expand_location, resolve_entry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// On disk and not in the list
    Untracked,
    /// A folder that holds listed entries but is not listed as a whole
    Partial,
    Tracked,
    /// In the list, not on disk
    Missing,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Untracked => "untracked",
            Status::Partial => "partly listed",
            Status::Tracked => "tracked",
            Status::Missing => "not on disk",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub dir: PathBuf,
    pub name: String,
    pub status: Status,
    /// Sections that list it
    pub sections: Vec<String>,
    /// Looks like a Plasma/KDE/GTK setting (worth a look when untracked)
    pub kde_like: bool,
}

pub fn looks_like_settings(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !n.starts_with('.') && (n.ends_with("rc") || n.starts_with('k') || n.starts_with("plasma") || n.starts_with("gtk") || n.contains("kde") || n == "latte" || n == "xsettingsd")
}

pub fn scan_dirs(roots: &Roots) -> Vec<PathBuf> {
    vec![roots.config.clone(), roots.share.join("plasma")]
}

pub fn check(roots: &Roots, cfg: &Config) -> Result<Vec<Row>> {
    // (section id, resolved location, entry)
    let mut tracked: Vec<(String, PathBuf, String)> = Vec::new();
    for s in &cfg.sections {
        if let Ok(e) = expand_location(&s.location, roots) {
            tracked.extend(s.entries.iter().map(|en| (s.id.clone(), e.path.clone(), en.clone())));
        }
    }
    let mut rows = Vec::new();
    for dir in scan_dirs(roots) {
        let names: BTreeSet<String> = fs::read_dir(&dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        for name in &names {
            let child = dir.join(name);
            let mut sections = BTreeSet::new();
            let mut full = false;
            for (id, loc, entry) in &tracked {
                if let Ok(rel) = child.strip_prefix(loc) {
                    if entry_covers(entry, rel) {
                        full = true;
                        sections.insert(id.clone());
                    }
                } else if let Ok(rest) = loc.join(entry).strip_prefix(&child) {
                    // The child sits above a listed path
                    if !rest.as_os_str().is_empty() {
                        sections.insert(id.clone());
                    }
                }
            }
            let status = if full {
                Status::Tracked
            } else if sections.is_empty() {
                Status::Untracked
            } else {
                Status::Partial
            };
            rows.push(Row { dir: dir.clone(), name: name.clone(), status, sections: sections.into_iter().collect(), kde_like: looks_like_settings(name) });
        }
        for (id, loc, entry) in &tracked {
            let abs = loc.join(entry);
            if abs.parent() == Some(dir.as_path()) && !entry.contains(['*', '?']) && resolve_entry(loc, entry).is_ok_and(|v| v.is_empty()) {
                let name = Path::new(entry).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                match rows.iter_mut().find(|r| r.status == Status::Missing && r.dir == dir && r.name == name) {
                    Some(r) => r.sections.push(id.clone()),
                    None => rows.push(Row { dir: dir.clone(), name, status: Status::Missing, sections: vec![id.clone()], kde_like: true }),
                }
            }
        }
    }
    rows.sort_by(|a, b| (a.dir != b.dir, &a.dir, a.status, &a.name).cmp(&(false, &b.dir, b.status, &b.name)));
    Ok(rows)
}

/// Text table for the CLI. Untracked entries that look like settings are marked `!`; other
/// untracked ones (app data) only show with `all`.
pub fn render(rows: &[Row], all: bool) -> String {
    let mut s = String::new();
    let mut dir: Option<&Path> = None;
    for r in rows {
        if r.status == Status::Untracked && !r.kde_like && !all {
            continue;
        }
        if r.status == Status::Tracked && !all {
            continue;
        }
        if dir != Some(&r.dir) {
            s.push_str(&format!("\n{}\n", r.dir.display()));
            dir = Some(&r.dir);
        }
        let mark = if r.status == Status::Untracked && r.kde_like { '!' } else { ' ' };
        let by = if r.sections.is_empty() { String::new() } else { format!("  [{}]", r.sections.join(", ")) };
        s.push_str(&format!(" {mark} {:<44} {}{by}\n", r.name, r.status.label()));
    }
    let count = |st: Status| rows.iter().filter(|r| r.status == st).count();
    s.push_str(&format!(
        "\n{} tracked, {} partly listed, {} untracked ({} look like settings), {} listed but not on disk\n",
        count(Status::Tracked),
        count(Status::Partial),
        count(Status::Untracked),
        rows.iter().filter(|r| r.status == Status::Untracked && r.kde_like).count(),
        count(Status::Missing)
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::super::profile::testutil::home;
    use super::*;

    #[test]
    fn marks_tracked_untracked_partial_and_missing() {
        let (_d, r) = home();
        fs::write(r.config.join("mystery-rc"), "").unwrap();
        fs::write(r.config.join("somethingelse"), "").unwrap();
        let rows = check(&r, &Config::built_in()).unwrap();
        let find = |dir: &Path, n: &str| rows.iter().find(|x| x.dir == dir && x.name == n);
        assert_eq!(find(&r.config, "kdeglobals").unwrap().status, Status::Tracked);
        assert_eq!(find(&r.config, "gtk-3.0").unwrap().status, Status::Tracked);
        let m = find(&r.config, "mystery-rc").unwrap();
        assert_eq!((m.status, m.kde_like), (Status::Untracked, true));
        assert!(!find(&r.config, "somethingelse").unwrap().kde_like);
        assert_eq!(find(&r.config, "plasmarc").unwrap().status, Status::Missing);
        assert_eq!(find(&r.share.join("plasma"), "plasmoids").unwrap().status, Status::Tracked);
        let text = render(&rows, false);
        assert!(text.contains("! mystery-rc") && !text.contains("somethingelse") && text.contains("not on disk"));
        assert!(render(&rows, true).contains("somethingelse"));
    }

    #[test]
    fn a_folder_above_listed_entries_is_partly_listed() {
        let (_d, r) = home();
        fs::create_dir_all(r.share.join("plasma/desktoptheme")).unwrap();
        fs::create_dir_all(r.share.join("plasma/unlisted-thing")).unwrap();
        let rows = check(&r, &Config::built_in()).unwrap();
        let st = |d: PathBuf, n: &str| rows.iter().find(|x| x.dir == d && x.name == n).unwrap().status;
        assert_eq!(st(r.share.join("plasma"), "desktoptheme"), Status::Tracked);
        assert_eq!(st(r.share.join("plasma"), "unlisted-thing"), Status::Untracked);
    }
}
