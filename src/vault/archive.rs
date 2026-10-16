//! Native zip handling for vault archives. Reads archives made by the shell version
//! (zip, 7z, bsdtar or python), whose entries may carry a "./" prefix.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use filetime::FileTime;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const MANIFEST: &str = "backup_manifest.json";

fn zip_time(t: SystemTime) -> zip::DateTime {
    let local: chrono::DateTime<chrono::Local> = t.into();
    zip::DateTime::try_from(local.naive_local()).unwrap_or_default()
}

fn system_time(t: zip::DateTime) -> Option<FileTime> {
    let naive: chrono::NaiveDateTime = t.try_into().ok()?;
    let local = naive.and_local_timezone(chrono::Local).earliest()?;
    Some(FileTime::from_unix_time(local.timestamp(), 0))
}

/// Zip the contents of `src_dir` (entry names relative to it). The archive holds app
/// logins and cookies, so it is created 0600.
pub fn create_zip_archive(src_dir: &Path, zip_file: &Path) -> Result<()> {
    if let Some(parent) = zip_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(zip_file);
    let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(zip_file).with_context(|| format!("creating {}", zip_file.display()))?;
    let mut zw = ZipWriter::new(BufWriter::new(file)).set_auto_large_file();

    for entry in WalkDir::new(src_dir).min_depth(1).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(src_dir)?.to_string_lossy().into_owned();
        let meta = path.symlink_metadata()?;
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(meta.mode() & 0o7777)
            .last_modified_time(zip_time(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        let ft = meta.file_type();
        if ft.is_dir() {
            zw.add_directory(format!("{name}/"), opts)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(path)?;
            zw.add_symlink(name, target.to_string_lossy(), opts)?;
        } else if ft.is_file() {
            zw.start_file(name, opts.large_file(meta.len() >= u32::MAX as u64))?;
            let mut f = File::open(path).with_context(|| format!("reading {}", path.display()))?;
            io::copy(&mut f, &mut zw)?;
        }
        // sockets / fifos / devices are skipped
    }
    zw.finish()?.flush()?;
    Ok(())
}

/// Create the parent of `out` after checking that its existing part resolves inside
/// `root`, so a symlink stored earlier in the archive can't redirect writes outside.
fn safe_parent(root: &Path, out: &Path) -> Result<()> {
    let parent = out.parent().unwrap_or(root);
    let mut existing = parent;
    while existing.symlink_metadata().is_err() {
        existing = existing.parent().unwrap_or(root);
    }
    if !existing.canonicalize()?.starts_with(root) {
        bail!("archive entry escapes the extraction folder: {}", out.display());
    }
    fs::create_dir_all(parent)?;
    Ok(())
}

fn remove_existing(out: &Path) {
    if let Ok(m) = out.symlink_metadata() {
        let _ = if m.is_dir() { fs::remove_dir_all(out) } else { fs::remove_file(out) };
    }
}

pub fn extract_zip_archive(zip_file: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let root = dest.canonicalize()?;
    let file = File::open(zip_file).with_context(|| format!("opening {}", zip_file.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file)).with_context(|| format!("reading {}", zip_file.display()))?;
    // Directory modes/times are applied last, so read-only dirs don't block their contents
    let mut dirs: Vec<(PathBuf, Option<u32>, Option<FileTime>)> = Vec::new();

    for i in 0..ar.len() {
        let mut f = ar.by_index(i)?;
        let Some(rel) = f.enclosed_name() else {
            eprintln!("skipping unsafe archive entry: {}", String::from_utf8_lossy(f.name_raw()));
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = root.join(&rel);
        let mode = f.unix_mode().map(|m| m & 0o777);
        let mtime = f.last_modified().and_then(system_time);
        safe_parent(&root, &out)?;

        if f.is_dir() {
            if out.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                remove_existing(&out);
            }
            fs::create_dir_all(&out)?;
            dirs.push((out, mode, mtime));
        } else if f.is_symlink() {
            let mut target = String::new();
            f.read_to_string(&mut target)?;
            remove_existing(&out);
            symlink(&target, &out).with_context(|| format!("creating link {}", out.display()))?;
        } else {
            remove_existing(&out);
            let mut w = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&out)
                .with_context(|| format!("writing {}", out.display()))?;
            io::copy(&mut f, &mut w)?;
            drop(w);
            fs::set_permissions(&out, fs::Permissions::from_mode(mode.unwrap_or(0o644)))?;
            if let Some(t) = mtime {
                let _ = filetime::set_file_mtime(&out, t);
            }
        }
    }
    for (dir, mode, mtime) in dirs.into_iter().rev() {
        if let Some(m) = mode {
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(m | 0o700));
        }
        if let Some(t) = mtime {
            let _ = filetime::set_file_mtime(&dir, t);
        }
    }
    Ok(())
}

/// The manifest text, at the archive root or one folder down; None if absent/unreadable.
pub fn read_zip_manifest(zip_file: &Path) -> Option<String> {
    let mut ar = ZipArchive::new(BufReader::new(File::open(zip_file).ok()?)).ok()?;
    let names: Vec<String> = ar.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let pick = names.iter().find(|n| n.trim_start_matches("./") == MANIFEST).or_else(|| names.iter().find(|n| n.ends_with(&format!("/{MANIFEST}"))))?.clone();
    let mut s = String::new();
    ar.by_name(&pick).ok()?.read_to_string(&mut s).ok()?;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_modes_links_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("zen/profile")).unwrap();
        fs::write(src.join(MANIFEST), "{\"apps\":{}}").unwrap();
        fs::write(src.join("zen/profile/prefs.js"), "x").unwrap();
        fs::set_permissions(src.join("zen/profile/prefs.js"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink("profile/prefs.js", src.join("zen/link")).unwrap();

        let zip = tmp.path().join("out/a.zip");
        create_zip_archive(&src, &zip).unwrap();
        assert_eq!(fs::metadata(&zip).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_zip_manifest(&zip).unwrap(), "{\"apps\":{}}");

        let dst = tmp.path().join("dst");
        extract_zip_archive(&zip, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("zen/profile/prefs.js")).unwrap(), "x");
        assert_eq!(fs::metadata(dst.join("zen/profile/prefs.js")).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_link(dst.join("zen/link")).unwrap(), Path::new("profile/prefs.js"));
    }

    #[test]
    fn manifest_found_with_dot_slash_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("b.zip");
        let mut zw = ZipWriter::new(File::create(&zip).unwrap());
        zw.start_file("./backup_manifest.json", SimpleFileOptions::default()).unwrap();
        zw.write_all(b"{}").unwrap();
        zw.finish().unwrap();
        assert_eq!(read_zip_manifest(&zip).unwrap(), "{}");
    }

    #[test]
    fn symlink_entries_cannot_redirect_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("evil.zip");
        let mut zw = ZipWriter::new(File::create(&zip).unwrap());
        zw.add_symlink("esc", tmp.path().join("outside").to_string_lossy(), SimpleFileOptions::default()).unwrap();
        zw.start_file("esc/pwned", SimpleFileOptions::default()).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();
        fs::create_dir_all(tmp.path().join("outside")).unwrap();
        assert!(extract_zip_archive(&zip, &tmp.path().join("dst")).is_err());
        assert!(!tmp.path().join("outside/pwned").exists());
    }
}
