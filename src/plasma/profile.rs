// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! Profiles on disk and `save`.
//!
//! ```text
//! <profiles>/<name>/
//!   meta.toml        kast and Plasma version, host, date, required packages
//!   profile.toml     which sections and entries were saved (no locations: those always
//!                    come from the local list when a profile is applied)
//!   files/<section id>/<entry>
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::config::{Category, Config, Kind, Section};
use super::deps::{self, PkgOwner, Requirement};
use super::fsops::{self, Owner};
use super::paths::{Roots, expand_location, resolve_entry, safe_rel, valid_profile_name};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSection {
    pub id: String,
    pub category: Category,
    pub kind: Kind,
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "section")]
    pub sections: Vec<StoredSection>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    #[default]
    Profile,
    /// Made by `apply` from the files it was about to overwrite
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub kast_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plasma_version: Option<String>,
    pub hostname: String,
    pub created: String,
    /// The kast commit whose built-in list this profile was made with
    pub default_list_commit: String,
    #[serde(default)]
    pub kind: ProfileKind,
    #[serde(default, rename = "requires", skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Requirement>,
}

/// What a run needs from the machine it runs on besides the files (injected in tests).
pub struct Ambient {
    pub now: chrono::DateTime<chrono::Local>,
    pub hostname: String,
    pub plasma_version: Option<String>,
    pub commit: String,
    /// `/usr/share` outside tests
    pub system_share: PathBuf,
    pub pkgs: Box<dyn PkgOwner>,
}

impl Ambient {
    pub fn detect() -> Self {
        Ambient {
            now: chrono::Local::now(),
            hostname: nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|_| "unknown".into()),
            plasma_version: super::reload::plasma_version(),
            commit: option_env!("KAST_COMMIT").unwrap_or("unknown").to_string(),
            system_share: PathBuf::from("/usr/share"),
            pkgs: Box::new(deps::Pacman),
        }
    }

    pub fn meta(&self, kind: ProfileKind, requires: Vec<Requirement>) -> Meta {
        Meta {
            kast_version: env!("CARGO_PKG_VERSION").to_string(),
            plasma_version: self.plasma_version.clone(),
            hostname: self.hostname.clone(),
            created: self.now.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            default_list_commit: self.commit.clone(),
            kind,
            requires,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
    pub meta: Meta,
    pub manifest: Manifest,
}

impl Profile {
    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    pub fn categories(&self) -> Vec<Category> {
        let mut c: Vec<Category> = self.manifest.sections.iter().map(|s| s.category).collect();
        c.sort();
        c.dedup();
        c
    }
}

pub fn profile_dir(roots: &Roots, name: &str) -> Result<PathBuf> {
    if !valid_profile_name(name) {
        bail!("{name:?} is not a valid profile name (letters, digits, - _ . ; at most 64 characters; no leading dot)");
    }
    Ok(roots.profiles.join(name))
}

/// Check a manifest read from disk: it may come from an archive.
pub fn validate_manifest(m: &Manifest) -> Result<()> {
    for s in &m.sections {
        if s.id.is_empty() || !s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            bail!("section id {:?} is not valid", s.id);
        }
        for e in &s.entries {
            safe_rel(e).with_context(|| format!("section {}", s.id))?;
        }
    }
    Ok(())
}

pub fn load(roots: &Roots, name: &str) -> Result<Profile> {
    let dir = profile_dir(roots, name)?;
    if !dir.join("profile.toml").is_file() {
        bail!("no profile named {name:?}");
    }
    let manifest: Manifest = toml::from_str(&fs::read_to_string(dir.join("profile.toml"))?).with_context(|| format!("{name}: profile.toml"))?;
    validate_manifest(&manifest).with_context(|| format!("{name}: profile.toml"))?;
    let meta: Meta = toml::from_str(&fs::read_to_string(dir.join("meta.toml")).with_context(|| format!("{name}: meta.toml"))?)
        .with_context(|| format!("{name}: meta.toml"))?;
    Ok(Profile { name: name.to_string(), dir, meta, manifest })
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub name: String,
    pub meta: Option<Meta>,
    pub files: usize,
    pub bytes: u64,
    pub categories: Vec<Category>,
}

/// Profiles and snapshots, by name.
pub fn list(roots: &Roots) -> Result<Vec<Summary>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(&roots.profiles) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| roots.profiles.display().to_string()),
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let (meta, categories) = match load(roots, &name) {
            Ok(p) => (Some(p.meta.clone()), p.categories()),
            Err(_) => (None, vec![]),
        };
        let files = fsops::list_files(&e.path(), Path::new("files")).len();
        out.push(Summary { name, meta, files, bytes: fsops::tree_size(&e.path().join("files")), categories });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn remove(roots: &Roots, name: &str) -> Result<()> {
    let dir = profile_dir(roots, name)?;
    match dir.symlink_metadata() {
        Ok(m) if m.is_dir() && dir.join("profile.toml").is_file() => {}
        Ok(_) => bail!("{name:?} is not a kast profile folder; not removing it"),
        Err(_) => bail!("no profile named {name:?}"),
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
}

#[derive(Debug, Default, Clone)]
pub struct Captured {
    pub sections: Vec<StoredSection>,
    /// `section/entry` listed in the config but not present on this machine
    pub missing: Vec<String>,
    pub warnings: Vec<String>,
    pub files: usize,
}

/// Copy the entries of the sections `want` accepts from their locations into
/// `dest_files/<section id>/`.
pub fn capture(roots: &Roots, cfg: &Config, dest_files: &Path, want: &dyn Fn(&Section) -> bool) -> Result<Captured> {
    let mut cap = Captured::default();
    for s in cfg.sections.iter().filter(|s| want(s)) {
        let loc = match expand_location(&s.location, roots) {
            Ok(e) => {
                cap.warnings.extend(e.warnings);
                e.path
            }
            Err(e) => {
                cap.warnings.push(format!("section {} skipped: {e:#}", s.id));
                continue;
            }
        };
        let mut stored = Vec::new();
        for entry in &s.entries {
            let found = match resolve_entry(&loc, entry) {
                Ok(f) => f,
                Err(e) => {
                    cap.warnings.push(format!("{}: {e:#}", s.id));
                    continue;
                }
            };
            if found.is_empty() {
                cap.missing.push(format!("{}/{entry}", s.id));
            }
            for rel in found {
                match fsops::copy_tree(&loc, &rel, &dest_files.join(&s.id), roots.owner, &mut cap.warnings) {
                    Ok(n) => {
                        cap.files += n;
                        stored.push(rel.to_string_lossy().into_owned());
                        note_links(&loc, &rel, &mut cap.warnings);
                    }
                    Err(e) => bail!("copying {}: {e}", loc.join(&rel).display()),
                }
            }
        }
        if !stored.is_empty() {
            cap.sections.push(StoredSection { id: s.id.clone(), category: s.category, kind: s.kind, entries: stored });
        }
    }
    Ok(cap)
}

fn note_links(loc: &Path, rel: &Path, warnings: &mut Vec<String>) {
    if let Ok(target) = fs::read_link(loc.join(rel)) {
        warnings.push(format!("{} is a symlink to {}; saved as a link, not its content", rel.display(), target.display()));
    }
}

/// Lines that look like `password=...` or `token=...` in the saved settings (the values are
/// never printed). Widgets such as calendars keep credentials in appletsrc (konsave #79).
pub fn scan_secrets(files: &Path) -> Vec<String> {
    let key_re = Regex::new(r"(?i)(passw(or)?d|passwd|secret|token|api[-_]?key|credential)").expect("valid regex");
    let mut out = Vec::new();
    for rel in fsops::list_files(files, Path::new("")) {
        let p = files.join(&rel);
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if !m.is_file() || m.len() > 1 << 20 {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else { continue };
        let mut keys: Vec<String> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(k, v)| !v.trim().is_empty() && key_re.is_match(k))
            .map(|(k, _)| k.trim().to_string())
            .collect();
        keys.sort();
        keys.dedup();
        if !keys.is_empty() {
            let shown = rel.components().skip(1).collect::<PathBuf>();
            out.push(format!("{}: {} setting(s) look like credentials ({}); check before sharing this profile", shown.display(), keys.len(), keys.join(", ")));
        }
    }
    out
}

/// Write `profile.toml` and `meta.toml` into a staging folder and put it in place as
/// `<profiles>/<name>`. An existing profile is replaced (never merged) only with `force`.
pub fn install_staged(roots: &Roots, stage: &Path, name: &str, manifest: &Manifest, meta: &Meta, force: bool) -> Result<()> {
    let dest = profile_dir(roots, name)?;
    fs::write(stage.join("profile.toml"), toml::to_string(manifest)?)?;
    fs::write(stage.join("meta.toml"), toml::to_string(meta)?)?;
    if let Some((u, g)) = roots.owner {
        crate::ctx::chown_path(stage, u, g);
        for f in ["profile.toml", "meta.toml"] {
            crate::ctx::chown_path(&stage.join(f), u, g);
        }
    }
    if dest.symlink_metadata().is_err() {
        return fs::rename(stage, &dest).with_context(|| format!("creating {}", dest.display()));
    }
    if !force {
        bail!("profile {name:?} already exists (use --force to replace it)");
    }
    let old = roots.profiles.join(format!(".old-{name}-{}", std::process::id()));
    fs::rename(&dest, &old).with_context(|| format!("replacing {}", dest.display()))?;
    if let Err(e) = fs::rename(stage, &dest) {
        let _ = fs::rename(&old, &dest);
        return Err(e).with_context(|| format!("replacing {}", dest.display()));
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

pub fn new_stage(roots: &Roots, prefix: &str) -> Result<PathBuf> {
    fsops::mkdir_owned(&roots.profiles, roots.owner).with_context(|| format!("creating {}", roots.profiles.display()))?;
    Ok(tempfile::Builder::new().prefix(prefix).tempdir_in(&roots.profiles)?.keep())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SaveOpts<'a> {
    pub force: bool,
    /// Also store the large assets (the `export` sections: themes, plasmoids, icons)
    pub assets: bool,
    pub only: Option<&'a [Category]>,
}

#[derive(Debug, Clone)]
pub struct SaveReport {
    pub name: String,
    pub missing: Vec<String>,
    pub warnings: Vec<String>,
    pub requires: Vec<Requirement>,
    pub files: usize,
    pub bytes: u64,
}

/// `save`: capture the entries of the list that exist into a new profile. The profile is
/// built beside the final folder and renamed into place, so a failure leaves nothing
/// half-written and `--force` replaces the old profile instead of merging into it.
pub fn save(roots: &Roots, cfg: &Config, name: &str, opts: SaveOpts, amb: &Ambient) -> Result<SaveReport> {
    let dest = profile_dir(roots, name)?;
    if dest.symlink_metadata().is_ok() && !opts.force {
        bail!("profile {name:?} already exists (use --force to replace it)");
    }
    let stage = new_stage(roots, ".saving-")?;
    let result = (|| -> Result<SaveReport> {
        let files = stage.join("files");
        fsops::mkdir_owned(&files, roots.owner)?;
        let want = |s: &Section| (s.kind == Kind::Save || opts.assets) && opts.only.is_none_or(|o| o.contains(&s.category));
        let mut cap = capture(roots, cfg, &files, &want)?;
        if cap.sections.is_empty() {
            bail!(
                "nothing to save: none of the listed Plasma files exist here{}",
                if cap.warnings.is_empty() { String::new() } else { format!(" ({})", cap.warnings.join("; ")) }
            );
        }
        cap.warnings.extend(scan_secrets(&files));
        let requires = deps::detect(&files, &amb.system_share, &*amb.pkgs);
        let manifest = Manifest { sections: cap.sections.clone() };
        install_staged(roots, &stage, name, &manifest, &amb.meta(ProfileKind::Profile, requires.clone()), opts.force)?;
        Ok(SaveReport {
            name: name.to_string(),
            missing: cap.missing,
            warnings: cap.warnings,
            requires,
            files: cap.files,
            bytes: fsops::tree_size(&dest.join("files")),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

#[allow(dead_code)]
fn _owner_is_used(_: Owner) {}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use crate::plasma::deps::FakePkgs;

    pub fn ambient() -> Ambient {
        let now = chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 18, 12, 30, 5).unwrap();
        Ambient {
            now,
            hostname: "testhost".into(),
            plasma_version: Some("6.8.0".into()),
            commit: "abc1234".into(),
            system_share: PathBuf::from("/nonexistent-share"),
            pkgs: Box::new(FakePkgs { owners: vec![], installed: vec![], available: vec![] }),
        }
    }

    /// A fake home with a few Plasma files.
    pub fn home() -> (tempfile::TempDir, Roots) {
        let d = tempfile::tempdir().unwrap();
        let r = Roots::under_home(d.path());
        fs::create_dir_all(r.config.join("gtk-3.0")).unwrap();
        fs::create_dir_all(r.config.join("autostart")).unwrap();
        fs::write(r.config.join("kdeglobals"), "[General]\nColorScheme=Nord\n").unwrap();
        fs::write(r.config.join("kglobalshortcutsrc"), "[kwin]\nExpose=Meta+W\n").unwrap();
        fs::write(r.config.join("plasma-org.kde.plasma.desktop-appletsrc"), "[Containments][1]\nplugin=org.kde.panel\n").unwrap();
        fs::write(r.config.join("kwinrc"), "[Windows]\nBorderSnapZone=20\n").unwrap();
        fs::write(r.config.join("gtk-3.0/settings.ini"), "[Settings]\ngtk-theme-name=Breeze\n").unwrap();
        fs::write(r.config.join("autostart/app.desktop"), "[Desktop Entry]\n").unwrap();
        fs::create_dir_all(r.share.join("plasma/plasmoids/my.widget")).unwrap();
        fs::write(r.share.join("plasma/plasmoids/my.widget/main.qml"), "Item {}").unwrap();
        fs::create_dir_all(r.share.join("color-schemes")).unwrap();
        fs::write(r.share.join("color-schemes/Nord.colors"), "[General]\n").unwrap();
        (d, r)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn save_stores_small_settings_and_lists_the_profile() {
        let (_d, r) = home();
        let cfg = Config::built_in();
        let rep = save(&r, &cfg, "work", SaveOpts::default(), &ambient()).unwrap();
        assert!(rep.files >= 6, "{rep:?}");
        let p = load(&r, "work").unwrap();
        assert_eq!(p.meta.hostname, "testhost");
        assert_eq!(p.meta.plasma_version.as_deref(), Some("6.8.0"));
        assert_eq!(p.meta.default_list_commit, "abc1234");
        assert_eq!(p.meta.kind, ProfileKind::Profile);
        assert_eq!(fs::read_to_string(p.files_dir().join("appearance/kdeglobals")).unwrap(), "[General]\nColorScheme=Nord\n");
        assert!(p.files_dir().join("gtk/gtk-3.0/settings.ini").is_file());
        assert!(!p.files_dir().join("plasmoids").exists(), "assets are not stored without --assets");
        let l = list(&r).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].name, "work");
        assert!(l[0].categories.contains(&Category::Gtk));
    }

    #[test]
    fn assets_and_category_filter() {
        let (_d, r) = home();
        let cfg = Config::built_in();
        save(&r, &cfg, "all", SaveOpts { assets: true, ..Default::default() }, &ambient()).unwrap();
        let p = load(&r, "all").unwrap();
        assert!(p.files_dir().join("plasmoids/plasma/plasmoids/my.widget/main.qml").is_file());
        assert!(p.files_dir().join("themes/color-schemes/Nord.colors").is_file());
        let only = [Category::Shortcuts];
        save(&r, &cfg, "keys", SaveOpts { only: Some(&only), ..Default::default() }, &ambient()).unwrap();
        let p = load(&r, "keys").unwrap();
        assert_eq!(p.manifest.sections.len(), 1);
        assert_eq!(p.manifest.sections[0].id, "shortcuts");
        assert!(!p.files_dir().join("appearance").exists());
    }

    #[test]
    fn existing_profile_needs_force_and_force_replaces_instead_of_merging() {
        let (_d, r) = home();
        let cfg = Config::built_in();
        save(&r, &cfg, "p", SaveOpts::default(), &ambient()).unwrap();
        assert!(save(&r, &cfg, "p", SaveOpts::default(), &ambient()).is_err());
        fs::remove_file(r.config.join("kwinrc")).unwrap();
        save(&r, &cfg, "p", SaveOpts { force: true, ..Default::default() }, &ambient()).unwrap();
        let p = load(&r, "p").unwrap();
        assert!(!p.files_dir().join("windows/kwinrc").exists(), "stale file must not survive --force");
        let leftovers: Vec<String> = fs::read_dir(&r.profiles).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(leftovers, vec!["p".to_string()], "no staging folders left behind");
    }

    #[test]
    fn nothing_to_save_is_an_error_and_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let r = Roots::under_home(d.path());
        assert!(save(&r, &Config::built_in(), "empty", SaveOpts::default(), &ambient()).is_err());
        assert!(list(&r).unwrap().is_empty());
        assert!(fs::read_dir(&r.profiles).map(|rd| rd.count() == 0).unwrap_or(true));
    }

    #[test]
    fn bad_names_never_touch_the_disk() {
        let (_d, r) = home();
        for n in ["../evil", "a/b", ".hidden", ""] {
            assert!(save(&r, &Config::built_in(), n, SaveOpts::default(), &ambient()).is_err());
            assert!(remove(&r, n).is_err());
            assert!(load(&r, n).is_err());
        }
    }

    #[test]
    fn remove_deletes_only_profiles() {
        let (_d, r) = home();
        save(&r, &Config::built_in(), "gone", SaveOpts::default(), &ambient()).unwrap();
        remove(&r, "gone").unwrap();
        assert!(list(&r).unwrap().is_empty());
        assert!(remove(&r, "gone").is_err());
        fs::create_dir_all(r.profiles.join("notaprofile")).unwrap();
        assert!(remove(&r, "notaprofile").is_err());
        assert!(r.profiles.join("notaprofile").is_dir());
    }

    #[test]
    fn symlinks_are_saved_as_links_with_a_warning() {
        let (_d, r) = home();
        fs::remove_file(r.config.join("kwinrc")).unwrap();
        std::os::unix::fs::symlink("/somewhere/dotfiles/kwinrc", r.config.join("kwinrc")).unwrap();
        let rep = save(&r, &Config::built_in(), "l", SaveOpts::default(), &ambient()).unwrap();
        let p = load(&r, "l").unwrap();
        assert_eq!(fs::read_link(p.files_dir().join("windows/kwinrc")).unwrap(), PathBuf::from("/somewhere/dotfiles/kwinrc"));
        assert!(rep.warnings.iter().any(|w| w.contains("symlink")));
    }

    #[test]
    fn credentials_are_flagged_without_printing_values() {
        let (_d, r) = home();
        fs::write(r.config.join("plasma-org.kde.plasma.desktop-appletsrc"), "[Applets][3][Configuration]\ncalendarToken=SECRETVALUE\nname=x\n").unwrap();
        let rep = save(&r, &Config::built_in(), "s", SaveOpts::default(), &ambient()).unwrap();
        let w = rep.warnings.iter().find(|w| w.contains("credentials")).expect("a credentials warning");
        assert!(w.contains("calendarToken") && !w.contains("SECRETVALUE"));
    }
}
