// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! `export` and `import` of a profile as a `.kast-plasma.tar.gz`.
//!
//! konsave's `.knsv` is a zip holding `conf.yaml`, `save/` and `export/`; this is a gzip
//! tar holding `profile.toml`, `meta.toml` and `files/<section>/...`, so it gets its own
//! extension. Unlike konsave, importing only creates a profile: it never writes to the live
//! configuration (that is what `apply` is for, with its plan and snapshot), and an archive
//! is unpacked defensively: no absolute or `..` paths, no writing through symlinks, no hard
//! links or devices, no links leaving their section, no overwriting a profile without force.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use filetime::FileTime;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tar::{EntryType, Header, HeaderMode};
use walkdir::WalkDir;

use super::apply::link_stays_inside;
use super::config::{Config, Kind, Section};
use super::deps::{self, Requirement};
use super::fsops;
use super::paths::{Roots, valid_profile_name};
use super::profile::{self, Ambient, Manifest, Meta};

pub const EXT: &str = ".kast-plasma.tar.gz";
const MAX_ENTRIES: usize = 200_000;
const MAX_BYTES: u64 = 8 << 30;
const MAX_TOML: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOpts {
    pub force: bool,
    /// Add the large assets (themes, plasmoids, icons...) from the live folders for sections
    /// the profile does not hold yet
    pub assets: bool,
}

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub warnings: Vec<String>,
    pub assets_added: usize,
}

/// `out` may be a folder (file named after the profile), a `*.tar.gz` / `*.tgz` path, or
/// any other path which gets the extension appended.
pub fn export_path(out: &Path, name: &str) -> PathBuf {
    let s = out.to_string_lossy();
    if out.is_dir() || s.ends_with('/') {
        out.join(format!("{name}{EXT}"))
    } else if s.ends_with(".tar.gz") || s.ends_with(".tgz") {
        out.to_path_buf()
    } else {
        PathBuf::from(format!("{s}{EXT}"))
    }
}

fn header_for(meta: &fs::Metadata, kind: EntryType) -> Header {
    let mut h = Header::new_gnu();
    h.set_metadata_in_mode(meta, HeaderMode::Deterministic);
    h.set_entry_type(kind);
    h.set_mode(meta.mode() & 0o777);
    h.set_mtime(meta.mtime().max(0) as u64);
    h
}

fn write_tar(stage: &Path, target: impl io::Write) -> Result<()> {
    let mut b = tar::Builder::new(GzEncoder::new(target, Compression::default()));
    b.follow_symlinks(false);
    for top in ["profile.toml", "meta.toml"] {
        let p = stage.join(top);
        let m = fs::metadata(&p)?;
        let mut h = header_for(&m, EntryType::Regular);
        b.append_data(&mut h, top, File::open(&p)?)?;
    }
    for e in WalkDir::new(stage.join("files")).follow_links(false).follow_root_links(false).sort_by_file_name() {
        let e = e?;
        let name = e.path().strip_prefix(stage)?;
        let m = e.path().symlink_metadata()?;
        if m.is_dir() {
            let mut h = header_for(&m, EntryType::Directory);
            h.set_size(0);
            b.append_data(&mut h, name, io::empty())?;
        } else if m.file_type().is_symlink() {
            let mut h = header_for(&m, EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            b.append_link(&mut h, name, fs::read_link(e.path())?)?;
        } else if m.is_file() {
            let mut h = header_for(&m, EntryType::Regular);
            b.append_data(&mut h, name, File::open(e.path())?)?;
        }
    }
    b.into_inner()?.finish()?;
    Ok(())
}

/// Write a profile to an archive. Large assets are copied from the live folders at this
/// moment (so the archive can hold more than the saved profile), and `profile.toml` in the
/// archive lists exactly what it contains.
pub fn export(roots: &Roots, cfg: &Config, name: &str, out: &Path, opts: ExportOpts, amb: &Ambient) -> Result<ExportReport> {
    let prof = profile::load(roots, name)?;
    let path = export_path(out, name);
    if path.symlink_metadata().is_ok() && !opts.force {
        bail!("{} already exists (use --force to overwrite it)", path.display());
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    let stage = profile::new_stage(roots, ".export-")?;
    let result = (|| -> Result<ExportReport> {
        let mut warnings = Vec::new();
        fsops::mkdir_owned(&stage.join("files"), None)?;
        if prof.files_dir().is_dir() {
            fsops::copy_tree(&prof.dir, Path::new("files"), &stage, None, &mut warnings)?;
        }
        let mut manifest = prof.manifest.clone();
        let mut requires: Vec<Requirement> = prof.meta.requires.clone();
        let mut assets_added = 0;
        if opts.assets {
            let have: HashSet<String> = manifest.sections.iter().map(|s| s.id.clone()).collect();
            let want = |s: &Section| s.kind == Kind::Export && !have.contains(&s.id);
            let cap = profile::capture(roots, cfg, &stage.join("files"), &want)?;
            assets_added = cap.files;
            warnings.extend(cap.warnings);
            manifest.sections.extend(cap.sections);
            for r in deps::detect(&stage.join("files"), &amb.system_share, &*amb.pkgs) {
                if !requires.contains(&r) {
                    requires.push(r);
                }
            }
        }
        warnings.extend(profile::scan_secrets(&stage.join("files")));
        let mut meta = prof.meta.clone();
        meta.requires = requires;
        fs::write(stage.join("profile.toml"), toml::to_string(&manifest)?)?;
        fs::write(stage.join("meta.toml"), toml::to_string(&meta)?)?;
        let mut tmp = tempfile::Builder::new().prefix(".kast-export-").tempfile_in(dir)?;
        write_tar(&stage, tmp.as_file_mut())?;
        tmp.as_file().sync_all()?;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644))?;
        if let Some((u, g)) = roots.owner {
            crate::ctx::chown_path(tmp.path(), u, g);
        }
        let bytes = tmp.as_file().metadata()?.len();
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(ExportReport { path: path.clone(), bytes, warnings, assets_added })
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub dirs: usize,
    pub links: usize,
    pub bytes: u64,
}

/// A member path as a relative path of plain components starting with `files`,
/// `profile.toml` or `meta.toml`.
fn member_path(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => bail!("archive member {:?} has an absolute or `..` path", p.display().to_string()),
        }
    }
    let mut it = out.components();
    match it.next().and_then(|c| c.as_os_str().to_str().map(str::to_string)).as_deref() {
        Some("files") => {}
        Some("profile.toml" | "meta.toml") if it.next().is_none() => {}
        _ => bail!("archive member {:?} is not part of a kast Plasma profile", p.display().to_string()),
    }
    Ok(out)
}

/// Read an archive member by member, checking each, and (when `dest` is given) write it
/// below `dest`. With no `dest` this is the dry run: the same checks, nothing written.
/// Returns what was seen, and the text of `profile.toml` and `meta.toml`.
fn unpack(archive: &Path, dest: Option<&Path>, owner: fsops::Owner) -> Result<(Stats, Option<String>, Option<String>)> {
    let file = File::open(archive).with_context(|| archive.display().to_string())?;
    let mut ar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    let (mut stats, mut manifest, mut meta) = (Stats::default(), None, None);
    let mut seen = HashSet::new();
    let mut count = 0usize;
    for entry in ar.entries().context("not a gzip tar archive")? {
        let mut e = entry.context("reading the archive")?;
        count += 1;
        if count > MAX_ENTRIES {
            bail!("archive has more than {MAX_ENTRIES} entries");
        }
        let path = member_path(&e.path()?)?;
        if !seen.insert(path.clone()) {
            bail!("archive member {} appears twice", path.display());
        }
        if let Some(dest) = dest {
            // Nothing may be written through a link made earlier in the same archive
            let mut p = path.parent();
            while let Some(a) = p.filter(|a| !a.as_os_str().is_empty()) {
                if dest.join(a).symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!("archive member {} would be written through the symlink {}", path.display(), a.display());
                }
                p = a.parent();
            }
        }
        let size = e.header().size()?;
        let mode = e.header().mode().unwrap_or(0o644) & 0o777;
        let mtime = e.header().mtime().unwrap_or(0);
        match e.header().entry_type() {
            EntryType::Directory => {
                stats.dirs += 1;
                if let Some(dest) = dest {
                    let d = dest.join(&path);
                    match d.symlink_metadata() {
                        Ok(m) if m.is_dir() => {}
                        Ok(_) => bail!("archive member {} collides with a file", path.display()),
                        Err(_) => fsops::mkdir_owned(&d, owner)?,
                    }
                }
            }
            EntryType::Regular | EntryType::Continuous => {
                stats.files += 1;
                stats.bytes += size;
                if stats.bytes > MAX_BYTES {
                    bail!("archive unpacks to more than {} GiB", MAX_BYTES >> 30);
                }
                let top = path.components().count() == 1;
                if top && size > MAX_TOML {
                    bail!("{} is too large", path.display());
                }
                if path.components().count() < 3 && !top {
                    bail!("archive member {} is not inside a section", path.display());
                }
                if top {
                    let mut text = String::new();
                    e.by_ref().take(MAX_TOML).read_to_string(&mut text).with_context(|| format!("{} is not UTF-8 text", path.display()))?;
                    if let Some(dest) = dest {
                        fs::write(dest.join(&path), &text)?;
                    }
                    if path == Path::new("profile.toml") { manifest = Some(text) } else { meta = Some(text) }
                } else if let Some(dest) = dest {
                    let target = dest.join(&path);
                    fsops::mkdir_owned(target.parent().expect("member has a parent"), owner)?;
                    let mut f =
                        OpenOptions::new().write(true).create_new(true).mode(0o600).open(&target).with_context(|| format!("creating {}", target.display()))?;
                    let copied = io::copy(&mut e.by_ref().take(size), &mut f)?;
                    if copied != size {
                        bail!("archive member {} is truncated", path.display());
                    }
                    drop(f);
                    fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o600))?;
                    filetime::set_file_mtime(&target, FileTime::from_unix_time(mtime as i64, 0))?;
                    if let Some((u, g)) = owner {
                        crate::ctx::chown_path(&target, u, g);
                    }
                }
            }
            EntryType::Symlink => {
                stats.links += 1;
                let target = e.link_name()?.context("symlink without a target")?.into_owned();
                let comps: Vec<_> = path.components().collect();
                if comps.len() < 3 || comps[0].as_os_str() != "files" {
                    bail!("archive link {} is not inside a section", path.display());
                }
                let section_root: PathBuf = comps[..2].iter().collect();
                if !link_stays_inside(&section_root, &path, &target) {
                    bail!("archive link {} points outside its section ({})", path.display(), target.display());
                }
                if let Some(dest) = dest {
                    let l = dest.join(&path);
                    fsops::mkdir_owned(l.parent().expect("member has a parent"), owner)?;
                    std::os::unix::fs::symlink(&target, &l).with_context(|| format!("creating link {}", l.display()))?;
                    if let Some((u, g)) = owner {
                        crate::ctx::lchown_path(&l, u, g);
                    }
                }
            }
            other => bail!("archive member {} has the unsupported type {other:?} (hard links, devices and pipes are refused)", path.display()),
        }
    }
    Ok((stats, manifest, meta))
}

/// `name.kast-plasma.tar.gz` -> `name`
pub fn default_name(archive: &Path) -> String {
    let f = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for suffix in [EXT, ".tar.gz", ".tgz"] {
        if let Some(s) = f.strip_suffix(suffix) {
            return s.to_string();
        }
    }
    f
}

#[derive(Debug, Clone, Default)]
pub struct ImportOpts {
    /// Name for the new profile (default: from the file name)
    pub name: Option<String>,
    /// Replace an existing profile of that name
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ImportReport {
    pub name: String,
    pub stats: Stats,
    pub replaces: bool,
    pub meta: Meta,
    pub manifest: Manifest,
}

pub fn import(roots: &Roots, archive: &Path, opts: &ImportOpts) -> Result<ImportReport> {
    let name = opts.name.clone().unwrap_or_else(|| default_name(archive));
    if !valid_profile_name(&name) {
        bail!("{name:?} is not a valid profile name; choose one with --name");
    }
    let dest = profile::profile_dir(roots, &name)?;
    let replaces = dest.symlink_metadata().is_ok();
    if replaces && !opts.force {
        bail!("profile {name:?} already exists; choose another name with --name, or use --force to replace it");
    }
    let parse = |manifest: Option<String>, meta: Option<String>| -> Result<(Manifest, Meta)> {
        let m: Manifest = toml::from_str(&manifest.context("the archive has no profile.toml")?).context("profile.toml")?;
        profile::validate_manifest(&m).context("profile.toml")?;
        let meta: Meta = toml::from_str(&meta.context("the archive has no meta.toml")?).context("meta.toml")?;
        Ok((m, meta))
    };
    if opts.dry_run {
        let (stats, m, t) = unpack(archive, None, None)?;
        let (manifest, meta) = parse(m, t)?;
        return Ok(ImportReport { name, stats, replaces, meta, manifest });
    }
    let stage = profile::new_stage(roots, ".import-")?;
    let result = (|| -> Result<ImportReport> {
        let (stats, m, t) = unpack(archive, Some(&stage), roots.owner)?;
        let (manifest, meta) = parse(m, t)?;
        fsops::mkdir_owned(&stage.join("files"), roots.owner)?;
        profile::install_staged(roots, &stage, &name, &manifest, &meta, opts.force)?;
        Ok(ImportReport { name: name.clone(), stats, replaces, meta, manifest })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::profile::testutil::*;
    use super::super::profile::{SaveOpts, load, save};
    use super::*;

    fn exported() -> (tempfile::TempDir, Roots, Config, PathBuf) {
        let (d, r) = home();
        let cfg = Config::built_in();
        save(&r, &cfg, "work", SaveOpts::default(), &ambient()).unwrap();
        let out = d.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let rep = export(&r, &cfg, "work", &out, ExportOpts { assets: true, ..Default::default() }, &ambient()).unwrap();
        assert_eq!(rep.path, out.join("work.kast-plasma.tar.gz"));
        assert!(rep.assets_added > 0 && rep.bytes > 0);
        (d, r, cfg, rep.path)
    }

    fn other_machine() -> (tempfile::TempDir, Roots) {
        let d = tempfile::tempdir().unwrap();
        let r = Roots::under_home(d.path());
        (d, r)
    }

    #[test]
    fn export_import_round_trip_with_assets_and_without_touching_live_files() {
        let (_d, _r, _cfg, tgz) = exported();
        let (_d2, r2) = other_machine();
        fs::create_dir_all(&r2.config).unwrap();
        fs::write(r2.config.join("kdeglobals"), "mine").unwrap();
        let rep = import(&r2, &tgz, &ImportOpts::default()).unwrap();
        assert_eq!(rep.name, "work");
        assert_eq!(fs::read_to_string(r2.config.join("kdeglobals")).unwrap(), "mine", "import never writes live config");
        let p = load(&r2, "work").unwrap();
        assert_eq!(p.meta.hostname, "testhost");
        assert!(p.files_dir().join("appearance/kdeglobals").is_file());
        assert!(p.files_dir().join("plasmoids/plasma/plasmoids/my.widget/main.qml").is_file(), "assets exported live are in the archive");
        assert!(p.manifest.sections.iter().any(|s| s.id == "plasmoids"), "profile.toml lists what the archive holds");
        assert_eq!(fs::read_to_string(p.files_dir().join("appearance/kdeglobals")).unwrap(), "[General]\nColorScheme=Nord\n");
    }

    #[test]
    fn import_name_collision_needs_force_or_a_new_name() {
        let (_d, _r, _cfg, tgz) = exported();
        let (_d2, r2) = other_machine();
        import(&r2, &tgz, &ImportOpts::default()).unwrap();
        assert!(import(&r2, &tgz, &ImportOpts::default()).is_err());
        import(&r2, &tgz, &ImportOpts { name: Some("copy".into()), ..Default::default() }).unwrap();
        import(&r2, &tgz, &ImportOpts { force: true, ..Default::default() }).unwrap();
        let names: Vec<String> = profile::list(&r2).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["copy".to_string(), "work".to_string()], "no staging folders left");
    }

    #[test]
    fn dry_run_runs_the_same_checks_and_writes_nothing() {
        let (_d, _r, _cfg, tgz) = exported();
        let (_d2, r2) = other_machine();
        let rep = import(&r2, &tgz, &ImportOpts { dry_run: true, ..Default::default() }).unwrap();
        assert!(rep.stats.files > 5);
        assert!(!r2.profiles.exists());
        let bad = evil_archive(&r2, &[("../escape", None, b"x")]);
        assert!(import(&r2, &bad, &ImportOpts { dry_run: true, ..Default::default() }).is_err());
    }

    type Member<'a> = (&'a str, Option<(EntryType, &'a str)>, &'a [u8]);

    /// An archive built byte by byte, because the tar crate refuses to write `..` itself.
    fn evil_archive(r: &Roots, members: &[Member]) -> PathBuf {
        let path = r.home.join(format!("evil-{}.tar.gz", members.len()));
        let mut b = tar::Builder::new(GzEncoder::new(File::create(&path).unwrap(), Compression::default()));
        let mut put = |name: &str, kind: EntryType, link: Option<&str>, data: &[u8]| {
            let mut h = Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(0o644);
            h.set_size(data.len() as u64);
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(l) = link {
                h.set_link_name(l).unwrap();
            }
            h.set_cksum();
            b.append(&h, data).unwrap();
        };
        put("profile.toml", EntryType::Regular, None, b"");
        put("meta.toml", EntryType::Regular, None, b"kast_version='1'\nhostname='h'\ncreated='c'\ndefault_list_commit='x'\n");
        for (name, special, data) in members {
            match special {
                Some((kind, link)) => put(name, *kind, Some(link), data),
                None => put(name, EntryType::Regular, None, data),
            }
        }
        b.into_inner().unwrap().finish().unwrap();
        path
    }

    fn rejected(members: &[Member]) -> String {
        let (_d, r) = other_machine();
        let a = evil_archive(&r, members);
        let err = import(&r, &a, &ImportOpts::default()).unwrap_err();
        assert!(profile::list(&r).unwrap().is_empty(), "a rejected archive leaves no profile");
        assert!(fs::read_dir(&r.profiles).map(|rd| rd.count() == 0).unwrap_or(true), "and no staging folder");
        assert!(!r.home.join("escape").exists());
        format!("{err:#}")
    }

    #[test]
    fn rejects_parent_traversal_and_absolute_paths() {
        assert!(rejected(&[("../escape", None, b"x")]).contains("`..`"));
        assert!(rejected(&[("files/a/../../../escape", None, b"x")]).contains("`..`"));
        assert!(rejected(&[("/etc/escape", None, b"x")]).contains("absolute"));
    }

    #[test]
    fn rejects_members_outside_the_profile_layout() {
        assert!(rejected(&[("other.txt", None, b"x")]).contains("not part of"));
        assert!(rejected(&[("files/x", None, b"x")]).contains("not inside a section"));
    }

    #[test]
    fn rejects_writing_through_a_symlink() {
        // d -> /tmp/x (outside), then d/file
        let m = rejected(&[("files/s/d", Some((EntryType::Symlink, "../../../../../tmp/x")), b""), ("files/s/d/file", None, b"x")]);
        assert!(m.contains("outside"), "{m}");
        // an inside link followed by a write through it is refused too
        let m = rejected(&[("files/s/real/f", None, b"a"), ("files/s/d", Some((EntryType::Symlink, "real")), b""), ("files/s/d/g", None, b"x")]);
        assert!(m.contains("through the symlink"), "{m}");
    }

    #[test]
    fn rejects_links_pointing_out_hard_links_and_devices() {
        assert!(rejected(&[("files/s/l", Some((EntryType::Symlink, "/etc/passwd")), b"")]).contains("outside"));
        assert!(rejected(&[("files/s/l", Some((EntryType::Symlink, "../../escape")), b"")]).contains("outside"));
        assert!(rejected(&[("files/s/h", Some((EntryType::Link, "files/s/other")), b"")]).contains("unsupported"));
        assert!(rejected(&[("files/s/dev", Some((EntryType::Char, "x")), b"")]).contains("unsupported"));
    }

    #[test]
    fn rejects_duplicate_members_and_bad_manifests() {
        assert!(rejected(&[("files/s/a", None, b"1"), ("files/s/a", None, b"2")]).contains("twice"));
        let (_d, r) = other_machine();
        let a = evil_archive(&r, &[]);
        // profile.toml is empty and valid, so this archive imports; make one with traversal in the manifest
        import(&r, &a, &ImportOpts::default()).unwrap();
        let bad_manifest = r.home.join("bm.tar.gz");
        let mut b = tar::Builder::new(GzEncoder::new(File::create(&bad_manifest).unwrap(), Compression::default()));
        for (n, t) in [
            ("profile.toml", "[[section]]\nid='a'\ncategory='apps'\nkind='save'\nentries=['../../.ssh']\n"),
            ("meta.toml", "kast_version='1'\nhostname='h'\ncreated='c'\ndefault_list_commit='x'\n"),
        ] {
            let mut h = Header::new_gnu();
            h.set_size(t.len() as u64);
            h.set_mode(0o644);
            h.set_path(n).unwrap();
            h.set_cksum();
            b.append(&h, t.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        assert!(import(&r, &bad_manifest, &ImportOpts { name: Some("bm".into()), ..Default::default() }).is_err());
        assert!(import(&r, &r.home.join("missing.tar.gz"), &ImportOpts::default()).is_err());
        fs::write(r.home.join("junk.tar.gz"), "not an archive").unwrap();
        assert!(import(&r, &r.home.join("junk.tar.gz"), &ImportOpts::default()).is_err());
    }

    #[test]
    fn imported_profile_cannot_apply_outside_the_local_list() {
        use super::super::apply;
        let (_d, r) = other_machine();
        let a = evil_archive(&r, &[("files/appearance/.ssh/authorized_keys", None, b"evil")]);
        // The archive claims a manifest entry through a crafted profile.toml
        let tgz = r.home.join("claim.tar.gz");
        let mut b = tar::Builder::new(GzEncoder::new(File::create(&tgz).unwrap(), Compression::default()));
        let manifest = "[[section]]\nid='appearance'\ncategory='appearance'\nkind='save'\nentries=['.ssh']\n";
        for (n, t) in [
            ("profile.toml", manifest),
            ("meta.toml", "kast_version='1'\nhostname='h'\ncreated='c'\ndefault_list_commit='x'\n"),
            ("files/appearance/.ssh/authorized_keys", "evil"),
        ] {
            let mut h = Header::new_gnu();
            h.set_size(t.len() as u64);
            h.set_mode(0o644);
            h.set_path(n).unwrap();
            h.set_cksum();
            b.append(&h, t.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        let _ = a;
        import(&r, &tgz, &ImportOpts::default()).unwrap();
        let p = load(&r, "claim").unwrap();
        let pl = apply::plan(&r, &Config::built_in(), &p, None).unwrap();
        assert!(pl.items.is_empty() && !pl.notes.is_empty(), "{}", pl.render());
        apply::execute(&r, &pl, true, &ambient()).unwrap();
        assert!(!r.home.join(".ssh").exists() && !r.config.join(".ssh").exists());
    }

    #[test]
    fn export_refuses_to_overwrite_without_force_and_names_files() {
        let (d, r, cfg, tgz) = exported();
        assert!(export(&r, &cfg, "work", tgz.parent().unwrap(), ExportOpts::default(), &ambient()).is_err());
        export(&r, &cfg, "work", tgz.parent().unwrap(), ExportOpts { force: true, ..Default::default() }, &ambient()).unwrap();
        assert_eq!(export_path(Path::new("/x/y"), "n"), PathBuf::from("/x/y.kast-plasma.tar.gz"));
        assert_eq!(export_path(Path::new("/x/y.tgz"), "n"), PathBuf::from("/x/y.tgz"));
        assert_eq!(export_path(d.path(), "n"), d.path().join("n.kast-plasma.tar.gz"));
        assert_eq!(default_name(Path::new("/a/b/my.kast-plasma.tar.gz")), "my");
        assert!(export(&r, &cfg, "nope", tgz.parent().unwrap(), ExportOpts::default(), &ambient()).is_err());
        let leftovers: Vec<_> = fs::read_dir(tgz.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(leftovers, vec!["work.kast-plasma.tar.gz".to_string()]);
    }

    #[test]
    fn modes_and_links_survive_the_round_trip() {
        let (_d, r) = home();
        fs::set_permissions(r.config.join("kwinrc"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(r.config.join("gtk-3.0/real.css"), "x").unwrap();
        std::os::unix::fs::symlink("real.css", r.config.join("gtk-3.0/gtk.css")).unwrap();
        let cfg = Config::built_in();
        save(&r, &cfg, "m", SaveOpts::default(), &ambient()).unwrap();
        let out = r.home.join("m.kast-plasma.tar.gz");
        export(&r, &cfg, "m", &out, ExportOpts::default(), &ambient()).unwrap();
        let (_d2, r2) = other_machine();
        import(&r2, &out, &ImportOpts::default()).unwrap();
        let p = load(&r2, "m").unwrap();
        assert_eq!(fs::metadata(p.files_dir().join("windows/kwinrc")).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_link(p.files_dir().join("gtk/gtk-3.0/gtk.css")).unwrap(), PathBuf::from("real.css"));
    }
}
