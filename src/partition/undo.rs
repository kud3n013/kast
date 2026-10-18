//! What undoing a data partition step does, one action at a time. `undo.rs` plans the order
//! (home folders, ownership, unmount, fstab, mount point); these run each part.
//!
//! Nothing here deletes anything on the partition, and a mount is never unmounted with `-l`
//! or `-f`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Runner;
use super::apply::{busy_processes, needed, unit_name, unmount};
use super::fstab;
use crate::ctx;
use crate::journal;

/// Is a real filesystem (not just the autofs placeholder of an idle automount) mounted here?
pub fn really_mounted(rn: &dyn Runner, mp: &str) -> bool {
    let r = rn.run("findmnt", &["--mountpoint", mp, "-n", "-o", "FSTYPE"]);
    r.ok && r.stdout.lines().any(|l| !l.trim().is_empty() && l.trim() != "autofs")
}

pub fn anything_mounted(rn: &dyn Runner, mp: &str) -> bool {
    let r = rn.run("findmnt", &["--mountpoint", mp, "-n", "-o", "FSTYPE"]);
    r.ok && !r.stdout.trim().is_empty()
}

/// Before the undo touches anything: the partition must not be in use. A message naming the
/// processes when it is.
pub fn check_not_busy(rn: &dyn Runner, mp: &str) -> Result<()> {
    if !really_mounted(rn, mp) {
        return Ok(());
    }
    let r = rn.run("fuser", &["-m", mp]);
    // fuser exits 0 when something uses it
    if r.ok && !format!("{}{}", r.stdout, r.stderr).trim().is_empty() {
        bail!("{mp} is busy; these processes are using it:\n{}\nClose them and run the undo again. Nothing was undone.", busy_processes(rn, mp));
    }
    Ok(())
}

/// Stop the units and unmount. A busy mount names its processes and stops the undo.
pub fn unmount_partition(rn: &dyn Runner, mp: &str, automount: bool) -> Result<()> {
    if automount {
        let _ = rn.run("systemctl", &["stop", &unit_name(rn, mp, "automount")]);
        let _ = rn.run("systemctl", &["stop", &unit_name(rn, mp, "mount")]);
    }
    if anything_mounted(rn, mp) {
        unmount(rn, mp)?;
    }
    if anything_mounted(rn, mp) {
        bail!("{mp} is still mounted after unmounting");
    }
    Ok(())
}

/// Hand the mount root back (only while the partition really is mounted: otherwise the
/// folder under it would be changed instead).
pub fn chown_back(rn: &dyn Runner, path: &Path, mp: &str, uid: u32, gid: u32, mode: u32) -> Result<String> {
    let _ = fs::read_dir(path); // an idle automount mounts when touched
    if !really_mounted(rn, mp) {
        return Ok("the partition isn't mounted, so its top folder keeps its owner".into());
    }
    if !ctx::chown_path(path, uid, gid) {
        bail!("could not change the owner of {}", path.display());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_context(|| format!("restoring the mode of {}", path.display()))?;
    Ok(String::new())
}

/// Copy a moved folder back: the real folder replaces the symlink. Only if the symlink is
/// still the one kast made, and there is room at home. The copy on the partition stays.
pub fn move_back(rn: &dyn Runner, home: &Path, from: &Path, uid: u32, gid: u32, foreign: bool, free: &dyn Fn(&Path) -> Option<u64>) -> Result<String> {
    let _ = rn;
    match fs::read_link(home) {
        Ok(t) if t == from => {}
        Ok(t) => bail!("{} now points to {}, not to {}: left alone", home.display(), t.display(), from.display()),
        Err(_) => bail!("{} is no longer the link kast made: left alone", home.display()),
    }
    if !from.is_dir() {
        // Nothing to copy (the partition has no such folder): just drop the link
        fs::remove_file(home)?;
        return Ok(format!("{} had no folder on the partition; the link was removed", from.display()));
    }
    let size = super::apply::dir_size(from);
    let parent = home.parent().unwrap_or(Path::new("/"));
    if let Some(f) = free(parent) {
        if f < needed(size) / 4 + size {
            bail!(
                "not enough room in {} to copy {} back: it needs {}, {} is free. Free some space and run the undo again",
                parent.display(),
                home.display(),
                super::human(size),
                super::human(f)
            );
        }
    }
    let name = home.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!(".{name}.kast-restore"));
    let _ = fs::remove_dir_all(&tmp);
    let preserve = if foreign { "--preserve=timestamps" } else { "--preserve=mode,ownership,timestamps,links" };
    let mut cp = std::process::Command::new("cp");
    cp.args(["-RT", preserve]).arg(from).arg(&tmp).stdin(std::process::Stdio::null()).stdout(ctx::log_stdio()).stderr(ctx::log_stdio());
    if !crate::report::run_tracked(&mut cp).is_ok_and(|s| s.success()) {
        let _ = fs::remove_dir_all(&tmp);
        bail!("copying {} back failed; the link stays", from.display());
    }
    if foreign {
        ctx::chown_r(&tmp, &format!("{uid}:{gid}"));
    }
    ctx::chown_path(&tmp, uid, gid);
    fs::remove_file(home).with_context(|| format!("removing the link {}", home.display()))?;
    fs::rename(&tmp, home).with_context(|| format!("putting {} in place", home.display()))?;
    Ok(format!("{} is a real folder again; the copy on the partition ({}) can be removed by hand", home.display(), from.display()))
}

/// Put `/etc/fstab` right: the snapshot kast saved when it is unchanged, otherwise the file
/// as it is now minus kast's line and comment (plus the line kast replaced). The result is
/// checked with `findmnt --verify` first; then systemd reloads.
pub struct FstabRevert<'a> {
    pub path: &'a Path,
    pub line: &'a str,
    pub comment: &'a str,
    pub replaced: &'a [String],
    /// The file as it was before kast, when it is unchanged since (else None)
    pub snapshot: Option<Vec<u8>>,
}

pub fn fstab_revert(rn: &dyn Runner, r: &FstabRevert) -> Result<()> {
    let now = fs::read(r.path).with_context(|| format!("reading {}", r.path.display()))?;
    let new: Vec<u8> = match &r.snapshot {
        Some(s) => s.clone(),
        None => {
            let text = String::from_utf8(now.clone()).map_err(|_| anyhow::anyhow!("{} is not valid UTF-8; not touching it", r.path.display()))?;
            fstab::remove_entry(&text, r.line, r.comment, r.replaced).into_bytes()
        }
    };
    if new == now {
        return Ok(());
    }
    let mp = fstab::entries(&String::from_utf8_lossy(&now)).into_iter().find(|l| l.raw.trim_end() == r.line.trim_end()).map(|l| l.file).unwrap_or_default();
    let cand =
        tempfile::Builder::new().prefix("kast-fstab.").tempfile_in(r.path.parent().unwrap_or(Path::new("/tmp"))).or_else(|_| tempfile::NamedTempFile::new())?;
    fs::write(cand.path(), &new)?;
    let before = fstab::verify_file(rn, r.path);
    let after = fstab::verify_file(rn, cand.path());
    let v = fstab::verify_diff(&before, &after, &mp);
    if !v.errors.is_empty() {
        bail!("findmnt --verify found problems in the restored /etc/fstab, so it was not installed:\n{}", v.errors.join("\n"));
    }
    ctx::write_atomic(r.path, &new).with_context(|| format!("writing {}", r.path.display()))?;
    let dr = rn.run("systemctl", &["daemon-reload"]);
    if !dr.ok {
        bail!("systemctl daemon-reload failed: {}", dr.message());
    }
    Ok(())
}

/// Remove the folders kast made for the mount point, deepest first, only while empty.
/// Returns the ones left because they are not.
pub fn remove_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut kept = Vec::new();
    for d in dirs.iter().rev() {
        match fs::remove_dir(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => kept.push(d.clone()),
        }
    }
    kept
}

/// The journal's hash of a file, for the "unchanged since" check.
pub fn hash_of(p: &Path) -> Option<String> {
    journal::sha256_file(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Change, Run, Status};
    use crate::partition::apply::{Env, Plan, Who, apply};
    use crate::partition::fake::Fake;
    use crate::partition::fstab::Choices;
    use crate::partition::{Driver, Out, Partition};
    use crate::undo::{Act, Item, View, execute_with, plan};
    use serde_json::{Value, json};

    fn out(s: &str) -> Out {
        Out::success(s)
    }

    fn change(seq: u64, kind: &str, data: Value) -> Change {
        Change { seq, kind: kind.into(), step: "partition".into(), status: Status::Done, data, newer: false }
    }

    fn run(changes: Vec<Change>) -> Run {
        Run { id: "r1".into(), ts: String::new(), version: "0".into(), host: String::new(), user: "p3ter".into(), steps: vec!["partition".into()], changes }
    }

    fn file(seq: u64, path: &str, before: Option<&str>, after: &str) -> Change {
        let b = before.map_or(json!({ "absent": true }), |h| json!({ "sha256": h, "mode": 420, "uid": 0, "gid": 0 }));
        change(seq, if before.is_some() { "file_write" } else { "file_create" }, json!({ "path": path, "before": b, "after_sha256": after }))
    }

    fn labels(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    /// dir_create, fstab (entry + write), mount, chown, two folder moves, user-dirs.dirs
    fn full_run() -> Run {
        run(vec![
            change(1, "dir_create", json!({ "path": "/media/data", "created": ["/media", "/media/data"] })),
            change(
                2,
                "fstab_entry",
                json!({ "path": "/etc/fstab", "line": "UUID=X /media/data ntfs3 nofail 0 0", "comment": "# kast: data partition (Data), added 2026-10-18", "replaced": ["UUID=X /media/old ntfs3 defaults 0 0"] }),
            ),
            file(3, "/etc/fstab", Some("fstab-before"), "fstab-after"),
            change(4, "mount", json!({ "mountpoint": "/media/data", "unit": "media-data.mount", "mode": "boot" })),
            change(5, "chown_root", json!({ "path": "/media/data", "before": { "uid": 0, "gid": 0, "mode": 493 } })),
            change(
                6,
                "folder_move",
                json!({ "name": "Documents", "home": "/home/p/Documents", "new_path": "/media/data/Documents", "size": 100, "symlink_created": true, "uid": 1000, "gid": 1000 }),
            ),
            change(
                7,
                "folder_move",
                json!({ "name": "Videos", "home": "/home/p/Videos", "new_path": "/media/data/Videos", "size": 100, "symlink_created": true, "uid": 1000, "gid": 1000 }),
            ),
            file(8, "/home/p/.config/user-dirs.dirs", Some("xdg-before"), "xdg-after"),
        ])
    }

    fn view_for(r: &Run, fstab_now: &str) -> View {
        let mut v = View::default();
        v.files.insert(PathBuf::from("/etc/fstab"), Some(fstab_now.into()));
        v.files.insert(PathBuf::from("/home/p/.config/user-dirs.dirs"), Some("xdg-after".into()));
        v.mounted.insert("/media/data".into(), true);
        for (h, n) in [("/home/p/Documents", "Documents"), ("/home/p/Videos", "Videos")] {
            v.links.insert(PathBuf::from(h), Some(PathBuf::from(format!("/media/data/{n}"))));
        }
        v.dirs.insert("/media".into(), Some(true));
        v.dirs.insert("/media/data".into(), Some(true));
        let _ = r;
        v
    }

    #[test]
    fn undo_runs_in_reverse_with_the_exact_fstab_line() {
        let r = full_run();
        let items = plan(&r, &view_for(&r, "fstab-after"));
        let l = labels(&items);
        assert_eq!(l.len(), 7, "{l:?}");
        assert!(l[0].starts_with("Copy /home/p/Videos back") && l[1].starts_with("Copy /home/p/Documents back"), "{l:?}");
        assert!(l[2].starts_with("Restore /home/p/.config/user-dirs.dirs"), "user-dirs.dirs goes back after the folders: {l:?}");
        assert!(l[3].starts_with("Give /media/data back to 0:0 (mode 0755)"), "{l:?}");
        assert_eq!(l[4], "Unmount /media/data");
        assert_eq!(l[5], "Remove from /etc/fstab: UUID=X /media/data ntfs3 nofail 0 0 (and put back: UUID=X /media/old ntfs3 defaults 0 0)");
        assert!(l[6].starts_with("Remove the mount point /media/data"));
        assert!(items.iter().all(|i| i.undoable && i.default_on));
        // the fstab write is reverted by the same item
        assert_eq!(items[5].seqs, [2, 3]);
        assert!(matches!(&items[5].act, Act::FstabRevert { snapshot: Some(h), .. } if h == "fstab-before"));
    }

    #[test]
    fn an_fstab_edited_since_only_loses_kasts_lines() {
        let r = full_run();
        let items = plan(&r, &view_for(&r, "edited-by-hand"));
        let it = items.iter().find(|i| matches!(i.act, Act::FstabRevert { .. })).unwrap();
        assert!(matches!(&it.act, Act::FstabRevert { snapshot: None, .. }));
        assert!(it.warning.as_deref().unwrap().contains("only kast's line"));
        assert!(it.undoable && it.default_on);
        // already as before: nothing to do for fstab
        let items = plan(&r, &view_for(&r, "fstab-before"));
        assert!(!items.iter().any(|i| matches!(i.act, Act::FstabRevert { .. })));
        // pruned copies: the line is removed instead of restoring the snapshot
        let mut v = view_for(&r, "fstab-after");
        v.pruned.insert("r1".into());
        let items = plan(&r, &v);
        assert!(items.iter().any(|i| matches!(&i.act, Act::FstabRevert { snapshot: None, .. })));
    }

    #[test]
    fn folder_moves_are_refused_when_the_link_was_changed() {
        let r = full_run();
        let mut v = view_for(&r, "fstab-after");
        v.links.insert("/home/p/Documents".into(), Some("/somewhere/else".into()));
        v.links.insert("/home/p/Videos".into(), None);
        let items = plan(&r, &v);
        let moves: Vec<&Item> = items.iter().filter(|i| i.label.starts_with("Copy ")).collect();
        assert_eq!(moves.len(), 2);
        assert!(moves.iter().all(|i| !i.undoable && !i.default_on && i.warning.as_deref().unwrap().contains("no longer the link kast made")));
        let un = items.iter().find(|i| i.label == "Unmount /media/data").unwrap();
        assert!(un.warning.as_deref().unwrap().contains("still link"));
    }

    #[test]
    fn a_mount_point_with_content_is_kept() {
        let r = full_run();
        let mut v = view_for(&r, "fstab-after");
        v.dirs.insert("/media/data".into(), Some(false));
        let items = plan(&r, &v);
        let it = items.last().unwrap();
        assert!(!it.undoable && it.warning.as_deref() == Some("kept: it is not empty"));
        // gone already: no item
        v.dirs.insert("/media/data".into(), None);
        v.dirs.insert("/media".into(), None);
        assert!(!plan(&r, &v).iter().any(|i| i.label.starts_with("Remove the mount point")));
        // a mount that isn't there any more: no unmount
        let mut v = view_for(&r, "fstab-after");
        v.mounted.insert("/media/data".into(), false);
        assert!(!plan(&r, &v).iter().any(|i| i.label.starts_with("Unmount")));
    }

    #[test]
    fn a_failed_or_rolled_back_run_has_nothing_to_undo() {
        let mut r = full_run();
        for c in &mut r.changes {
            c.status = Status::Undone;
        }
        assert!(plan(&r, &view_for(&r, "fstab-after")).is_empty());
        let mut r = full_run();
        for c in r.changes.iter_mut().filter(|c| matches!(c.kind.as_str(), "mount" | "chown_root" | "dir_create")) {
            c.status = Status::Failed;
        }
        let l = labels(&plan(&r, &view_for(&r, "fstab-after")));
        assert!(!l.iter().any(|s| s.starts_with("Unmount") || s.starts_with("Give") || s.starts_with("Remove the mount point")), "{l:?}");
    }

    #[test]
    fn a_busy_mount_stops_the_undo_before_anything_is_touched() {
        let r = full_run();
        let items = plan(&r, &view_for(&r, "fstab-after"));
        let rn = Fake::new(vec![
            ("findmnt --mountpoint /media/data", out("ntfs3\n")),
            (
                "fuser",
                Out {
                    ok: true,
                    code: Some(0),
                    stdout: String::new(),
                    stderr: "                     USER        PID ACCESS COMMAND\n/media/data:         p3ter      4242 ..c.. bash".into(),
                },
            ),
        ]);
        let t = tempfile::tempdir().unwrap();
        let e = execute_with(&items, t.path(), "p3ter", &rn, &mut |_, _| {}).unwrap_err().to_string();
        assert!(e.contains("busy") && e.contains("4242") && e.contains("Nothing was undone"), "{e}");
        assert!(!rn.called("umount") && !rn.called("daemon-reload"));
    }

    #[test]
    fn a_mount_that_turns_busy_during_the_undo_stops_it_there() {
        let r = run(vec![file(1, "/etc/fstab", Some("a"), "b"), change(2, "mount", json!({ "mountpoint": "/media/data", "mode": "boot" }))]);
        let mut v = View::default();
        v.mounted.insert("/media/data".into(), true);
        v.files.insert("/etc/fstab".into(), Some("b".into()));
        let items = plan(&r, &v);
        let rn = Fake::new(vec![
            ("findmnt --mountpoint", out("ntfs3\n")),
            ("fuser", Out::failure("")), // idle at the pre-check
            ("umount /media/data", Out::failure("umount: /media/data: target is busy.")),
        ]);
        let t = tempfile::tempdir().unwrap();
        let e = execute_with(&items, t.path(), "p3ter", &rn, &mut |_, _| {}).unwrap_err().to_string();
        assert!(e.contains("busy") && e.contains("The undo stopped there"), "{e}");
        assert!(!rn.called("--verify"), "fstab was not touched");
        assert!(!rn.calls.borrow().iter().any(|c| c.contains("umount -l") || c.contains("umount -f")));
    }

    #[test]
    fn fstab_revert_removes_only_kasts_lines_and_checks_before_installing() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("fstab");
        let line = "UUID=X /media/data ntfs3 nofail 0 0";
        let comment = "# kast: data partition (Data), added 2026-10-18";
        let now = format!("# top\nUUID=a / ext4 defaults 0 1\n{comment}\n{line}\n# mine\n");
        fs::write(&path, &now).unwrap();
        let rn = Fake::new(vec![]);
        let r = FstabRevert { path: &path, line, comment, replaced: &["UUID=X /media/old ntfs3 defaults 0 0".to_string()], snapshot: None };
        fstab_revert(&rn, &r).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# top\nUUID=a / ext4 defaults 0 1\n# mine\nUUID=X /media/old ntfs3 defaults 0 0\n");
        assert!(rn.called("systemctl daemon-reload") && rn.count("--verify") == 2);
        // a failed check leaves the file alone
        fs::write(&path, &now).unwrap();
        let rn = Fake::new(vec![("kast-fstab.", Out { ok: false, code: Some(1), stdout: "/x\n   [E] boom\n".into(), stderr: String::new() })]);
        let e = fstab_revert(&rn, &r).unwrap_err().to_string();
        assert!(e.contains("not installed") && e.contains("boom"));
        assert_eq!(fs::read_to_string(&path).unwrap(), now);
        assert!(!rn.called("daemon-reload"));
        // the snapshot wins when given
        let rn = Fake::new(vec![]);
        fstab_revert(&rn, &FstabRevert { snapshot: Some(b"orig\n".to_vec()), ..r }).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "orig\n");
    }

    #[test]
    fn a_folder_comes_back_and_the_copy_on_the_partition_stays() {
        let t = tempfile::tempdir().unwrap();
        let (home, part) = (t.path().join("home"), t.path().join("part"));
        fs::create_dir_all(part.join("Documents/sub")).unwrap();
        fs::write(part.join("Documents/sub/a.txt"), "doc").unwrap();
        fs::create_dir_all(&home).unwrap();
        let link = home.join("Documents");
        std::os::unix::fs::symlink(part.join("Documents"), &link).unwrap();
        let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        let rn = Fake::new(vec![]);
        // changed link: refused
        let other = home.join("Other");
        std::os::unix::fs::symlink("/tmp", &other).unwrap();
        assert!(move_back(&rn, &other, &part.join("Documents"), uid, gid, true, &|_| None).unwrap_err().to_string().contains("points to"));
        // no room: refused, the link stays
        let e = move_back(&rn, &link, &part.join("Documents"), uid, gid, true, &|_| Some(10)).unwrap_err().to_string();
        assert!(e.contains("not enough room"), "{e}");
        assert!(link.is_symlink());
        // fine
        let note = move_back(&rn, &link, &part.join("Documents"), uid, gid, true, &|_| Some(1 << 40)).unwrap();
        assert!(note.contains("can be removed by hand"));
        assert!(!link.is_symlink() && link.is_dir());
        assert_eq!(fs::read_to_string(link.join("sub/a.txt")).unwrap(), "doc");
        assert_eq!(fs::read_to_string(part.join("Documents/sub/a.txt")).unwrap(), "doc", "nothing on the partition is deleted");
        assert!(!home.join(".Documents.kast-restore").exists());
    }

    #[test]
    fn the_top_folder_is_only_chowned_while_the_partition_is_mounted() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("mp");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        let not = Fake::new(vec![("findmnt", Out::failure(""))]);
        assert!(chown_back(&not, &d, "/media/data", uid, gid, 0o755).unwrap().contains("isn't mounted"));
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700, "the folder under the mount is not touched");
        let idle = Fake::new(vec![("findmnt", out("autofs\n"))]);
        assert!(chown_back(&idle, &d, "/media/data", uid, gid, 0o755).unwrap().contains("isn't mounted"));
        let mounted = Fake::new(vec![("findmnt", out("ntfs3\n"))]);
        assert_eq!(chown_back(&mounted, &d, "/media/data", uid, gid, 0o755).unwrap(), "");
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn mount_point_folders_go_deepest_first_and_only_when_empty() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("media"), t.path().join("media/data"));
        fs::create_dir_all(&b).unwrap();
        assert!(remove_dirs(&[a.clone(), b.clone()]).is_empty());
        assert!(!a.exists());
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("f"), "x").unwrap();
        assert_eq!(remove_dirs(&[a.clone(), b.clone()]), [b.clone(), a.clone()]);
        assert!(b.join("f").exists());
    }

    // --- The whole way there and back ------------------------------------------------------

    const UUID: &str = "6C62ADEA62ADB96A";
    const FSTAB: &str =
        "# my fstab\nUUID=D0A3-F918 /boot vfat defaults 0 2\nUUID=6C62ADEA62ADB96A /media/old ntfs3 defaults 0 0\nUUID=2720fb5d / btrfs subvol=/@ 0 0";

    #[test]
    fn apply_then_undo_leaves_the_machine_as_it_was() {
        crate::ctx::init_for_tests();
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/fstab"), FSTAB).unwrap();
        let home = root.join("home/p3ter");
        fs::create_dir_all(home.join("Documents/sub")).unwrap();
        fs::write(home.join("Documents/sub/a.txt"), "doc").unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        let xdg = "XDG_DOCUMENTS_DIR=\"$HOME/Documents\"\n";
        fs::write(home.join(".config/user-dirs.dirs"), xdg).unwrap();
        let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        let part =
            Partition { path: "/dev/sdb1".into(), fstype: Some("ntfs".into()), uuid: Some(UUID.into()), label: Some("Data".into()), ..Default::default() };
        let mut plan_ = Plan {
            choices: Choices::new(&part, Driver::Ntfs3, "/media/data", uid, gid),
            part,
            replace: vec![3],
            unmount_first: false,
            folders: vec!["Documents".into()],
            who: Who { user: "p3ter".into(), uid, gid, home: home.clone() },
            date: "2026-10-18".into(),
        };
        plan_.choices.uid = uid;
        let mounted_json = format!(r#"{{"filesystems":[{{"fstype":"ntfs3","options":"rw,uid={uid},gid={gid},fmask=0177,dmask=0077","uuid":"{UUID}"}}]}}"#);
        let rn = Fake::new(vec![("blkid -p", out(&format!("TYPE=ntfs\nUUID={UUID}\n"))), ("findmnt --target", out(&mounted_json))]);
        let run_dir = root.join("run");
        let env = Env { rn: &rn, probe_base: run_dir, progress: &|_| {}, free: &crate::partition::apply::avail };

        let (items, outcome) = crate::journal::with_root(&root, || {
            crate::journal::begin_run(&["partition"], "p3ter").unwrap();
            apply(&plan_, &env).unwrap();
            crate::journal::end_run();
            let runs = crate::journal::runs(&crate::journal::load(&crate::journal::state_dir()).unwrap().entries);
            let r = &runs[0];
            let mut v = crate::undo::gather(&[r]);
            v.mounted.insert("/media/data".into(), true);
            let items = plan(r, &v);
            // the sandboxed paths are what the journal recorded
            let mp = "/media/data";
            let rn2 = Fake::new(vec![("kast-fstab.", out(""))]).then(&format!("findmnt --mountpoint {mp}"), vec![out("ntfs3\n"), out("ntfs3\n"), out("")]);
            let o = execute_with(&items, &crate::journal::state_dir(), "p3ter", &rn2, &mut |_, _| {}).unwrap();
            assert!(rn2.called("umount ") && rn2.called("daemon-reload"), "{:?}", rn2.calls.borrow());
            (items, o)
        });
        assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
        let l = labels(&items);
        assert_eq!(l.len(), 5, "{l:?}"); // folder, user-dirs, unmount, fstab, mount point (no chown on ntfs)
        assert_eq!(fs::read(root.join("etc/fstab")).unwrap(), FSTAB.as_bytes(), "fstab as before");
        assert!(home.join("Documents").is_dir() && !home.join("Documents").is_symlink());
        assert_eq!(fs::read_to_string(home.join("Documents/sub/a.txt")).unwrap(), "doc");
        assert_eq!(fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap(), xdg);
        assert!(
            root.join("media/data/Documents/sub/a.txt").exists(),
            "the copy on the partition stays, so its mount point is kept (here the partition is a plain folder)"
        );
        assert!(outcome.notes.iter().any(|n| n.contains("can be removed by hand")));
        // everything is marked undone: nothing left to offer
        crate::journal::with_root(&root, || {
            let runs = crate::journal::runs(&crate::journal::load(&crate::journal::state_dir()).unwrap().entries);
            let r = runs.iter().find(|r| r.steps == ["partition"]).unwrap();
            assert!(
                r.changes.iter().filter(|c| c.kind != "dir_create").all(|c| c.status == Status::Undone),
                "{:?}",
                r.changes.iter().map(|c| (&c.kind, c.status)).collect::<Vec<_>>()
            );
        });
    }
}
