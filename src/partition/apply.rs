//! Applying a partition choice: test-mount first, then write `/etc/fstab`, mount through it
//! and verify; any failure after the file is touched rolls everything back. Every change is
//! journaled before it happens (`dir_create`, `file_write`, `fstab_entry`, `mount`,
//! `chown_root`, `folder_move`) so `kast undo` can revert it.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::fstab::{self, Choices, FstabEntry, Mode, Spot};
use super::{Driver, Fs, Partition, Runner};
use crate::ctx::{self, log};
use crate::journal::{self, sys_path};
use crate::setup::{self, HomeState, Moved};

/// The user the folders belong to.
#[derive(Debug, Clone, PartialEq)]
pub struct Who {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl Who {
    pub fn current() -> Who {
        let c = ctx::ctx();
        Who { user: c.target_user.clone(), uid: c.target_uid, gid: c.target_gid, home: c.target_home.clone() }
    }
}

/// Everything decided before the run starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub part: Partition,
    pub choices: Choices,
    /// Numbers of the lines of /etc/fstab to replace (the user agreed)
    pub replace: Vec<usize>,
    /// Unmount it first if it is mounted somewhere else
    pub unmount_first: bool,
    /// Home folders to move onto the partition
    pub folders: Vec<String>,
    pub who: Who,
    /// For the comment line (`2026-10-18`)
    pub date: String,
}

impl Plan {
    pub fn entry(&self) -> FstabEntry {
        fstab::fstab_entry(&self.part, &self.choices)
    }

    pub fn comment(&self) -> String {
        FstabEntry::comment(&self.part, &self.date)
    }
}

pub struct Env<'a> {
    pub rn: &'a dyn Runner,
    /// Where the private test-mount folder goes (`/run`)
    pub probe_base: PathBuf,
    pub progress: &'a dyn Fn(&str),
    /// Free bytes at a path (`statvfs`; tests set their own)
    pub free: &'a dyn Fn(&Path) -> Option<u64>,
}

impl<'a> Env<'a> {
    pub fn new(rn: &'a dyn Runner, progress: &'a dyn Fn(&str)) -> Env<'a> {
        Env { rn, probe_base: PathBuf::from("/run"), progress, free: &avail }
    }
}

/// How it went.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub line: String,
    pub mountpoint: String,
    pub mounted: bool,
    /// None: not tested (the top folder is root's)
    pub writable_as_user: Option<bool>,
    pub moved: Vec<String>,
    pub kept: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub notes: Vec<String>,
}

pub const UNDO_HINT: &str = "undo from Undo changes (u), or `sudo kast undo --last --step partition`";

// --- Test mounts that must not outlive kast ----------------------------------------------

static PROBES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// After a signal or a job: unmount and remove any test mount still around. Plain `umount`,
/// never lazy or forced.
pub fn cleanup_probes() {
    let dirs = std::mem::take(&mut *PROBES.lock().unwrap());
    for d in dirs {
        let _ = std::process::Command::new("umount").arg(&d).stdin(std::process::Stdio::null()).output();
        let _ = fs::remove_dir(&d);
    }
}

fn track(p: &Path) {
    PROBES.lock().unwrap().push(p.to_path_buf());
}

fn untrack(p: &Path) {
    PROBES.lock().unwrap().retain(|d| d != p);
}

/// A private folder for the test mount.
fn probe_dir(base: &Path) -> Result<PathBuf> {
    fs::create_dir_all(base).ok();
    let d = tempfile::Builder::new().prefix("kast-probe.").tempdir_in(base).context("creating a folder for the test mount")?;
    let p = d.keep();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).ok();
    track(&p);
    Ok(p)
}

fn drop_probe(rn: &dyn Runner, p: &Path) -> Result<()> {
    let u = rn.run("umount", &[&p.to_string_lossy()]);
    if !u.ok && is_mounted_at(rn, p) {
        bail!("couldn't unmount the test mount {}: {}", p.display(), u.message());
    }
    fs::remove_dir(p).ok();
    untrack(p);
    Ok(())
}

fn is_mounted_at(rn: &dyn Runner, p: &Path) -> bool {
    let r = rn.run("findmnt", &["--mountpoint", &p.to_string_lossy(), "-n", "-o", "SOURCE"]);
    r.ok && !r.stdout.trim().is_empty()
}

// --- Helpers ------------------------------------------------------------------------------

/// `UUID=…` / `PARTUUID=…` / the device, as the fstab line says it.
fn mount_args<'a>(entry: &'a FstabEntry, opts: &'a str, target: &'a str) -> Vec<&'a str> {
    let mut a = vec!["-t", entry.vfstype.as_str()];
    if !opts.is_empty() {
        a.extend(["-o", opts]);
    }
    a.extend([entry.spec.as_str(), target]);
    a
}

/// The `.mount`/`.automount` unit name for a path (`systemd-escape --path`).
pub fn unit_name(rn: &dyn Runner, path: &str, suffix: &str) -> String {
    let r = rn.run("systemd-escape", &["--path", &format!("--suffix={suffix}"), path]);
    if r.ok && !r.stdout.trim().is_empty() {
        return r.stdout.trim().to_string();
    }
    let t = path.trim_matches('/');
    let mut name = String::new();
    for (i, b) in t.bytes().enumerate() {
        match b {
            b'/' => name.push('-'),
            b'.' if i == 0 => name.push_str("\\x2e"),
            c if c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'.') => name.push(c as char),
            c => name.push_str(&format!("\\x{c:02x}")),
        }
    }
    format!("{}.{suffix}", if name.is_empty() { "-".into() } else { name })
}

pub fn avail(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a zeroed statvfs the call fills in
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 { Some(s.f_bavail as u64 * s.f_frsize as u64) } else { None }
}

/// Bytes in a folder (symlinks not followed).
pub fn dir_size(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).into_iter().flatten().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

/// What moving `sizes` needs on the partition: the data plus a margin.
pub fn needed(sizes: u64) -> u64 {
    sizes + sizes / 20 + (64 << 20)
}

fn ntfs_advice() -> &'static str {
    "If Windows uses Fast Startup or was hibernated, the partition is marked dirty and can't be mounted read-write. \
     Shut Windows down fully (Shift + Shut down) or turn off Fast Startup (Control Panel > Power Options), then try again."
}

/// What to tell the user when the test mount fails: the message, the kernel's last words,
/// and for a dirty NTFS volume what to do about it.
pub fn explain_mount_failure(fs: Fs, err: &str, kernel: &str) -> String {
    let mut m = format!("The partition could not be mounted: {err}");
    if !kernel.trim().is_empty() {
        m += &format!("\nKernel log:\n{}", kernel.trim());
    }
    let all = format!("{err} {kernel}").to_lowercase();
    if fs == Fs::Ntfs && ["dirty", "hibernat", "unsafe state", "fast restart", "fast start", "unclean", "windows_names"].iter().any(|w| all.contains(w)) {
        m += &format!("\n{}", ntfs_advice());
    }
    m
}

/// The last kernel messages about this device or driver (root can usually read them).
fn kernel_tail(rn: &dyn Runner, part: &Partition, driver: Driver) -> String {
    let dev = part.path.rsplit('/').next().unwrap_or_default().to_string();
    let drv = driver.fstab_type().replace("-", "");
    let mut out = rn.run("dmesg", &["--notime"]);
    if !out.ok {
        out = rn.run("journalctl", &["-k", "-n", "200", "--no-pager", "-o", "cat"]);
    }
    let lines: Vec<&str> = out.stdout.lines().filter(|l| l.contains(&dev) || l.to_lowercase().contains(&drv)).collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

/// Processes holding a mount (`fuser -vm`), for the "busy" message.
pub fn busy_processes(rn: &dyn Runner, path: &str) -> String {
    let r = rn.run("fuser", &["-vm", path]);
    let text = format!("{}{}", r.stdout, r.stderr);
    let t = text.trim();
    if t.is_empty() { "(fuser could not list them)".into() } else { t.to_string() }
}

/// `umount`, never lazy or forced. A busy mount names its processes.
pub fn unmount(rn: &dyn Runner, path: &str) -> Result<()> {
    let r = rn.run("umount", &[path]);
    if r.ok {
        return Ok(());
    }
    if r.message().to_lowercase().contains("busy") {
        bail!("{path} is busy; these processes are using it:\n{}\nClose them and try again.", busy_processes(rn, path));
    }
    bail!("umount {path} failed: {}", r.message())
}

fn parse_export(s: &str) -> std::collections::HashMap<String, String> {
    s.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect()
}

/// Where a btrfs filesystem's subvolumes are, as `btrfs subvolume list` prints them
/// (`ID 256 gen 12 top level 5 path @data`) -> `/@data`.
pub fn parse_subvolumes(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split_once(" path ").map(|(_, p)| format!("/{}", p.trim()))).collect()
}

/// Mount the btrfs filesystem read-only for a moment and list its subvolumes.
pub fn list_subvolumes(rn: &dyn Runner, part: &Partition, probe_base: &Path) -> Result<Vec<String>> {
    let probe = probe_dir(probe_base)?;
    let spec = part.uuid.as_deref().map(|u| format!("UUID={u}")).unwrap_or_else(|| part.path.clone());
    let m = rn.run("mount", &["-t", "btrfs", "-o", "ro", &spec, &probe.to_string_lossy()]);
    if !m.ok {
        drop_probe(rn, &probe).ok();
        bail!("could not mount it to list the subvolumes: {}", m.message());
    }
    let l = rn.run("btrfs", &["subvolume", "list", &probe.to_string_lossy()]);
    drop_probe(rn, &probe)?;
    if !l.ok {
        bail!("btrfs subvolume list failed: {}", l.message());
    }
    Ok(parse_subvolumes(&l.stdout))
}

// --- Checking what the kernel reports -----------------------------------------------------

fn octal(s: &str) -> Option<u32> {
    u32::from_str_radix(s, 8).ok()
}

fn collect_rows(v: &Value, out: &mut Vec<Value>) {
    if let Some(a) = v.get("filesystems").and_then(Value::as_array) {
        for f in a {
            out.push(f.clone());
            collect_rows(f, out);
        }
    }
    if let Some(a) = v.get("children").and_then(Value::as_array) {
        for f in a {
            out.push(f.clone());
            collect_rows(f, out);
        }
    }
}

/// `findmnt --target … -J` against what was asked for: the real mount (not the autofs
/// placeholder) must be this filesystem with the same type and the options that matter.
/// The kernel prints options its own way, so ownership and masks are compared as numbers.
pub fn check_mounted(json_text: &str, entry: &FstabEntry, part: &Partition, driver: Driver) -> Result<(), String> {
    let v: Value = serde_json::from_str(json_text).map_err(|e| format!("findmnt's answer could not be read: {e}"))?;
    let mut rows = Vec::new();
    collect_rows(&v, &mut rows);
    let row = rows.iter().rfind(|r| r["fstype"].as_str() != Some("autofs")).ok_or("nothing is mounted there")?;
    let fstype = row["fstype"].as_str().unwrap_or_default();
    let want: &[&str] = if driver == Driver::Ntfs3g { &["fuseblk", "ntfs-3g", "ntfs"] } else { &[driver.fstab_type()] };
    if !want.contains(&fstype) {
        return Err(format!("it is mounted as {fstype}, not {}", driver.fstab_type()));
    }
    if let (Some(u), Some(got)) = (part.uuid.as_deref(), row["uuid"].as_str()) {
        if u != got {
            return Err(format!("the mounted filesystem has UUID {got}, not {u}"));
        }
    }
    let opts = row["options"].as_str().unwrap_or_default();
    let have: std::collections::HashMap<&str, Option<&str>> = opts
        .split(',')
        .map(|o| match o.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (o, None),
        })
        .collect();
    let ro_wanted = entry.options.iter().any(|o| o == "ro");
    if ro_wanted != have.contains_key("ro") {
        return Err(format!(
            "it is mounted {} but {} was asked for",
            if have.contains_key("ro") { "read-only" } else { "read-write" },
            if ro_wanted { "read-only" } else { "read-write" }
        ));
    }
    for o in entry.mount_options() {
        let Some((k, want_v)) = o.split_once('=') else { continue };
        let got = have.get(k).copied().flatten();
        let same = match k {
            "uid" | "gid" | "dmask" | "fmask" | "umask" if driver == Driver::Ntfs3g => continue,
            "uid" | "gid" => got == Some(want_v),
            "dmask" | "fmask" | "umask" => got.and_then(octal) == octal(want_v),
            "subvol" => got.map(|g| g.trim_start_matches('/')) == Some(want_v.trim_start_matches('/')),
            _ => continue,
        };
        if !same {
            return Err(format!("the mount shows {k}={} instead of {k}={want_v}", got.unwrap_or("(nothing)")));
        }
    }
    Ok(())
}

// --- The apply flow -----------------------------------------------------------------------

#[derive(Default)]
struct State {
    created_dirs: Vec<PathBuf>,
    dir_seq: Option<u64>,
    original_fstab: Option<Vec<u8>>,
    fstab_written: bool,
    fstab_seqs: Vec<u64>,
    mount_seq: Option<u64>,
    mounted: bool,
    chown: Option<(PathBuf, u32, u32, u32)>,
    chown_seq: Option<u64>,
    mp: String,
    automount: bool,
}

fn fstab_file() -> PathBuf {
    sys_path("/etc/fstab")
}

/// The pre-checks: still the same device, not mounted (or unmount it), a usable mount point,
/// and an fstab we can edit. Returns what is at the mount point.
fn precheck(plan: &Plan, env: &Env) -> Result<Spot> {
    let p = &plan.part;
    let b = env.rn.run("blkid", &["-p", "-o", "export", &p.path]);
    if !b.ok {
        bail!("{} can't be read (is it still plugged in?): {}", p.path, b.message());
    }
    let id = parse_export(&b.stdout);
    match (p.uuid.as_deref(), id.get("UUID"), p.partuuid.as_deref(), id.get("PART_ENTRY_UUID")) {
        (Some(want), Some(got), _, _) if want == got => {}
        (Some(want), got, _, _) => {
            bail!("{} no longer has the UUID {want} (it has {}): the device changed; scan again", p.path, got.map_or("none", String::as_str))
        }
        (None, _, Some(want), Some(got)) if want.eq_ignore_ascii_case(got) => {}
        (None, ..) => bail!("{} is not the partition that was chosen any more; scan again", p.path),
    }
    if let (Some(t), Some(got)) = (p.fstype.as_deref(), id.get("TYPE")) {
        if t != got {
            bail!("{} is {got} now, not {t}; scan again", p.path);
        }
    }
    let mounted = env.rn.run("findmnt", &["-rn", "-S", &p.path, "-o", "TARGET"]);
    let targets: Vec<&str> = mounted.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if !targets.is_empty() {
        if !plan.unmount_first {
            bail!("{} is mounted at {}; unmount it first (or let kast do it)", p.path, targets.join(", "));
        }
        for t in targets {
            (env.progress)(&format!("Unmounting {t}"));
            unmount(env.rn, t)?;
        }
    }
    let (_, spot) = fstab::check_mountpoint(&plan.choices.mountpoint, &sys_path(&plan.choices.mountpoint), env.rn).map_err(anyhow::Error::msg)?;
    Ok(spot)
}

/// Mount it the way the entry says, on a private folder, and see that it works. Nothing is
/// changed on the system.
fn test_mount(plan: &Plan, entry: &FstabEntry, env: &Env) -> Result<()> {
    let probe = probe_dir(&env.probe_base)?;
    let result = (|| -> Result<()> {
        let opts = entry.mount_options().join(",");
        let ps = probe.to_string_lossy().into_owned();
        let m = env.rn.run("mount", &mount_args(entry, &opts, &ps));
        if !m.ok {
            let tail = kernel_tail(env.rn, &plan.part, plan.choices.driver);
            bail!("{}", explain_mount_failure(plan.part.fs().unwrap_or(plan.choices.driver.fs()), &m.message(), &tail));
        }
        if !plan.choices.read_only {
            let f = probe.join(format!(".kast-probe.{}", std::process::id()));
            let w = fs::write(&f, b"").and_then(|_| fs::remove_file(&f));
            if let Err(e) = w {
                let advice = if plan.choices.driver.fs() == Fs::Ntfs { format!("\n{}", ntfs_advice()) } else { String::new() };
                bail!("The partition mounted, but only read-only ({e}).{advice} Choose read-only to mount it that way.");
            }
        }
        if !plan.folders.is_empty() {
            let sizes: u64 = plan.folders.iter().map(|f| dir_size(&plan.who.home.join(f))).sum();
            if let Some(free) = (env.free)(&probe) {
                if free < needed(sizes) {
                    bail!(
                        "Not enough room on the partition for the folders: they take {}, it has {} free (with some margin).",
                        super::human(sizes),
                        super::human(free)
                    );
                }
            }
        }
        Ok(())
    })();
    let unmounted = drop_probe(env.rn, &probe);
    result?;
    unmounted
}

fn rm_dirs(dirs: &[PathBuf]) {
    for d in dirs.iter().rev() {
        let _ = fs::remove_dir(d);
    }
}

/// Take back everything done so far. fstab goes back byte for byte.
fn rollback(st: &mut State, env: &Env, why: &str) {
    log("ERROR", &format!("Rolling back: {why}"));
    (env.progress)("Rolling back");
    if let Some((path, uid, gid, mode)) = st.chown.take() {
        ctx::chown_path(&path, uid, gid);
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
    }
    if st.mounted {
        let ok = if st.automount {
            let a = unit_name(env.rn, &st.mp, "automount");
            let m = unit_name(env.rn, &st.mp, "mount");
            let _ = env.rn.run("systemctl", &["stop", &a]);
            let _ = env.rn.run("systemctl", &["stop", &m]);
            !is_mounted_at(env.rn, Path::new(&st.mp))
        } else {
            unmount(env.rn, &st.mp).is_ok()
        };
        if !ok {
            log("ERROR", &format!("Could not unmount {} while rolling back", st.mp));
        }
        st.mounted = false;
    }
    if st.fstab_written {
        if let Some(orig) = &st.original_fstab {
            match ctx::write_atomic(&fstab_file(), orig) {
                Ok(()) => log("INFO", "Restored /etc/fstab exactly as it was"),
                Err(e) => log("ERROR", &format!("COULD NOT restore /etc/fstab: {e}. The saved copy is /etc/fstab.kast-bak.*")),
            }
        }
        let _ = env.rn.run("systemctl", &["daemon-reload"]);
        st.fstab_written = false;
    }
    rm_dirs(&st.created_dirs);
    for s in st.fstab_seqs.drain(..).chain(st.mount_seq.take()).chain(st.chown_seq.take()).chain(st.dir_seq.take()) {
        journal::revert_change(Some(s), &format!("rolled back: {why}"));
    }
}

/// Run the plan. Errors before `/etc/fstab` is touched leave nothing behind; errors after
/// roll it all back.
pub fn apply(plan: &Plan, env: &Env) -> Result<Report> {
    let entry = plan.entry();
    let comment = plan.comment();
    let mp = plan.choices.mountpoint.clone();
    let automount = plan.choices.mode == Mode::Automount;
    let mut report = Report { line: entry.to_string(), mountpoint: mp.clone(), notes: entry.notes.clone(), ..Default::default() };

    (env.progress)("Checking the partition");
    let spot = precheck(plan, env)?;
    // The new content, built before anything changes: a conflict is an error here
    let path = fstab_file();
    let current = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(current.clone()).map_err(|_| anyhow::anyhow!("/etc/fstab is not valid UTF-8; not touching it"))?;
    let edit = fstab::add_entry(&text, &plan.part, &entry, &comment, &plan.replace).map_err(anyhow::Error::msg)?;

    (env.progress)("Test-mounting the partition");
    test_mount(plan, &entry, env)?;

    let mut st = State { original_fstab: Some(current), mp: mp.clone(), automount, ..Default::default() };
    let r = (|| -> Result<()> {
        if spot == Spot::Missing {
            (env.progress)("Creating the mount point");
            let mut missing = Vec::new();
            let mut d = sys_path(&mp);
            while !d.exists() {
                missing.push(d.clone());
                match d.parent() {
                    Some(p) => d = p.to_path_buf(),
                    None => break,
                }
            }
            missing.reverse();
            st.dir_seq = journal::start_change("dir_create", json!({ "path": sys_path(&mp), "created": missing }));
            let made = fs::create_dir_all(sys_path(&mp));
            journal::finish_change("dir_create", st.dir_seq, made.is_ok(), json!({}));
            st.created_dirs = missing.clone();
            made.with_context(|| format!("creating {mp}"))?;
            for d in &missing {
                fs::set_permissions(d, fs::Permissions::from_mode(0o755)).ok();
            }
        }

        (env.progress)("Checking the new /etc/fstab");
        let cand = tempfile::Builder::new()
            .prefix("kast-fstab.")
            .tempfile_in(&env.probe_base)
            .or_else(|_| tempfile::NamedTempFile::new())
            .context("creating a temp file for the new fstab")?;
        fs::write(cand.path(), edit.text.as_bytes())?;
        let before = fstab::verify_file(env.rn, &path);
        let after = fstab::verify_file(env.rn, cand.path());
        let v = fstab::verify_diff(&before, &after, &mp);
        for w in &v.warnings {
            log("WARN", &format!("findmnt --verify: {w}"));
            report.notes.push(format!("findmnt warns: {w}"));
        }
        if !v.errors.is_empty() {
            bail!("findmnt --verify found problems in the new /etc/fstab, so it was not installed:\n{}", v.errors.join("\n"));
        }

        (env.progress)("Writing /etc/fstab");
        let ts = ctx::now_str("%Y%m%d_%H%M%S");
        let backup = PathBuf::from(format!("{}.kast-bak.{ts}", path.display()));
        ctx::write_atomic(&backup, st.original_fstab.as_deref().unwrap_or_default()).context("saving a copy of /etc/fstab")?;
        let entry_seq = journal::start_change(
            "fstab_entry",
            json!({
                "path": path, "line": entry.to_string(), "comment": comment, "replaced": edit.replaced,
                "mountpoint": mp, "uuid": plan.part.uuid, "backup": backup,
            }),
        );
        st.fstab_seqs.extend(entry_seq);
        st.fstab_written = true;
        let wrote = journal::write_file_tracked(&path, edit.text.as_bytes(), None);
        journal::finish_change("fstab_entry", entry_seq, wrote.is_ok(), json!({}));
        if let Ok(Some(s)) = &wrote {
            st.fstab_seqs.push(*s);
        }
        wrote.context("writing /etc/fstab")?;
        log("INFO", &format!("Wrote to /etc/fstab: {entry}"));
        let dr = env.rn.run("systemctl", &["daemon-reload"]);
        if !dr.ok {
            bail!("systemctl daemon-reload failed: {}", dr.message());
        }

        (env.progress)("Mounting");
        let unit = unit_name(env.rn, &mp, if automount { "automount" } else { "mount" });
        st.mount_seq = journal::start_change(
            "mount",
            json!({ "mountpoint": mp, "unit": unit, "mode": if automount { "automount" } else { "boot" }, "uuid": plan.part.uuid }),
        );
        let m = if automount {
            let s = env.rn.run("systemctl", &["start", &unit]);
            if s.ok {
                // Touching the folder triggers the mount
                let _ = fs::read_dir(sys_path(&mp));
            }
            s
        } else {
            env.rn.run("mount", &["--target", &mp])
        };
        journal::finish_change("mount", st.mount_seq, m.ok, json!({}));
        st.mounted = m.ok || is_mounted_at(env.rn, Path::new(&mp));
        if !m.ok {
            bail!("mounting {mp} through /etc/fstab failed: {}", m.message());
        }

        (env.progress)("Verifying the mount");
        let f = env.rn.run("findmnt", &["--target", &mp, "-J", "-o", "SOURCE,FSTYPE,OPTIONS,UUID"]);
        if !f.ok {
            bail!("{mp} is not mounted after mounting: {}", f.message());
        }
        check_mounted(&f.stdout, &entry, &plan.part, plan.choices.driver).map_err(anyhow::Error::msg)?;
        report.mounted = true;

        if plan.choices.own_top_folder && !plan.choices.driver.fs().foreign() {
            let root = sys_path(&mp);
            let md = fs::metadata(&root)?;
            let (uid, gid, mode) = (md.uid(), md.gid(), md.mode() & 0o7777);
            st.chown_seq = journal::start_change(
                "chown_root",
                json!({ "path": root, "before": { "uid": uid, "gid": gid, "mode": mode }, "uid": plan.who.uid, "gid": plan.who.gid }),
            );
            st.chown = Some((root.clone(), uid, gid, mode));
            let done = ctx::chown_path(&root, plan.who.uid, plan.who.gid) && fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).is_ok();
            journal::finish_change("chown_root", st.chown_seq, done, json!({}));
            if !done {
                bail!("could not give {mp} to {}", plan.who.user);
            }
        }

        // The real-world check: can the user write there?
        let tested = !plan.choices.read_only && (plan.choices.driver.fs().foreign() || plan.choices.own_top_folder);
        if tested {
            (env.progress)(&format!("Checking that {} can write", plan.who.user));
            let probe = format!("{mp}/.kast-write-test.{}", std::process::id());
            let t = env.rn.run_as_user(&["touch", &probe]);
            let _ = env.rn.run_as_user(&["rm", "-f", &probe]);
            if !t.ok {
                bail!("{} can't write to {mp} ({}). Check the permission preset.", plan.who.user, t.message());
            }
            report.writable_as_user = Some(true);
        }
        Ok(())
    })();
    if let Err(e) = r {
        rollback(&mut st, env, &e.to_string());
        return Err(anyhow::anyhow!("{e}\nNothing was changed: /etc/fstab is back as it was."));
    }

    if !plan.folders.is_empty() {
        (env.progress)("Moving home folders");
        move_folders(plan, env, &mut report)?;
    }
    Ok(report)
}

// --- Home folders -------------------------------------------------------------------------

/// The XDG key of a home folder (`Downloads` -> `DOWNLOAD`).
pub fn xdg_key(dir: &str) -> Option<&'static str> {
    Some(match dir {
        "Desktop" => "DESKTOP",
        "Documents" => "DOCUMENTS",
        "Downloads" => "DOWNLOAD",
        "Music" => "MUSIC",
        "Pictures" => "PICTURES",
        "Videos" => "VIDEOS",
        "Public" => "PUBLICSHARE",
        "Templates" => "TEMPLATES",
        _ => return None,
    })
}

/// `user-dirs.dirs` with these keys pointing at the new places; every other line stays.
pub fn update_user_dirs(text: &str, set: &[(&str, String)]) -> String {
    let mut out = String::new();
    let mut done = vec![false; set.len()];
    for l in text.split_inclusive('\n') {
        let t = l.trim_start();
        let hit = set.iter().position(|(k, _)| t.strip_prefix(&format!("XDG_{k}_DIR=")).is_some());
        match hit {
            Some(i) => {
                out.push_str(&format!("XDG_{}_DIR=\"{}\"\n", set[i].0, set[i].1));
                done[i] = true;
            }
            None => out.push_str(l),
        }
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for (i, (k, v)) in set.iter().enumerate() {
        if !done[i] {
            out.push_str(&format!("XDG_{k}_DIR=\"{v}\"\n"));
        }
    }
    out
}

fn move_folders(plan: &Plan, env: &Env, report: &mut Report) -> Result<()> {
    let mount = sys_path(&plan.choices.mountpoint);
    let foreign = plan.choices.driver.fs().foreign();
    let who = &plan.who;
    let sizes: u64 = plan.folders.iter().map(|f| dir_size(&who.home.join(f))).sum();
    if let Some(free) = (env.free)(&mount) {
        if free < needed(sizes) {
            bail!("Not enough room on the partition for the folders ({} needed, {} free); no folder was moved.", super::human(sizes), super::human(free));
        }
    }
    let mut set: Vec<(&str, String)> = Vec::new();
    for dir in &plan.folders {
        let target = mount.join(dir);
        let home_dir = who.home.join(dir);
        let before = setup::home_state(&home_dir);
        let size = dir_size(&home_dir);
        let was = match &before {
            HomeState::Dir => "dir",
            HomeState::File => "file",
            HomeState::Link(_) => "symlink",
            HomeState::Absent => "absent",
        };
        let changes = matches!(before, HomeState::Dir | HomeState::Absent);
        let seq = changes
            .then(|| journal::start_change("folder_move", json!({ "name": dir, "home": home_dir, "new_path": target, "size": size, "home_was": was, "uid": who.uid, "gid": who.gid, "foreign": foreign })))
            .flatten();
        let r = setup::migrate_one(&mount, &who.home, dir, who.uid, who.gid, foreign, ctx::log_stdio);
        let ok = matches!(r, Ok(Moved::Linked));
        journal::finish_change("folder_move", seq, ok, json!({ "symlink_created": ok }));
        match r? {
            Moved::Linked | Moved::AlreadyLinked => {
                report.moved.push(dir.clone());
                if let Some(k) = xdg_key(dir) {
                    set.push((k, target.display().to_string()));
                }
            }
            Moved::Kept(why) => report.kept.push((dir.clone(), why)),
            Moved::Failed => report.failed.push((dir.clone(), target.display().to_string())),
        }
    }
    if !set.is_empty() {
        let cfg = who.home.join(".config/user-dirs.dirs");
        let old = fs::read_to_string(&cfg).unwrap_or_default();
        let new = update_user_dirs(&old, &set);
        if new != old {
            ctx::mkdir_user(cfg.parent().unwrap())?;
            journal::write_file(&cfg, new.as_bytes(), Some(0o644)).with_context(|| format!("writing {}", cfg.display()))?;
            ctx::chown_path(&cfg, who.uid, who.gid);
        }
    }
    Ok(())
}

// --- Dry run ------------------------------------------------------------------------------

/// What `apply` would do, as lines of text (for `--dry-run` and the review).
pub fn describe(plan: &Plan) -> Vec<String> {
    let e = plan.entry();
    let c = &plan.choices;
    let mut v = vec![
        format!(
            "Partition:    {} ({}{})",
            plan.part.path,
            plan.part.fs().map_or("?", Fs::name),
            plan.part.label.as_deref().map(|l| format!(", \"{l}\"")).unwrap_or_default()
        ),
        format!("fstab line:   {e}"),
        format!("comment:      {}", plan.comment()),
        format!("Mount point:  {} ({})", c.mountpoint, if c.mode == Mode::Automount { "mounted on first access" } else { "mounted at boot" }),
        format!("Test mount:   mount -t {} -o {} {} <private folder>, a write test, unmount", e.vfstype, e.mount_options().join(","), e.spec),
        format!(
            "Then:         check the new fstab with findmnt --verify, write it, systemctl daemon-reload, {}",
            if c.mode == Mode::Automount { "start the automount unit" } else { "mount --target" }
        ),
    ];
    for n in &e.notes {
        v.push(format!("Note:         {n}"));
    }
    if !plan.replace.is_empty() {
        v.push(format!("Replaces:     fstab line(s) {}", plan.replace.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")));
    }
    if c.own_top_folder && !c.driver.fs().foreign() {
        v.push(format!("Ownership:    chown {}:{} and chmod 0755 on {} (the folder only, not what is inside)", plan.who.uid, plan.who.gid, c.mountpoint));
    }
    if plan.unmount_first {
        v.push("Unmount:      it is mounted elsewhere now; kast unmounts it first".into());
    }
    if !plan.folders.is_empty() {
        v.push(format!(
            "Home folders: {} are copied to {} and replaced by links (the originals go only after a full copy)",
            plan.folders.join(", "),
            c.mountpoint
        ));
    }
    v.push(format!("Undo:         {UNDO_HINT}"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::Out;
    use crate::partition::fake::Fake;
    use crate::partition::fstab::Preset;

    const FSTAB: &str = "UUID=D0A3-F918 /boot vfat defaults 0 2\nUUID=2720fb5d-cbdd /  btrfs subvol=/@ 0 0";
    const UUID: &str = "6C62ADEA62ADB96A";

    fn ntfs() -> Partition {
        Partition {
            path: "/dev/sdb1".into(),
            name: "sdb1".into(),
            disk: "/dev/sdb".into(),
            fstype: Some("ntfs".into()),
            uuid: Some(UUID.into()),
            partuuid: Some("bbbb-01".into()),
            label: Some("Data".into()),
            size: 1 << 33,
            ..Default::default()
        }
    }

    fn plan_for(p: Partition, driver: Driver) -> Plan {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let who = Who { user: "p3ter".into(), uid, gid, home: PathBuf::from("/nonexistent-home") };
        Plan {
            choices: Choices::new(&p, driver, "/media/data", uid, gid),
            part: p,
            replace: vec![],
            unmount_first: false,
            folders: vec![],
            who,
            date: "2026-10-18".into(),
        }
    }

    fn ok_blkid() -> (&'static str, Out) {
        ("blkid -p", Out::success(&format!("TYPE=ntfs\nUUID={UUID}\n")))
    }

    fn mounted_json(opts: &str) -> Out {
        Out::success(&format!(r#"{{"filesystems":[{{"source":"/dev/sdb1","fstype":"ntfs3","options":"{opts}","uuid":"{UUID}"}}]}}"#))
    }

    struct Sys {
        _t: tempfile::TempDir,
        run: PathBuf,
    }

    /// A fake system under a temp dir with this fstab.
    fn sys(fstab: &str) -> Sys {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("etc")).unwrap();
        fs::write(t.path().join("etc/fstab"), fstab).unwrap();
        let run = t.path().join("run");
        Sys { _t: t, run }
    }

    fn go(s: &Sys, rn: &Fake, plan: &Plan) -> Result<Report> {
        ctx::init_for_tests();
        let env = Env { rn, probe_base: s.run.clone(), progress: &|_| {}, free: &avail };
        let root = s._t.path().to_path_buf();
        journal::with_root(&root, || {
            journal::begin_run(&["partition"], "p3ter").unwrap();
            let r = apply(plan, &env);
            journal::end_run();
            r
        })
    }

    fn changes(s: &Sys) -> Vec<(String, journal::Status)> {
        let root = s._t.path().to_path_buf();
        journal::with_root(&root, || {
            let runs = journal::runs(&journal::load(&journal::state_dir()).unwrap().entries);
            runs.iter().flat_map(|r| r.changes.iter().map(|c| (c.kind.clone(), c.status))).collect()
        })
    }

    fn fstab_bytes(s: &Sys) -> Vec<u8> {
        fs::read(s._t.path().join("etc/fstab")).unwrap()
    }

    #[test]
    fn happy_path_test_mounts_first_writes_the_line_and_mounts_only_that_entry() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![ok_blkid(), ("findmnt --target", mounted_json("rw,relatime,uid=1000,gid=1001,fmask=0177,dmask=0077"))]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.choices.uid = 1000;
        plan.choices.gid = 1001;
        let r = go(&s, &rn, &plan).unwrap();
        assert!(r.mounted);
        assert_eq!(r.writable_as_user, Some(true));
        let new = String::from_utf8(fstab_bytes(&s)).unwrap();
        assert!(new.starts_with(FSTAB), "everything else byte for byte");
        assert!(new.ends_with("\n# kast: data partition (Data), added 2026-10-18\nUUID=6C62ADEA62ADB96A /media/data ntfs3 uid=1000,gid=1001,dmask=0077,fmask=0177,iocharset=utf8,windows_names,nofail,x-systemd.device-timeout=10s 0 0\n"), "{new}");
        assert!(s._t.path().join("media/data").is_dir());
        // order: the test mount happens before the file is written; no mount -a
        let probe_mount = rn.index("mount -t ntfs3");
        assert!(rn.calls.borrow()[probe_mount].contains("-o uid=1000,gid=1001,dmask=0077,fmask=0177,iocharset=utf8,windows_names UUID=6C62ADEA62ADB96A /"));
        assert!(!rn.calls.borrow()[probe_mount].contains("nofail") && !rn.calls.borrow()[probe_mount].contains("x-systemd"));
        assert!(probe_mount < rn.index("systemctl daemon-reload") && rn.index("systemctl daemon-reload") < rn.index("mount --target /media/data"));
        assert!(!rn.called("mount -a"));
        assert_eq!(rn.count("--verify"), 2, "the current file and the candidate");
        assert!(rn.called("umount") && rn.called("as-user touch /media/data/.kast-write-test"));
        assert_eq!(changes(&s).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["dir_create", "fstab_entry", "file_write", "mount"]);
        assert!(changes(&s).iter().all(|(_, st)| *st == journal::Status::Done));
        // a copy of the old file for people who look
        let baks: Vec<_> =
            fs::read_dir(s._t.path().join("etc")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("fstab.kast-bak.")).collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(fs::read(baks[0].path()).unwrap(), FSTAB.as_bytes());
        // the private test mount folder is gone
        assert_eq!(fs::read_dir(&s.run).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("kast-probe.")).count(), 0);
    }

    #[test]
    fn a_dirty_ntfs_volume_stops_at_the_test_mount_and_changes_nothing() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![
            ok_blkid(),
            ("mount -t ntfs3", Out::failure("mount: wrong fs type, bad option, bad superblock on /dev/sdb1")),
            ("dmesg", Out::success("ntfs3: sdb1: volume is dirty and \"force\" flag is not set!\nunrelated line\n")),
        ]);
        let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
        assert!(e.contains("volume is dirty") && e.contains("Fast Startup") && e.contains("Shift"), "{e}");
        assert!(!e.contains("unrelated"));
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
        assert!(!s._t.path().join("media").exists(), "no mount point created");
        assert!(changes(&s).is_empty());
        assert!(!rn.called("daemon-reload") && !rn.called("--verify"));
        assert_eq!(fs::read_dir(&s.run).unwrap().count(), 0, "no probe folder left");
    }

    #[test]
    fn a_failed_findmnt_verify_leaves_fstab_alone_and_removes_the_mount_point() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![
            ok_blkid(),
            (
                "kast-fstab.",
                Out {
                    ok: false,
                    code: Some(1),
                    stdout: "/media/data\n   [E] unreachable on boot required source: UUID=6C62ADEA62ADB96A\n".into(),
                    stderr: String::new(),
                },
            ),
        ]);
        let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
        assert!(e.contains("findmnt --verify found problems") && e.contains("UUID=6C62ADEA62ADB96A"), "{e}");
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
        assert!(!s._t.path().join("media/data").exists());
        assert!(!rn.called("mount --target"));
        assert!(changes(&s).iter().all(|(_, st)| *st == journal::Status::Undone), "{:?}", changes(&s));
    }

    #[test]
    fn a_mount_failure_after_the_write_restores_the_exact_previous_bytes() {
        for original in [FSTAB, "UUID=a / ext4 defaults 0 1\r\n# crlf\r\n", ""] {
            let s = sys(original);
            let rn = Fake::new(vec![ok_blkid(), ("mount --target", Out::failure("mount: /media/data: can't find in /etc/fstab."))]);
            let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
            assert!(e.contains("mounting /media/data through /etc/fstab failed") && e.contains("back as it was"), "{e}");
            assert_eq!(fstab_bytes(&s), original.as_bytes());
            assert_eq!(rn.count("daemon-reload"), 2, "reloaded again after restoring");
            assert!(!s._t.path().join("media/data").exists());
            let ch = changes(&s);
            assert!(!ch.is_empty() && ch.iter().all(|(_, st)| *st == journal::Status::Undone), "{ch:?}");
        }
    }

    #[test]
    fn a_wrong_mount_after_the_write_also_rolls_back_and_unmounts() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![ok_blkid(), ("findmnt --target", mounted_json("rw,uid=999,gid=999,fmask=0177,dmask=0077"))]);
        let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
        assert!(e.contains("uid=999"), "{e}");
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
        assert!(rn.calls.borrow().iter().any(|c| c == "umount /media/data"));
    }

    #[test]
    fn the_user_not_being_able_to_write_rolls_back() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![
            ok_blkid(),
            ("findmnt --target", mounted_json("rw,uid=1,gid=1,fmask=0177,dmask=0077")),
            ("as-user touch", Out::failure("Permission denied")),
        ]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.choices.uid = 1;
        plan.choices.gid = 1;
        let e = go(&s, &rn, &plan).unwrap_err().to_string();
        assert!(e.contains("can't write"), "{e}");
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
    }

    #[test]
    fn automount_starts_the_unit_and_does_not_call_mount() {
        let s = sys(FSTAB);
        let mut p = ntfs();
        p.external = true;
        p.fstype = Some("exfat".into());
        p.uuid = Some("A40D-85E7".into());
        let json = r#"{"filesystems":[{"fstype":"autofs","options":"rw"},{"source":"/dev/sdb1","fstype":"exfat","options":"rw,uid=1000,gid=1001,fmask=0177,dmask=0077","uuid":"A40D-85E7"}]}"#;
        let rn = Fake::new(vec![("blkid -p", Out::success("TYPE=exfat\nUUID=A40D-85E7\n")), ("findmnt --target", Out::success(json))]);
        let mut plan = plan_for(p, Driver::Exfat);
        plan.choices.uid = 1000;
        plan.choices.gid = 1001;
        let r = go(&s, &rn, &plan).unwrap();
        assert!(r.mounted);
        assert!(rn.called("systemctl start media-data.automount") && !rn.called("mount --target"));
        assert!(String::from_utf8(fstab_bytes(&s)).unwrap().contains("x-systemd.device-timeout=5s,x-systemd.automount,x-systemd.idle-timeout=5min 0 0"));
    }

    #[test]
    fn a_device_that_changed_or_is_mounted_elsewhere_is_refused() {
        let s = sys(FSTAB);
        let rn = Fake::new(vec![("blkid -p", Out::success("TYPE=ntfs\nUUID=FFFFFFFFFFFFFFFF\n"))]);
        let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
        assert!(e.contains("no longer has the UUID"), "{e}");
        let rn = Fake::new(vec![ok_blkid(), ("findmnt -rn -S", Out::success("/run/media/p3ter/Data\n"))]);
        let e = go(&s, &rn, &plan_for(ntfs(), Driver::Ntfs3)).unwrap_err().to_string();
        assert!(e.contains("mounted at /run/media/p3ter/Data"), "{e}");
        assert!(!rn.called("umount"));
        // with permission it unmounts first
        let rn = Fake::new(vec![
            ok_blkid(),
            ("findmnt -rn -S", Out::success("/run/media/p3ter/Data\n")),
            ("findmnt --target", mounted_json("rw,uid=1000,gid=1001,fmask=0177,dmask=0077")),
        ]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.unmount_first = true;
        plan.choices.uid = 1000;
        plan.choices.gid = 1001;
        go(&s, &rn, &plan).unwrap();
        assert!(rn.calls.borrow().iter().any(|c| c == "umount /run/media/p3ter/Data"));
        // busy: say who, stop
        let s = sys(FSTAB);
        let rn = Fake::new(vec![
            ok_blkid(),
            ("findmnt -rn -S", Out::success("/run/media/p3ter/Data\n")),
            ("umount /run/media", Out::failure("umount: /run/media/p3ter/Data: target is busy.")),
            ("fuser", Out { ok: true, code: Some(0), stdout: String::new(), stderr: "USER PID ACCESS COMMAND\np3ter 4242 ..c.. bash".into() }),
        ]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.unmount_first = true;
        let e = go(&s, &rn, &plan).unwrap_err().to_string();
        assert!(e.contains("busy") && e.contains("4242"), "{e}");
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
    }

    #[test]
    fn a_conflicting_entry_needs_the_replace_flag_and_is_journaled_as_replaced() {
        let old = "UUID=6C62ADEA62ADB96A /media/old ntfs3 defaults 0 0";
        let text = format!("{FSTAB}\n{old}\n");
        let s = sys(&text);
        let rn = Fake::new(vec![ok_blkid(), ("findmnt --target", mounted_json("rw,uid=1000,gid=1001,fmask=0177,dmask=0077"))]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.choices.uid = 1000;
        plan.choices.gid = 1001;
        assert!(go(&s, &rn, &plan).unwrap_err().to_string().contains("already has an entry"));
        assert_eq!(fstab_bytes(&s), text.as_bytes());
        plan.replace = vec![3];
        go(&s, &rn, &plan).unwrap();
        let new = String::from_utf8(fstab_bytes(&s)).unwrap();
        assert!(!new.contains(old) && new.matches("UUID=6C62ADEA62ADB96A").count() == 1);
        let root = s._t.path().to_path_buf();
        journal::with_root(&root, || {
            let runs = journal::runs(&journal::load(&journal::state_dir()).unwrap().entries);
            let c = runs.iter().flat_map(|r| &r.changes).find(|c| c.kind == "fstab_entry").unwrap();
            assert_eq!(c.data["replaced"][0], old);
            assert_eq!(c.data["mountpoint"], "/media/data");
        });
    }

    #[test]
    fn ext4_top_folder_is_handed_to_the_user_and_the_old_owner_is_journaled() {
        let s = sys(FSTAB);
        let mut p = ntfs();
        p.fstype = Some("ext4".into());
        p.uuid = Some("8b1e2f3a-aaaa-bbbb-cccc-0123456789ab".into());
        let json = r#"{"filesystems":[{"source":"/dev/sdb1","fstype":"ext4","options":"rw,noatime","uuid":"8b1e2f3a-aaaa-bbbb-cccc-0123456789ab"}]}"#;
        let rn =
            Fake::new(vec![("blkid -p", Out::success("TYPE=ext4\nUUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab\n")), ("findmnt --target", Out::success(json))]);
        let mut plan = plan_for(p, Driver::Ext4);
        plan.choices.own_top_folder = true;
        let r = go(&s, &rn, &plan).unwrap();
        assert_eq!(r.writable_as_user, Some(true));
        let ch = changes(&s);
        assert_eq!(ch.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["dir_create", "fstab_entry", "file_write", "mount", "chown_root"]);
        let root = s._t.path().to_path_buf();
        journal::with_root(&root, || {
            let runs = journal::runs(&journal::load(&journal::state_dir()).unwrap().entries);
            let c = runs[0].changes.iter().find(|c| c.kind == "chown_root").unwrap();
            assert!(c.data["before"]["uid"].is_u64() && c.data["before"]["mode"].is_u64());
        });
        assert!(String::from_utf8(fstab_bytes(&s)).unwrap().ends_with("/media/data ext4 nofail,x-systemd.device-timeout=10s 0 2\n"));
        // without it, nothing is tested as the user and nothing is chowned
        let s = sys(FSTAB);
        let mut plan = plan_for(
            {
                let mut p = ntfs();
                p.fstype = Some("ext4".into());
                p.uuid = Some("8b1e2f3a-aaaa-bbbb-cccc-0123456789ab".into());
                p
            },
            Driver::Ext4,
        );
        plan.choices.own_top_folder = false;
        let rn =
            Fake::new(vec![("blkid -p", Out::success("TYPE=ext4\nUUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab\n")), ("findmnt --target", Out::success(json))]);
        assert_eq!(go(&s, &rn, &plan).unwrap().writable_as_user, None);
        assert!(!changes(&s).iter().any(|(k, _)| k == "chown_root"));
    }

    #[test]
    fn folders_are_moved_journaled_and_the_xdg_file_updated() {
        let s = sys(FSTAB);
        let home = s._t.path().join("home/p3ter");
        fs::create_dir_all(home.join("Documents/sub")).unwrap();
        fs::write(home.join("Documents/sub/a.txt"), "doc").unwrap();
        fs::create_dir_all(s._t.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(s._t.path().join("elsewhere"), home.join("Music")).unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::write(home.join(".config/user-dirs.dirs"), "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_CUSTOM_DIR=\"$HOME/x\"\n").unwrap();
        let rn = Fake::new(vec![ok_blkid(), ("findmnt --target", mounted_json("rw,uid=1000,gid=1001,fmask=0177,dmask=0077"))]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.choices.uid = 1000;
        plan.choices.gid = 1001;
        plan.who.home = home.clone();
        plan.folders = vec!["Documents".into(), "Music".into(), "Projects".into()];
        let r = go(&s, &rn, &plan).unwrap();
        let mount = s._t.path().join("media/data");
        assert_eq!(r.moved, ["Documents", "Projects"]);
        assert_eq!(r.kept.len(), 1, "{:?}", r.kept);
        assert_eq!(fs::read_link(home.join("Documents")).unwrap(), mount.join("Documents"));
        assert_eq!(fs::read_to_string(mount.join("Documents/sub/a.txt")).unwrap(), "doc");
        assert_eq!(fs::read_link(home.join("Music")).unwrap(), s._t.path().join("elsewhere"));
        let dirs = fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap();
        assert!(dirs.contains("XDG_CUSTOM_DIR=\"$HOME/x\"") && dirs.contains(&format!("XDG_DOCUMENTS_DIR=\"{}\"", mount.join("Documents").display())));
        assert!(dirs.contains("XDG_DOWNLOAD_DIR=\"$HOME/Downloads\""), "folders not moved keep their entry");
        let kinds: Vec<String> = changes(&s).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds.iter().filter(|k| *k == "folder_move").count(), 2, "{kinds:?}");
        assert_eq!(kinds.last().unwrap(), "file_write");
    }

    #[test]
    fn not_enough_room_stops_at_the_test_mount_before_anything_changes() {
        let s = sys(FSTAB);
        let home = s._t.path().join("home/p3ter");
        fs::create_dir_all(home.join("Documents")).unwrap();
        fs::write(home.join("Documents/a"), vec![0u8; 4096]).unwrap();
        let rn = Fake::new(vec![ok_blkid()]);
        let mut plan = plan_for(ntfs(), Driver::Ntfs3);
        plan.who.home = home.clone();
        plan.folders = vec!["Documents".into()];
        let root = s._t.path().to_path_buf();
        let tight = |_: &Path| Some(10u64 << 20);
        let env = Env { rn: &rn, probe_base: s.run.clone(), progress: &|_| {}, free: &tight };
        ctx::init_for_tests();
        let e = journal::with_root(&root, || apply(&plan, &env)).unwrap_err().to_string();
        assert!(e.contains("Not enough room"), "{e}");
        assert_eq!(fstab_bytes(&s), FSTAB.as_bytes());
        assert!(!home.join("Documents").is_symlink());
        assert!(!rn.called("--verify"));
    }

    #[test]
    fn what_the_kernel_reports_is_compared_semantically() {
        let p = ntfs();
        let plan = plan_for(p.clone(), Driver::Ntfs3);
        let e = plan.entry();
        let ok = |o: &str| check_mounted(&format!(r#"{{"filesystems":[{{"fstype":"ntfs3","options":"{o}","uuid":"{UUID}"}}]}}"#), &e, &p, Driver::Ntfs3);
        let uid = plan.choices.uid;
        let gid = plan.choices.gid;
        assert!(ok(&format!("rw,nosuid,relatime,uid={uid},gid={gid},fmask=177,dmask=77,iocharset=utf8")).is_ok(), "masks compare as numbers");
        assert!(ok(&format!("rw,uid={uid},gid={gid},fmask=0133,dmask=0077")).unwrap_err().contains("fmask"));
        assert!(ok(&format!("ro,uid={uid},gid={gid},fmask=0177,dmask=0077")).unwrap_err().contains("read-only"));
        assert!(ok("rw,fmask=0177,dmask=0077").is_err(), "no uid shown");
        assert!(
            check_mounted(r#"{"filesystems":[{"fstype":"vfat","options":"rw","uuid":"6C62ADEA62ADB96A"}]}"#, &e, &p, Driver::Ntfs3)
                .unwrap_err()
                .contains("vfat")
        );
        assert!(check_mounted(r#"{"filesystems":[{"fstype":"ntfs3","options":"rw","uuid":"OTHER"}]}"#, &e, &p, Driver::Ntfs3).unwrap_err().contains("OTHER"));
        assert!(check_mounted(r#"{"filesystems":[{"fstype":"autofs","options":"rw"}]}"#, &e, &p, Driver::Ntfs3).is_err(), "only the placeholder");
        // read-only was asked for
        let mut ro = plan.clone();
        ro.choices.read_only = true;
        assert!(
            check_mounted(
                &format!(r#"{{"filesystems":[{{"fstype":"ntfs3","options":"ro,uid={uid},gid={gid},fmask=0177,dmask=0077","uuid":"{UUID}"}}]}}"#),
                &ro.entry(),
                &p,
                Driver::Ntfs3
            )
            .is_ok()
        );
        // ntfs-3g shows up as fuseblk without uid=
        let g = plan_for(p.clone(), Driver::Ntfs3g);
        assert!(
            check_mounted(
                &format!(r#"{{"filesystems":[{{"fstype":"fuseblk","options":"rw,user_id=0,group_id=0","uuid":"{UUID}"}}]}}"#),
                &g.entry(),
                &p,
                Driver::Ntfs3g
            )
            .is_ok()
        );
        // btrfs subvol
        let mut b = plan_for(Partition { fstype: Some("btrfs".into()), uuid: Some("u".into()), ..ntfs() }, Driver::Btrfs);
        b.choices.subvol = Some("/@data".into());
        assert!(
            check_mounted(r#"{"filesystems":[{"fstype":"btrfs","options":"rw,subvolid=257,subvol=/@data","uuid":"u"}]}"#, &b.entry(), &b.part, Driver::Btrfs)
                .is_ok()
        );
        assert!(
            check_mounted(r#"{"filesystems":[{"fstype":"btrfs","options":"rw,subvolid=5,subvol=/","uuid":"u"}]}"#, &b.entry(), &b.part, Driver::Btrfs).is_err()
        );
    }

    #[test]
    fn small_pieces() {
        assert_eq!(
            parse_subvolumes("ID 256 gen 12 top level 5 path @data\nID 257 gen 9 top level 256 path @data/nested dir\n"),
            ["/@data", "/@data/nested dir"]
        );
        let rn = Fake::new(vec![("systemd-escape", Out::failure("not installed"))]);
        assert_eq!(unit_name(&rn, "/media/data", "mount"), "media-data.mount");
        assert_eq!(unit_name(&rn, "/media/my data", "automount"), "media-my\\x20data.automount");
        assert_eq!(unit_name(&rn, "/media/a-b", "mount"), "media-a\\x2db.mount");
        let rn = Fake::new(vec![("systemd-escape", Out::success("x.mount\n"))]);
        assert_eq!(unit_name(&rn, "/x", "mount"), "x.mount");
        assert_eq!(update_user_dirs("", &[("DOWNLOAD", "/m/Downloads".into())]), "XDG_DOWNLOAD_DIR=\"/m/Downloads\"\n");
        assert_eq!(
            update_user_dirs("# c\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"", &[("DESKTOP", "/m/D".into()), ("MUSIC", "/m/M".into())]),
            "# c\nXDG_DESKTOP_DIR=\"/m/D\"\nXDG_MUSIC_DIR=\"/m/M\"\n"
        );
        assert_eq!(needed(0), 64 << 20);
        let m = explain_mount_failure(Fs::Ntfs, "wrong fs type", "ntfs3: sdb1: volume is dirty and \"force\" flag is not set!");
        assert!(m.contains("Fast Startup"));
        assert!(!explain_mount_failure(Fs::Ext4, "wrong fs type", "dirty").contains("Fast Startup"));
        let d = describe(&plan_for(ntfs(), Driver::Ntfs3));
        assert!(
            d.iter().any(|l| l.starts_with("fstab line:   UUID=6C62ADEA62ADB96A /media/data ntfs3 "))
                && d.last().unwrap().contains("kast undo --last --step partition")
        );
        let _ = Preset::OnlyMe;
    }
}
