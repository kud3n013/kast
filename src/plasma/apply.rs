// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! `apply`: build one [`Plan`] of what would be created, overwritten or skipped, then
//! snapshot the files it overwrites and execute exactly that plan. `--dry-run` only prints
//! the plan, so it can never disagree with a real run.
//!
//! Where files go comes from the *local* list, never from the profile: a profile (or an
//! imported archive) can only restore entries the local list names, below the local
//! locations. Nothing is ever deleted.

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

use super::config::{Category, Config, Kind};
use super::fsops;
use super::paths::{Roots, entry_covers, expand_location, safe_rel};
use super::profile::{self, Ambient, Manifest, Profile, ProfileKind, StoredSection};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Create,
    Overwrite,
    Unchanged,
    Skip(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    File,
    Link(PathBuf),
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub section: String,
    pub category: Category,
    pub section_kind: Kind,
    /// Path below the section's location
    pub rel: PathBuf,
    pub src: PathBuf,
    pub dest: PathBuf,
    pub kind: ItemKind,
    pub action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub profile: String,
    pub items: Vec<PlanItem>,
    /// Problems with whole sections or entries (not in the local list, location not found)
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub create: usize,
    pub overwrite: usize,
    pub unchanged: usize,
    pub skip: usize,
}

impl Plan {
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for i in &self.items {
            match i.action {
                Action::Create => c.create += 1,
                Action::Overwrite => c.overwrite += 1,
                Action::Unchanged => c.unchanged += 1,
                Action::Skip(_) => c.skip += 1,
            }
        }
        c.skip += self.notes.len();
        c
    }

    pub fn has_changes(&self) -> bool {
        let c = self.counts();
        c.create + c.overwrite > 0
    }

    /// The dry-run listing. Unchanged files are counted, not listed.
    pub fn render(&self) -> String {
        let mut s = String::new();
        for i in &self.items {
            let (tag, why) = match &i.action {
                Action::Create => ("create   ", String::new()),
                Action::Overwrite => ("overwrite", String::new()),
                Action::Unchanged => continue,
                Action::Skip(r) => ("skip     ", format!("  ({r})")),
            };
            let _ = writeln!(s, "  {tag} {}{why}", i.dest.display());
        }
        for n in &self.notes {
            let _ = writeln!(s, "  skip      {n}");
        }
        let c = self.counts();
        let _ = write!(s, "{} to create, {} to overwrite, {} unchanged, {} skipped", c.create, c.overwrite, c.unchanged, c.skip);
        s
    }
}

/// Lexically: does a symlink's target stay below `root` when the link sits at `link`?
pub(super) fn link_stays_inside(root: &Path, link: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut cur: Vec<Component> = link.parent().unwrap_or(root).strip_prefix(root).map(|p| p.components().collect()).unwrap_or_default();
    for c in target.components() {
        match c {
            Component::Normal(_) => cur.push(c),
            Component::ParentDir => {
                if cur.pop().is_none() {
                    return false;
                }
            }
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

/// A parent between the location and the destination that exists but is not a folder.
fn blocked_parent(loc: &Path, dest: &Path) -> Option<PathBuf> {
    let mut p = dest.parent()?;
    while p != loc && p.starts_with(loc) {
        if p.symlink_metadata().is_ok() {
            return (!p.is_dir()).then(|| p.to_path_buf());
        }
        p = p.parent()?;
    }
    None
}

fn classify(loc: &Path, dest: &Path, kind: &ItemKind, src: &Path) -> Action {
    if let Some(b) = blocked_parent(loc, dest) {
        return Action::Skip(format!("{} is not a folder", b.display()));
    }
    let Ok(m) = dest.symlink_metadata() else { return Action::Create };
    if m.is_dir() {
        return Action::Skip("a folder is in the way".into());
    }
    match kind {
        ItemKind::Link(t) => {
            if fs::read_link(dest).is_ok_and(|cur| &cur == t) { Action::Unchanged } else { Action::Overwrite }
        }
        ItemKind::File => {
            if m.is_file() && fsops::same_content(src, dest) { Action::Unchanged } else { Action::Overwrite }
        }
    }
}

/// Plan the apply of `profile`, optionally only for some categories.
pub fn plan(roots: &Roots, cfg: &Config, profile: &Profile, only: Option<&[Category]>) -> Result<Plan> {
    let mut plan = Plan { profile: profile.name.clone(), ..Default::default() };
    let files = profile.files_dir();
    for stored in &profile.manifest.sections {
        if only.is_some_and(|o| !o.contains(&stored.category)) {
            continue;
        }
        let Some(sec) = cfg.section(&stored.id).filter(|s| s.kind == stored.kind) else {
            plan.notes.push(format!("section {:?}: not in your Plasma list, so kast will not write it", stored.id));
            continue;
        };
        let loc = match expand_location(&sec.location, roots) {
            Ok(e) => e.path,
            Err(e) => {
                plan.notes.push(format!("section {:?}: {e:#}", stored.id));
                continue;
            }
        };
        for entry in &stored.entries {
            let Ok(rel_entry) = safe_rel(entry) else {
                plan.notes.push(format!("{}/{entry}: unsafe path", stored.id));
                continue;
            };
            if !sec.entries.iter().any(|p| entry_covers(p, &rel_entry)) {
                plan.notes.push(format!("{}/{entry}: not an entry of your Plasma list", stored.id));
                continue;
            }
            let sec_dir = files.join(&stored.id);
            for rel in fsops::list_files(&sec_dir, &rel_entry) {
                let src = sec_dir.join(&rel);
                let dest = loc.join(&rel);
                let Ok(m) = src.symlink_metadata() else { continue };
                let kind = if m.file_type().is_symlink() {
                    let t = fs::read_link(&src)?;
                    ItemKind::Link(t)
                } else if m.is_file() {
                    ItemKind::File
                } else {
                    plan.items.push(PlanItem { section: stored.id.clone(), category: stored.category, section_kind: stored.kind, rel, src, dest, kind: ItemKind::File, action: Action::Skip("not a regular file".into()) });
                    continue;
                };
                let action = match &kind {
                    ItemKind::Link(t) if !link_stays_inside(&loc, &dest, t) => Action::Skip(format!("link to {} points outside", t.display())),
                    _ => classify(&loc, &dest, &kind, &src),
                };
                plan.items.push(PlanItem { section: stored.id.clone(), category: stored.category, section_kind: stored.kind, rel, src, dest, kind, action });
            }
            if fsops::list_files(&sec_dir, &rel_entry).is_empty() && !sec_dir.join(&rel_entry).is_dir() {
                plan.notes.push(format!("{}/{entry}: listed in the profile but its files are missing", stored.id));
            }
        }
    }
    Ok(plan)
}

#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub created: usize,
    pub overwritten: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
    /// Name of the snapshot taken before overwriting (restore it with `apply <name>`)
    pub snapshot: Option<String>,
}

pub fn snapshot_name(amb: &Ambient) -> String {
    format!("pre-apply-{}", amb.now.format("%Y%m%d-%H%M%S"))
}

/// Store the files the plan overwrites as a profile of kind snapshot. Only existing files
/// are in it: what an apply creates is not removed when the snapshot is restored.
fn take_snapshot(roots: &Roots, plan: &Plan, amb: &Ambient) -> Result<Option<String>> {
    let overwrites: Vec<&PlanItem> = plan.items.iter().filter(|i| i.action == Action::Overwrite).collect();
    if overwrites.is_empty() {
        return Ok(None);
    }
    let mut name = snapshot_name(amb);
    let mut n = 1;
    while profile::profile_dir(roots, &name)?.symlink_metadata().is_ok() {
        n += 1;
        name = format!("{}-{n}", snapshot_name(amb));
    }
    let stage = profile::new_stage(roots, ".snapshot-")?;
    let built = (|| -> Result<()> {
        let mut manifest = Manifest::default();
        let mut warnings = Vec::new();
        for it in overwrites {
            let sec_dir = stage.join("files").join(&it.section);
            fsops::copy_tree(it.dest.parent().expect("dest has a parent"), Path::new(it.dest.file_name().expect("dest has a name")), &sec_dir.join(it.rel.parent().unwrap_or(Path::new(""))), roots.owner, &mut warnings)?;
            let rel = it.rel.to_string_lossy().into_owned();
            match manifest.sections.iter_mut().find(|s| s.id == it.section) {
                Some(s) => s.entries.push(rel),
                None => manifest.sections.push(StoredSection { id: it.section.clone(), category: it.category, kind: it.section_kind, entries: vec![rel] }),
            }
        }
        profile::install_staged(roots, &stage, &name, &manifest, &amb.meta(ProfileKind::Snapshot, vec![]), false)
    })();
    if let Err(e) = built {
        let _ = fs::remove_dir_all(&stage);
        bail!("could not take the safety snapshot, nothing was changed: {e:#}");
    }
    Ok(Some(name))
}

/// Execute a plan: snapshot first (a failure aborts before anything is written), then
/// write every create/overwrite atomically. Errors on single files are collected.
pub fn execute(roots: &Roots, plan: &Plan, snapshot: bool, amb: &Ambient) -> Result<ApplyReport> {
    let mut rep = ApplyReport::default();
    if snapshot {
        rep.snapshot = take_snapshot(roots, plan, amb)?;
    }
    for it in &plan.items {
        let r = match (&it.action, &it.kind) {
            (Action::Create | Action::Overwrite, ItemKind::File) => fsops::copy_file_atomic(&it.src, &it.dest, roots.owner),
            (Action::Create | Action::Overwrite, ItemKind::Link(t)) => fsops::symlink_atomic(t, &it.dest, roots.owner),
            (Action::Unchanged, _) => {
                rep.unchanged += 1;
                continue;
            }
            (Action::Skip(_), _) => {
                rep.skipped += 1;
                continue;
            }
        };
        match r {
            Ok(()) if it.action == Action::Create => rep.created += 1,
            Ok(()) => rep.overwritten += 1,
            Err(e) => rep.failed.push(format!("{}: {e}", it.dest.display())),
        }
    }
    rep.skipped += plan.notes.len();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::super::profile::testutil::*;
    use super::super::profile::{SaveOpts, load, save};
    use super::*;
    use std::os::unix::fs::symlink;

    fn saved() -> (tempfile::TempDir, Roots, Config) {
        let (d, r) = home();
        let cfg = Config::built_in();
        save(&r, &cfg, "p", SaveOpts { assets: true, ..Default::default() }, &ambient()).unwrap();
        (d, r, cfg)
    }

    #[test]
    fn round_trip_restores_changed_and_deleted_files() {
        let (_d, r, cfg) = saved();
        let before = fs::read_to_string(r.config.join("kdeglobals")).unwrap();
        fs::write(r.config.join("kdeglobals"), "[General]\nColorScheme=Other\n").unwrap();
        fs::remove_file(r.config.join("kwinrc")).unwrap();
        fs::write(r.config.join("untracked-rc"), "mine").unwrap();
        let p = load(&r, "p").unwrap();
        let plan = plan(&r, &cfg, &p, None).unwrap();
        let c = plan.counts();
        assert_eq!((c.create, c.overwrite), (1, 1), "{}", plan.render());
        assert!(c.unchanged >= 4);
        let rep = execute(&r, &plan, true, &ambient()).unwrap();
        assert_eq!((rep.created, rep.overwritten), (1, 1));
        assert!(rep.failed.is_empty());
        assert_eq!(fs::read_to_string(r.config.join("kdeglobals")).unwrap(), before);
        assert!(r.config.join("kwinrc").is_file());
        assert_eq!(fs::read_to_string(r.config.join("untracked-rc")).unwrap(), "mine", "nothing is deleted");
    }

    #[test]
    fn snapshot_then_restore_undoes_an_apply() {
        let (_d, r, cfg) = saved();
        fs::write(r.config.join("kdeglobals"), "[General]\nColorScheme=Current\n").unwrap();
        fs::write(r.config.join("gtk-3.0/settings.ini"), "current gtk").unwrap();
        let p = load(&r, "p").unwrap();
        let pl = plan(&r, &cfg, &p, None).unwrap();
        let rep = execute(&r, &pl, true, &ambient()).unwrap();
        let snap = rep.snapshot.clone().expect("a snapshot was taken");
        assert_eq!(snap, "pre-apply-20261018-123005");
        assert_eq!(fs::read_to_string(r.config.join("kdeglobals")).unwrap(), "[General]\nColorScheme=Nord\n");
        let sp = load(&r, &snap).unwrap();
        assert_eq!(sp.meta.kind, ProfileKind::Snapshot);
        // Undo: apply the snapshot
        let pl = plan(&r, &cfg, &sp, None).unwrap();
        assert_eq!(pl.counts().overwrite, 2, "{}", pl.render());
        execute(&r, &pl, false, &ambient()).unwrap();
        assert_eq!(fs::read_to_string(r.config.join("kdeglobals")).unwrap(), "[General]\nColorScheme=Current\n");
        assert_eq!(fs::read_to_string(r.config.join("gtk-3.0/settings.ini")).unwrap(), "current gtk");
    }

    #[test]
    fn same_second_snapshots_get_distinct_names() {
        let (_d, r, cfg) = saved();
        let p = load(&r, "p").unwrap();
        for _ in 0..2 {
            fs::write(r.config.join("kdeglobals"), "x").unwrap();
            let pl = plan(&r, &cfg, &p, None).unwrap();
            execute(&r, &pl, true, &ambient()).unwrap();
        }
        let names: Vec<String> = profile::list(&r).unwrap().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"pre-apply-20261018-123005".to_string()) && names.contains(&"pre-apply-20261018-123005-2".to_string()), "{names:?}");
    }

    #[test]
    fn dry_run_plan_matches_what_execute_does_and_writes_nothing() {
        let (_d, r, cfg) = saved();
        fs::write(r.config.join("kwinrc"), "changed").unwrap();
        let p = load(&r, "p").unwrap();
        let pl = plan(&r, &cfg, &p, None).unwrap();
        let text = pl.render();
        assert!(text.contains("overwrite") && text.contains("kwinrc"), "{text}");
        assert_eq!(fs::read_to_string(r.config.join("kwinrc")).unwrap(), "changed");
        assert!(profile::list(&r).unwrap().iter().all(|s| s.name == "p"), "planning takes no snapshot");
    }

    #[test]
    fn categories_filter_the_plan() {
        let (_d, r, cfg) = saved();
        fs::write(r.config.join("kglobalshortcutsrc"), "x").unwrap();
        fs::write(r.config.join("kwinrc"), "x").unwrap();
        let p = load(&r, "p").unwrap();
        let only = [Category::Shortcuts];
        let pl = plan(&r, &cfg, &p, Some(&only)).unwrap();
        assert!(pl.items.iter().all(|i| i.category == Category::Shortcuts));
        assert_eq!(pl.counts().overwrite, 1);
        execute(&r, &pl, true, &ambient()).unwrap();
        assert_eq!(fs::read_to_string(r.config.join("kwinrc")).unwrap(), "x", "other categories untouched");
    }

    #[test]
    fn profile_cannot_write_outside_the_local_list() {
        let (_d, r, cfg) = saved();
        let mut p = load(&r, "p").unwrap();
        // A hostile profile: an entry the list does not cover, a section it does not know
        fs::create_dir_all(p.files_dir().join("appearance/.ssh")).unwrap();
        fs::write(p.files_dir().join("appearance/.ssh/authorized_keys"), "evil").unwrap();
        fs::create_dir_all(p.files_dir().join("evil")).unwrap();
        fs::write(p.files_dir().join("evil/x"), "evil").unwrap();
        p.manifest.sections[0].entries.push(".ssh".into());
        p.manifest.sections.push(StoredSection { id: "evil".into(), category: Category::Apps, kind: super::super::config::Kind::Save, entries: vec!["x".into()] });
        let pl = plan(&r, &cfg, &p, None).unwrap();
        assert!(pl.items.iter().all(|i| !i.dest.to_string_lossy().contains(".ssh") && i.section != "evil"));
        assert!(pl.notes.iter().any(|n| n.contains(".ssh")) && pl.notes.iter().any(|n| n.contains("evil")));
        execute(&r, &pl, true, &ambient()).unwrap();
        assert!(!r.home.join(".ssh").exists() && !r.config.join(".ssh").exists());
    }

    #[test]
    fn folders_in_the_way_and_escaping_links_are_skipped() {
        let (_d, r, cfg) = saved();
        let p = load(&r, "p").unwrap();
        fs::remove_file(r.config.join("kwinrc")).unwrap();
        fs::create_dir_all(r.config.join("kwinrc/dir")).unwrap();
        let pl = plan(&r, &cfg, &p, None).unwrap();
        let it = pl.items.iter().find(|i| i.dest.ends_with("kwinrc")).unwrap();
        assert!(matches!(it.action, Action::Skip(_)));
        // a link leaving the location
        symlink("../../etc/passwd", p.files_dir().join("windows/kwinrulesrc")).unwrap();
        let mut p2 = p.clone();
        p2.manifest.sections.iter_mut().find(|s| s.id == "windows").unwrap().entries.push("kwinrulesrc".into());
        let pl = plan(&r, &cfg, &p2, None).unwrap();
        let it = pl.items.iter().find(|i| i.dest.ends_with("kwinrulesrc")).unwrap();
        assert!(matches!(&it.action, Action::Skip(m) if m.contains("outside")), "{:?}", it.action);
        assert!(!r.config.join("kwinrulesrc").exists());
    }

    #[test]
    fn link_check() {
        let root = Path::new("/h/.config");
        assert!(link_stays_inside(root, &root.join("a/l"), Path::new("../b")));
        assert!(link_stays_inside(root, &root.join("l"), Path::new("a/b")));
        assert!(!link_stays_inside(root, &root.join("l"), Path::new("../x")));
        assert!(!link_stays_inside(root, &root.join("a/l"), Path::new("../../x")));
        assert!(!link_stays_inside(root, &root.join("l"), Path::new("/etc")));
    }

    #[test]
    fn applying_creates_missing_parent_folders() {
        let (_d, r, cfg) = saved();
        fs::remove_dir_all(r.config.join("gtk-3.0")).unwrap();
        let p = load(&r, "p").unwrap();
        let pl = plan(&r, &cfg, &p, None).unwrap();
        execute(&r, &pl, true, &ambient()).unwrap();
        assert!(r.config.join("gtk-3.0/settings.ini").is_file());
    }
}
