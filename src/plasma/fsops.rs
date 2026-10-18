//! File operations the module is built on: atomic writes, symlinks kept as symlinks, and
//! ownership handed to the user when kast runs as root.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use filetime::FileTime;
use walkdir::WalkDir;

use crate::ctx::{chown_path, lchown_path};

pub type Owner = Option<(u32, u32)>;

/// `mkdir -p` that gives every folder it creates to `owner`.
pub fn mkdir_owned(dir: &Path, owner: Owner) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut p = dir;
    while p.symlink_metadata().is_err() {
        missing.push(p.to_path_buf());
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => break,
        }
    }
    fs::create_dir_all(dir)?;
    if let Some((u, g)) = owner {
        for d in &missing {
            chown_path(d, u, g);
        }
    }
    Ok(())
}

/// Write `src`'s content to `dst` through a temp file in the same folder, then rename it
/// over. Keeps the mode and modification time. `dst` is replaced, never written through.
pub fn copy_file_atomic(src: &Path, dst: &Path, owner: Owner) -> io::Result<()> {
    let parent = dst.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    mkdir_owned(parent, owner)?;
    let meta = fs::metadata(src)?;
    let mut input = File::open(src)?;
    let mut tmp = tempfile::Builder::new().prefix(".kast-tmp-").tempfile_in(parent)?;
    io::copy(&mut input, tmp.as_file_mut())?;
    tmp.as_file().sync_all()?;
    // Owner read/write always: a profile file saved 0444 must not make the restored one
    // read-only (KDE: `Configuration file ... not writable`)
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode((meta.mode() & 0o777) | 0o600))?;
    filetime::set_file_mtime(tmp.path(), FileTime::from_last_modification_time(&meta))?;
    if let Some((u, g)) = owner {
        chown_path(tmp.path(), u, g);
    }
    tmp.persist(dst).map_err(|e| e.error)?;
    Ok(())
}

/// Create (or replace) the symlink `dst -> target` by renaming a new link over it.
pub fn symlink_atomic(target: &Path, dst: &Path, owner: Owner) -> io::Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    let parent = dst.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    mkdir_owned(parent, owner)?;
    let tmp = parent.join(format!(".kast-link-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    symlink(target, &tmp)?;
    if let Some((u, g)) = owner {
        lchown_path(&tmp, u, g);
    }
    fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Copy `src_root/rel` (a file, link or folder tree) to `dst_root/rel`. Symlinks are
/// copied as symlinks and never followed; sockets, devices and pipes are skipped with a
/// warning. Returns the number of files and links copied.
pub fn copy_tree(src_root: &Path, rel: &Path, dst_root: &Path, owner: Owner, warnings: &mut Vec<String>) -> io::Result<usize> {
    let mut n = 0;
    for entry in WalkDir::new(src_root.join(rel)).follow_links(false).follow_root_links(false).sort_by_file_name() {
        let entry = entry.map_err(io::Error::other)?;
        let sub = entry.path().strip_prefix(src_root).expect("walk stays below its root");
        let dst = dst_root.join(sub);
        let ft = entry.file_type();
        if ft.is_dir() {
            mkdir_owned(&dst, owner)?;
        } else if ft.is_symlink() {
            symlink_atomic(&fs::read_link(entry.path())?, &dst, owner)?;
            n += 1;
        } else if ft.is_file() {
            copy_file_atomic(entry.path(), &dst, owner)?;
            n += 1;
        } else {
            warnings.push(format!("{}: not a regular file, skipped", sub.display()));
        }
    }
    Ok(n)
}

/// Equal content (sizes first, then bytes).
pub fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else { return false };
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (File::open(a), File::open(b)) else { return false };
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (Ok(na), Ok(nb)) = (read_full(&mut fa, &mut ba), read_full(&mut fb, &mut bb)) else { return false };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// Files and links below a path (not following links), relative to `root`, sorted.
pub fn list_files(root: &Path, rel: &Path) -> Vec<PathBuf> {
    WalkDir::new(root.join(rel))
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
        .filter(|e| !e.file_type().is_dir())
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .collect()
}

/// Total size of the files below a path.
pub fn tree_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_copy_keeps_mode_and_mtime_and_replaces_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        fs::write(&src, "new").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
        filetime::set_file_mtime(&src, FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
        let other = d.path().join("other");
        fs::write(&other, "keep").unwrap();
        let dst = d.path().join("a/b/dst");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        symlink(&other, &dst).unwrap();
        copy_file_atomic(&src, &dst, None).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "new");
        assert_eq!(fs::read_to_string(&other).unwrap(), "keep", "the link target must not be written through");
        let m = fs::symlink_metadata(&dst).unwrap();
        assert!(m.is_file());
        assert_eq!(m.mode() & 0o777, 0o640);
        assert_eq!(FileTime::from_last_modification_time(&m).unix_seconds(), 1_600_000_000);
        let leftovers: Vec<_> =
            fs::read_dir(dst.parent().unwrap()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".kast-")).collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn copy_tree_keeps_symlinks_as_links() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("s");
        fs::create_dir_all(s.join("dir/sub")).unwrap();
        fs::write(s.join("dir/sub/f"), "x").unwrap();
        symlink("/etc/passwd", s.join("dir/link")).unwrap();
        let t = d.path().join("t");
        let mut w = Vec::new();
        let n = copy_tree(&s, Path::new("dir"), &t, None, &mut w).unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs::read_to_string(t.join("dir/sub/f")).unwrap(), "x");
        assert_eq!(fs::read_link(t.join("dir/link")).unwrap(), PathBuf::from("/etc/passwd"));
        assert!(w.is_empty());
    }

    #[test]
    fn same_content_and_sizes() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        fs::write(&a, "hello").unwrap();
        fs::write(&b, "hello").unwrap();
        fs::write(&c, "hellp").unwrap();
        assert!(same_content(&a, &b));
        assert!(!same_content(&a, &c));
        assert!(!same_content(&a, &d.path().join("missing")));
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(12), "12 B");
    }
}
