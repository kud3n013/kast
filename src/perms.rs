//! Ownership and owner-write checks for files kast restores into a user's home.
//!
//! KDE shows `Configuration file "..." not writable` when a config file is root-owned or
//! lacks owner write. [`scan`] finds such files (never following symlinks, never leaving the
//! folders it is allowed to look at), [`repair`] fixes them and returns every failure, so no
//! `chown`/`chmod` error is ever dropped silently.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use nix::fcntl::{AT_FDCWD, AtFlags};
use nix::unistd::{Gid, Uid, fchownat};
use walkdir::WalkDir;

/// A file or folder the user cannot (fully) use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    /// Owned by the wanted user and group
    pub owned: bool,
    /// Owner read and write (and execute for folders)
    pub usable: bool,
    pub link: bool,
    pub dir: bool,
}

impl Finding {
    pub fn describe(&self, uid: u32, gid: u32) -> String {
        let mut what = Vec::new();
        if !self.owned {
            what.push(format!("owned by {}:{} (want {uid}:{gid})", self.uid, self.gid));
        }
        if !self.usable {
            what.push(format!("mode {:o} lacks owner {}", self.mode & 0o7777, if self.dir { "rwx" } else { "rw" }));
        }
        format!("{}: {}", self.path.display(), what.join(", "))
    }
}

fn inspect(path: &Path, uid: u32, gid: u32) -> Option<Finding> {
    let m = path.symlink_metadata().ok()?;
    let link = m.file_type().is_symlink();
    let dir = m.is_dir();
    // The group only counts when we could change it: a non-root run in a setgid folder
    // creates files in the folder's group, which the user can still write
    let owned = m.uid() == uid && (m.gid() == gid || !Uid::effective().is_root());
    // A symlink's own mode is meaningless; what it points to is not ours to judge
    let need = if dir { 0o700 } else { 0o600 };
    let usable = link || m.mode() & need == need;
    (!owned || !usable).then(|| Finding { path: path.to_path_buf(), uid: m.uid(), gid: m.gid(), mode: m.mode(), owned, usable, link, dir })
}

fn walk(path: &Path, uid: u32, gid: u32) -> Vec<Finding> {
    WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
        .filter_map(|e| inspect(e.path(), uid, gid))
        .collect()
}

/// Everything below `path` (a file, link or folder) that is not owned by `uid:gid` or lacks
/// owner read/write. `path` must lie inside one of `jails`; links are reported, not followed.
pub fn scan(jails: &[PathBuf], path: &Path, uid: u32, gid: u32) -> Result<Vec<Finding>> {
    if !jails.iter().any(|j| path.starts_with(j)) || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("{} is outside the folders kast may repair", path.display());
    }
    Ok(walk(path, uid, gid))
}

/// What a repair did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repaired {
    pub fixed: usize,
    pub failed: Vec<(PathBuf, String)>,
}

/// `chown -h` without following the final symlink.
pub fn lchown(path: &Path, uid: u32, gid: u32) -> Result<(), nix::errno::Errno> {
    fchownat(AT_FDCWD, path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)), AtFlags::AT_SYMLINK_NOFOLLOW)
}

fn sudo_chown(paths: &[&Path], uid: u32, gid: u32) -> Result<(), String> {
    for chunk in paths.chunks(200) {
        let st = Command::new("sudo")
            .args(["-n", "chown", "-h", &format!("{uid}:{gid}"), "--"])
            .args(chunk)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("sudo: {e}"))?;
        if !st.success() {
            return Err("sudo -n chown failed (needs passwordless sudo, or run kast with sudo)".into());
        }
    }
    Ok(())
}

/// Fix `findings`: chown to `uid:gid` (directly as root; through `sudo -n` when
/// `sudo_fallback`; otherwise reported as failed), then add owner rw (rwx for folders).
pub fn repair(findings: &[Finding], uid: u32, gid: u32, sudo_fallback: bool) -> Repaired {
    let mut out = Repaired::default();
    let is_root = Uid::effective().is_root();
    let unowned: Vec<&Path> = findings.iter().filter(|f| !f.owned).map(|f| f.path.as_path()).collect();
    let mut chown_failed: Vec<(PathBuf, String)> = Vec::new();
    if !unowned.is_empty() {
        if is_root {
            for p in &unowned {
                if let Err(e) = lchown(p, uid, gid) {
                    chown_failed.push((p.to_path_buf(), format!("chown: {e}")));
                }
            }
        } else if sudo_fallback {
            if let Err(e) = sudo_chown(&unowned, uid, gid) {
                chown_failed.extend(unowned.iter().map(|p| (p.to_path_buf(), e.clone())));
            }
        } else {
            chown_failed.extend(unowned.iter().map(|p| (p.to_path_buf(), "chown needs root".to_string())));
        }
    }
    for f in findings {
        if let Some((_, e)) = chown_failed.iter().find(|(p, _)| *p == f.path) {
            out.failed.push((f.path.clone(), e.clone()));
            continue;
        }
        if !f.usable && !f.link {
            let need = if f.dir { 0o700 } else { 0o600 };
            let mode = (f.mode & 0o7777) | need;
            if let Err(e) = std::fs::set_permissions(&f.path, std::os::unix::fs::PermissionsExt::from_mode(mode)) {
                out.failed.push((f.path.clone(), format!("chmod {mode:o}: {e}")));
                continue;
            }
        }
        out.fixed += 1;
    }
    out
}

/// Hand a whole tree to `uid:gid` with owner rw(x), links untouched. No jail: for kast's own
/// staging folders and snapshots. Returns every failure.
pub fn fix_tree(root: &Path, uid: u32, gid: u32) -> Vec<(PathBuf, String)> {
    repair(&walk(root, uid, gid), uid, gid, false).failed
}

/// Tests that look at the shared test home take turns.
#[cfg(test)]
pub static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn me() -> (u32, u32) {
        (Uid::effective().as_raw(), Gid::effective().as_raw())
    }

    #[test]
    fn finds_and_fixes_a_file_without_owner_write() {
        let t = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let rc = t.path().join("spectaclerc");
        fs::write(&rc, "x").unwrap();
        fs::set_permissions(&rc, fs::Permissions::from_mode(0o444)).unwrap();
        let sub = t.path().join("d");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
        let f = scan(&[t.path().to_path_buf()], t.path(), uid, gid).unwrap();
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|x| x.owned && !x.usable));
        let r = repair(&f, uid, gid, false);
        assert_eq!((r.fixed, r.failed.len()), (2, 0));
        assert_eq!(fs::metadata(&rc).unwrap().permissions().mode() & 0o600, 0o600);
        assert_eq!(fs::metadata(&sub).unwrap().permissions().mode() & 0o700, 0o700);
        assert!(scan(&[t.path().to_path_buf()], t.path(), uid, gid).unwrap().is_empty());
    }

    #[test]
    fn a_wrong_owner_is_reported_not_swallowed_when_it_cannot_be_fixed() {
        if Uid::effective().is_root() {
            eprintln!("skipped: root can chown");
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        fs::write(t.path().join("a"), "x").unwrap();
        // pretend the user is somebody else
        let f = scan(&[t.path().to_path_buf()], &t.path().join("a"), uid + 1, gid).unwrap();
        assert_eq!(f.len(), 1);
        assert!(!f[0].owned && f[0].usable);
        let r = repair(&f, uid + 1, gid, false);
        assert_eq!(r.fixed, 0);
        assert_eq!(r.failed.len(), 1);
        assert!(r.failed[0].1.contains("chown needs root"));
    }

    #[test]
    fn symlinks_are_never_followed_and_the_jail_is_enforced() {
        let t = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let home = t.path().join("home");
        let outside = t.path().join("outside");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("secret");
        fs::write(&victim, "x").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o400)).unwrap();
        symlink(&outside, home.join("dirlink")).unwrap();
        symlink(&victim, home.join("filelink")).unwrap();
        let f = scan(std::slice::from_ref(&home), &home, uid, gid).unwrap();
        assert!(f.is_empty(), "links to unusable targets are not findings: {f:?}");
        // a link as the scan root is not followed either
        assert!(scan(std::slice::from_ref(&home), &home.join("dirlink"), uid, gid).unwrap().is_empty());
        assert_eq!(fs::metadata(&victim).unwrap().permissions().mode() & 0o777, 0o400, "target untouched");
        // outside the jail: refused
        assert!(scan(std::slice::from_ref(&home), &outside, uid, gid).is_err());
        assert!(scan(std::slice::from_ref(&home), &home.join("../outside"), uid, gid).is_err());
        // a link owned by someone else is a finding, but only the link itself
        let f = scan(std::slice::from_ref(&home), &home.join("filelink"), uid + 1, gid).unwrap();
        assert_eq!(f.len(), 1);
        assert!(f[0].link);
        let r = repair(&f, uid + 1, gid, false);
        assert_eq!(fs::metadata(&victim).unwrap().permissions().mode() & 0o777, 0o400);
        if !Uid::effective().is_root() {
            assert_eq!(r.failed.len(), 1);
        }
    }

    #[test]
    fn fix_tree_makes_a_staging_tree_usable() {
        let t = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        fs::create_dir_all(t.path().join("kde/config")).unwrap();
        fs::write(t.path().join("kde/config/spectaclerc"), "x").unwrap();
        fs::set_permissions(t.path().join("kde/config/spectaclerc"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(t.path().join("kde/config"), fs::Permissions::from_mode(0o000)).unwrap();
        // the walk needs to read the folder: give back the minimum first, as extraction does
        fs::set_permissions(t.path().join("kde/config"), fs::Permissions::from_mode(0o500)).unwrap();
        assert!(fix_tree(&t.path().join("kde"), uid, gid).is_empty());
        assert_eq!(fs::metadata(t.path().join("kde/config/spectaclerc")).unwrap().permissions().mode() & 0o600, 0o600);
    }
}
