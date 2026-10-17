//! Packages a profile needs: themes, icon themes, Kvantum themes and plasmoids it refers to
//! that live in system folders. Best effort: a name is only recorded when `pacman -Qo` can
//! say which package owns it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// icon-theme, color-scheme, plasma-theme, look-and-feel, kvantum-theme, gtk-theme, plasmoid
    pub kind: String,
    pub name: String,
    pub package: String,
}

/// What pacman knows. A trait so tests need no pacman.
pub trait PkgOwner {
    /// The package that owns a file or folder
    fn owner(&self, path: &Path) -> Option<String>;
    fn installed(&self, pkg: &str) -> bool;
    /// In a repository pacman can install it from (official, CachyOS or archrepo)
    fn available(&self, pkg: &str) -> bool;
}

pub struct Pacman;

fn pacman_ok(args: &[&str]) -> Option<String> {
    let out = Command::new("pacman").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl PkgOwner for Pacman {
    fn owner(&self, path: &Path) -> Option<String> {
        let p = path.to_string_lossy();
        pacman_ok(&["-Qqo", &p]).and_then(|o| o.lines().next().map(str::to_string)).filter(|s| !s.is_empty())
    }

    fn installed(&self, pkg: &str) -> bool {
        pacman_ok(&["-Qq", pkg]).is_some()
    }

    fn available(&self, pkg: &str) -> bool {
        pacman_ok(&["-Si", pkg]).is_some()
    }
}

/// `[section]` + `key=value` lookup in an INI file (KDE's rc files, GTK's settings.ini).
pub fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name == section;
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            let v = v.trim();
            return (!v.is_empty()).then(|| v.to_string());
        }
    }
    None
}

fn find_saved(files: &Path, rel: &str) -> Option<String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(files).ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.iter().find_map(|d| fs::read_to_string(d.join(rel)).ok())
}

/// Where a named thing would be installed by a package.
fn candidates(kind: &str, name: &str, share: &Path) -> Vec<PathBuf> {
    match kind {
        "icon-theme" => vec![share.join("icons").join(name)],
        "color-scheme" => vec![share.join("color-schemes").join(format!("{name}.colors"))],
        "plasma-theme" => vec![share.join("plasma/desktoptheme").join(name)],
        "look-and-feel" => vec![share.join("plasma/look-and-feel").join(name)],
        "kvantum-theme" => vec![share.join("Kvantum").join(name)],
        "gtk-theme" => vec![share.join("themes").join(name)],
        "plasmoid" => vec![share.join("plasma/plasmoids").join(name)],
        _ => vec![],
    }
}

/// Read the saved settings under `files` (`<section>/<entry>`) and name the packages
/// providing the themes and plasmoids they use. `share` is `/usr/share` outside tests.
pub fn detect(files: &Path, share: &Path, pkgs: &dyn PkgOwner) -> Vec<Requirement> {
    let mut wanted: Vec<(&str, String)> = Vec::new();
    if let Some(t) = find_saved(files, "kdeglobals") {
        for (kind, section, key) in
            [("icon-theme", "Icons", "Theme"), ("color-scheme", "General", "ColorScheme"), ("look-and-feel", "KDE", "LookAndFeelPackage")]
        {
            wanted.extend(ini_value(&t, section, key).map(|v| (kind, v)));
        }
    }
    if let Some(t) = find_saved(files, "plasmarc") {
        wanted.extend(ini_value(&t, "Theme", "name").map(|v| ("plasma-theme", v)));
    }
    if let Some(t) = find_saved(files, "Kvantum/kvantum.kvconfig") {
        wanted.extend(ini_value(&t, "General", "theme").map(|v| ("kvantum-theme", v)));
    }
    if let Some(t) = find_saved(files, "gtk-3.0/settings.ini") {
        wanted.extend(ini_value(&t, "Settings", "gtk-theme-name").map(|v| ("gtk-theme", v)));
        wanted.extend(ini_value(&t, "Settings", "gtk-icon-theme-name").map(|v| ("icon-theme", v)));
    }
    if let Some(t) = find_saved(files, "plasma-org.kde.plasma.desktop-appletsrc") {
        for line in t.lines() {
            if let Some(id) = line.trim().strip_prefix("plugin=")
                && !id.is_empty()
            {
                wanted.push(("plasmoid", id.to_string()));
            }
        }
    }
    let mut out: Vec<Requirement> = Vec::new();
    for (kind, name) in wanted {
        if name.contains('/') || out.iter().any(|r| r.kind == kind && r.name == name) {
            continue;
        }
        let package = candidates(kind, &name, share).iter().filter(|p| p.symlink_metadata().is_ok()).find_map(|p| pkgs.owner(p));
        if let Some(package) = package {
            out.push(Requirement { kind: kind.to_string(), name, package });
        }
    }
    out
}

/// The packages among `reqs` that are not installed, without repeats, in order.
pub fn missing(reqs: &[Requirement], pkgs: &dyn PkgOwner) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in reqs {
        if !out.contains(&r.package) && !pkgs.installed(&r.package) {
            out.push(r.package.clone());
        }
    }
    out
}

/// Split into what a repository can install and what it cannot (AUR packages, say).
pub fn split_installable(names: &[String], pkgs: &dyn PkgOwner) -> (Vec<String>, Vec<String>) {
    names.iter().cloned().partition(|n| pkgs.available(n))
}

#[cfg(test)]
pub struct FakePkgs {
    pub owners: Vec<(PathBuf, &'static str)>,
    pub installed: Vec<&'static str>,
    pub available: Vec<&'static str>,
}

#[cfg(test)]
impl PkgOwner for FakePkgs {
    fn owner(&self, path: &Path) -> Option<String> {
        self.owners.iter().find(|(p, _)| p == path).map(|(_, n)| n.to_string())
    }
    fn installed(&self, pkg: &str) -> bool {
        self.installed.contains(&pkg)
    }
    fn available(&self, pkg: &str) -> bool {
        self.available.contains(&pkg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_lookup_is_per_section() {
        let t = "[General]\nColorScheme=A\n[Icons]\nTheme = Papirus\nColorScheme=B\n";
        assert_eq!(ini_value(t, "General", "ColorScheme").as_deref(), Some("A"));
        assert_eq!(ini_value(t, "Icons", "Theme").as_deref(), Some("Papirus"));
        assert_eq!(ini_value(t, "Icons", "Missing"), None);
    }

    #[test]
    fn detects_themes_and_plasmoids_with_owners() {
        let d = tempfile::tempdir().unwrap();
        let files = d.path().join("files");
        fs::create_dir_all(files.join("appearance")).unwrap();
        fs::create_dir_all(files.join("panels")).unwrap();
        fs::write(files.join("appearance/kdeglobals"), "[Icons]\nTheme=Papirus-Dark\n[General]\nColorScheme=Nord\n").unwrap();
        fs::write(files.join("appearance/plasmarc"), "[Theme]\nname=Layan\n").unwrap();
        fs::write(
            files.join("panels/plasma-org.kde.plasma.desktop-appletsrc"),
            "[Containments][1]\nplugin=org.kde.panel\n[Applets][2]\nplugin=org.kde.plasma.kickoff\nplugin=org.kde.plasma.kickoff\n",
        )
        .unwrap();
        let share = d.path().join("usr/share");
        fs::create_dir_all(share.join("icons/Papirus-Dark")).unwrap();
        fs::create_dir_all(share.join("plasma/plasmoids/org.kde.plasma.kickoff")).unwrap();
        fs::create_dir_all(share.join("plasma/desktoptheme/Layan")).unwrap();
        let pk = FakePkgs {
            owners: vec![
                (share.join("icons/Papirus-Dark"), "papirus-icon-theme"),
                (share.join("plasma/plasmoids/org.kde.plasma.kickoff"), "plasma-desktop"),
                (share.join("plasma/desktoptheme/Layan"), "layan-kde-git"),
            ],
            installed: vec!["plasma-desktop"],
            available: vec!["papirus-icon-theme"],
        };
        let reqs = detect(&files, &share, &pk);
        let pairs: Vec<(&str, &str)> = reqs.iter().map(|r| (r.kind.as_str(), r.package.as_str())).collect();
        assert_eq!(pairs, vec![("icon-theme", "papirus-icon-theme"), ("plasma-theme", "layan-kde-git"), ("plasmoid", "plasma-desktop")]);
        let miss = missing(&reqs, &pk);
        assert_eq!(miss, vec!["papirus-icon-theme".to_string(), "layan-kde-git".to_string()]);
        let (ok, no) = split_installable(&miss, &pk);
        assert_eq!((ok, no), (vec!["papirus-icon-theme".to_string()], vec!["layan-kde-git".to_string()]));
    }
}
