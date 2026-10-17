//! The change journal: every change kast makes to the system, recorded so it can be listed
//! and undone later.
//!
//! - `/var/lib/kast/journal.jsonl`: append-only, one JSON object per line, fsynced. A change
//!   is written as `started` (with its before-state) *before* it happens, then `done` or
//!   `failed`; a `started` without an outcome is an incomplete change (crash, power loss)
//!   that undo can still revert from its before-state.
//! - `/var/lib/kast/files/<run>/<sha256>`: copies of files before kast changed them.
//! - Root only: the directory is 0700 and files 0600. Undo replays these as root, so a
//!   journal or copy that isn't owned by us or that others can write is refused, never used.
//!
//! Entries carry `"v": 1`. Unknown fields are ignored; an entry with a higher `v` comes from
//! a newer kast and is shown as such, but not undone.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The journal format this kast writes and understands.
pub const VERSION: u32 = 1;
pub const STATE_DIR: &str = "/var/lib/kast";

// --- Paths ---------------------------------------------------------------------------------

thread_local! {
    /// Tests put the "system" under a temp directory; in kast itself it's always `/`.
    static ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// The run being recorded on this thread (the job thread, or the CLI's main thread).
    static CURRENT: RefCell<Option<Arc<Journal>>> = const { RefCell::new(None) };
}

/// A system path (`/etc/pacman.conf`), under the test root when one is set.
pub fn sys_path(p: impl AsRef<Path>) -> PathBuf {
    let p = p.as_ref();
    ROOT.with(|r| match &*r.borrow() {
        Some(root) => root.join(p.strip_prefix("/").unwrap_or(p)),
        None => p.to_path_buf(),
    })
}

/// Run `f` with the system under `root` (tests).
#[cfg(test)]
pub fn with_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    ROOT.with(|r| *r.borrow_mut() = Some(root.to_path_buf()));
    let out = f();
    ROOT.with(|r| *r.borrow_mut() = None);
    CURRENT.with(|c| *c.borrow_mut() = None);
    out
}

pub fn state_dir() -> PathBuf {
    sys_path(STATE_DIR)
}

// --- Hashing and safety checks ------------------------------------------------------------

pub fn sha256(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(p: &Path) -> Option<String> {
    fs::read(p).ok().map(|d| sha256(&d))
}

fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Refuse what others could have written: it must be ours (root, in kast) and not group- or
/// world-writable, and not a symlink.
pub fn check_secure(p: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if m.file_type().is_symlink() {
        return Err(format!("{} is a symlink; refusing to use it", p.display()));
    }
    if m.uid() != euid() {
        return Err(format!("{} is owned by uid {}, not {}; refusing to use it", p.display(), m.uid(), euid()));
    }
    if m.mode() & 0o022 != 0 {
        return Err(format!("{} can be written by other users (mode {:o}); refusing to use it", p.display(), m.mode() & 0o777));
    }
    Ok(())
}

fn make_private_dir(p: &Path) -> io::Result<()> {
    fs::create_dir_all(p)?;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))
}

// --- Entries -------------------------------------------------------------------------------

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub run: String,
    /// The change this line is about (a `started` line and its outcome share it)
    pub seq: u64,
    #[serde(default)]
    pub ts: String,
    pub kind: String,
    pub status: String,
    /// The step that made the change ("repo", "packages", ...)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub step: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Written ahead, no outcome: kast stopped in the middle of it
    Incomplete,
    Done,
    Failed,
    /// Reverted by an undo
    Undone,
}

/// A change, with everything known about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub seq: u64,
    pub kind: String,
    pub step: String,
    pub status: Status,
    /// The `started` data merged with the outcome's
    pub data: Value,
    /// Written by a newer kast (higher `v`): shown, never undone
    pub newer: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: String,
    pub ts: String,
    pub version: String,
    pub host: String,
    pub user: String,
    pub steps: Vec<String>,
    pub changes: Vec<Change>,
}

impl Run {
    #[cfg(test)]
    pub fn change(&self, seq: u64) -> Option<&Change> {
        self.changes.iter().find(|c| c.seq == seq)
    }
}

/// Merge `b` into `a` (objects only).
fn merge(a: &mut Value, b: &Value) {
    if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
}

/// Group the lines into runs and changes, oldest first. `undo` lines mark their target.
pub fn runs(entries: &[Entry]) -> Vec<Run> {
    let mut order: Vec<String> = Vec::new();
    let mut by_run: BTreeMap<String, Run> = BTreeMap::new();
    let mut undone: Vec<(String, u64)> = Vec::new();
    for e in entries {
        let newer = e.v > VERSION;
        if e.kind == "undo" && e.status == "done" && !newer {
            if let (Some(r), Some(s)) = (e.data["target_run"].as_str(), e.data["target_seq"].as_u64()) {
                undone.push((r.to_string(), s));
            }
            continue;
        }
        if e.kind == "undo" {
            continue;
        }
        let run = by_run.entry(e.run.clone()).or_insert_with(|| {
            order.push(e.run.clone());
            Run {
                id: e.run.clone(),
                ts: e.ts.clone(),
                version: String::new(),
                host: String::new(),
                user: String::new(),
                steps: Vec::new(),
                changes: Vec::new(),
            }
        });
        if e.kind == "run" {
            run.version = e.data["version"].as_str().unwrap_or_default().to_string();
            run.host = e.data["host"].as_str().unwrap_or_default().to_string();
            run.user = e.data["user"].as_str().unwrap_or_default().to_string();
            run.steps = e.data["steps"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
            run.ts = e.ts.clone();
            continue;
        }
        match run.changes.iter_mut().find(|c| c.seq == e.seq) {
            Some(c) => {
                merge(&mut c.data, &e.data);
                c.newer |= newer;
                c.status = match e.status.as_str() {
                    "done" => Status::Done,
                    "failed" => Status::Failed,
                    _ => c.status,
                };
            }
            None => run.changes.push(Change {
                seq: e.seq,
                kind: e.kind.clone(),
                step: e.step.clone(),
                status: match e.status.as_str() {
                    "done" => Status::Done,
                    "failed" => Status::Failed,
                    _ => Status::Incomplete,
                },
                data: e.data.clone(),
                newer,
            }),
        }
    }
    for (r, s) in undone {
        if let Some(c) = by_run.get_mut(&r).and_then(|run| run.changes.iter_mut().find(|c| c.seq == s)) {
            c.status = Status::Undone;
        }
    }
    order.into_iter().filter_map(|id| by_run.remove(&id)).collect()
}

/// What `load` found.
#[derive(Debug, Default)]
pub struct Loaded {
    pub entries: Vec<Entry>,
    /// Lines that couldn't be read, and files refused as unsafe
    pub problems: Vec<String>,
}

/// Read the journal in `dir` (normally `state_dir()`). A missing journal is an empty one;
/// an unsafe one is refused.
pub fn load(dir: &Path) -> Result<Loaded, String> {
    let path = dir.join("journal.jsonl");
    if !path.exists() {
        return Ok(Loaded::default());
    }
    check_secure(dir)?;
    check_secure(&path)?;
    let f = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Loaded::default();
    for (i, line) in io::BufReader::new(f).lines().enumerate() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(e) => out.entries.push(e),
            // A torn last line after a crash, or garbage: skip it, say so
            Err(e) => out.problems.push(format!("journal line {}: {e}", i + 1)),
        }
    }
    Ok(out)
}

// --- Writing -------------------------------------------------------------------------------

/// The open journal of one run.
pub struct Journal {
    dir: PathBuf,
    pub run: String,
    seq: AtomicU64,
    file: Mutex<File>,
    step: Mutex<String>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Journal({})", self.run)
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

fn run_id() -> String {
    let mut rnd = [0u8; 3];
    if File::open("/dev/urandom").and_then(|mut f| io::Read::read_exact(&mut f, &mut rnd)).is_err() {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        rnd = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    }
    format!("{}-{:02x}{:02x}{:02x}", chrono::Local::now().format("%Y%m%d-%H%M%S"), rnd[0], rnd[1], rnd[2])
}

impl Journal {
    /// Open `dir`'s journal for a new run and write the run's header.
    pub fn open(dir: &Path, steps: &[&str], user: &str) -> io::Result<Journal> {
        make_private_dir(dir)?;
        let path = dir.join("journal.jsonl");
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        if let Err(e) = check_secure(dir).and_then(|_| check_secure(&path)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, e));
        }
        let host = fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
        let first = if steps.len() == 1 { steps[0].to_string() } else { String::new() };
        let j = Journal { dir: dir.to_path_buf(), run: run_id(), seq: AtomicU64::new(0), file: Mutex::new(file), step: Mutex::new(first) };
        j.append("run", "done", 0, json!({ "version": env!("CARGO_PKG_VERSION"), "host": host, "user": user, "steps": steps }))?;
        Ok(j)
    }

    fn append(&self, kind: &str, status: &str, seq: u64, data: Value) -> io::Result<()> {
        let step = self.step.lock().map(|s| s.clone()).unwrap_or_default();
        let e = Entry { v: VERSION, run: self.run.clone(), seq, ts: now(), kind: kind.into(), status: status.into(), step, data };
        let mut line = serde_json::to_string(&e).map_err(io::Error::other)?;
        line.push('\n');
        let mut f = self.file.lock().map_err(|_| io::Error::other("journal lock poisoned"))?;
        f.write_all(line.as_bytes())?;
        f.sync_data()
    }

    /// Write a change ahead: returns its number for `finish`.
    pub fn start(&self, kind: &str, data: Value) -> io::Result<u64> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.append(kind, "started", seq, data)?;
        Ok(seq)
    }

    pub fn finish(&self, kind: &str, seq: u64, ok: bool, data: Value) -> io::Result<()> {
        self.append(kind, if ok { "done" } else { "failed" }, seq, data)
    }

    /// A finished change in one line (nothing to write ahead: e.g. a file kast created).
    pub fn note(&self, kind: &str, data: Value) -> io::Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.append(kind, "done", seq, data)
    }

    /// Keep a copy of `data` under its hash; identical content is stored once.
    pub fn store(&self, data: &[u8]) -> io::Result<String> {
        let hash = sha256(data);
        let dir = self.dir.join("files").join(&self.run);
        make_private_dir(&self.dir.join("files"))?;
        make_private_dir(&dir)?;
        let p = dir.join(&hash);
        if !p.exists() {
            let mut f = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&p)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        Ok(hash)
    }
}

/// Where a run's copy of the content with this hash is.
pub fn copy_path(dir: &Path, run: &str, hash: &str) -> PathBuf {
    dir.join("files").join(run).join(hash)
}

/// Read a stored copy, checking it is safe and still has its hash.
pub fn read_copy(dir: &Path, run: &str, hash: &str) -> Result<Vec<u8>, String> {
    let p = copy_path(dir, run, hash);
    check_secure(&p)?;
    let data = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if sha256(&data) != hash {
        return Err(format!("{} no longer matches its hash; refusing to use it", p.display()));
    }
    Ok(data)
}

// --- The current run -----------------------------------------------------------------------

/// Start recording a run on this thread. Without root (or with a dry run) nothing is
/// recorded and the engines just do their work.
pub fn begin_run(steps: &[&str], user: &str) -> Option<Arc<Journal>> {
    let j = match Journal::open(&state_dir(), steps, user) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            crate::ctx::log_file_only("WARN", &format!("Changes of this run are not recorded for undo: {e}"));
            return None;
        }
    };
    CURRENT.with(|c| *c.borrow_mut() = Some(j.clone()));
    crate::ctx::log_file_only("INFO", &format!("Recording changes for undo as run {}", j.run));
    Some(j)
}

/// The step the next changes belong to.
pub fn set_step(step: &str) {
    if let Some(j) = current()
        && let Ok(mut s) = j.step.lock()
    {
        *s = step.to_string();
    }
}

pub fn end_run() {
    CURRENT.with(|c| *c.borrow_mut() = None);
}

pub fn current() -> Option<Arc<Journal>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Record the run of `f` when running as root and not a dry run.
pub fn recorded<T>(steps: &[&str], dry_run: bool, f: impl FnOnce() -> T) -> T {
    let c = crate::ctx::ctx();
    let on = c.is_root && !dry_run && current().is_none();
    if on {
        begin_run(steps, &c.target_user);
    }
    let out = f();
    if on {
        end_run();
    }
    out
}

fn warn(e: io::Error) {
    crate::ctx::log_file_only("WARN", &format!("Could not record a change for undo: {e}"));
}

// --- What the engines call -----------------------------------------------------------------

/// The before-state of `path` for the journal, with a copy of its content kept.
fn before_state(j: &Journal, path: &Path) -> io::Result<Value> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({ "absent": true })),
        Err(e) => Err(e),
        Ok(m) => {
            let data = fs::read(path)?;
            let hash = j.store(&data)?;
            Ok(json!({ "sha256": hash, "mode": m.mode() & 0o7777, "uid": m.uid(), "gid": m.gid() }))
        }
    }
}

/// Write `data` to `path` atomically and record it: the previous content (or that there
/// was none) before, the new hash after. `mode` applies to a new file.
pub fn write_file(path: &Path, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    let Some(j) = current() else {
        return write_plain(path, data, mode);
    };
    let before = before_state(&j, path)?;
    let kind = if before["absent"] == true { "file_create" } else { "file_write" };
    let seq = j.start(kind, json!({ "path": path, "before": before }))?;
    let result = write_plain(path, data, mode);
    let after = sha256(data);
    if let Err(e) = j.finish(kind, seq, result.is_ok(), json!({ "after_sha256": after })) {
        warn(e);
    }
    result
}

fn write_plain(path: &Path, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    let new = !path.exists();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    crate::ctx::write_atomic(path, data)?;
    if let (true, Some(m)) = (new, mode) {
        fs::set_permissions(path, fs::Permissions::from_mode(m))?;
    }
    Ok(())
}

/// `howdy set key value`, with the previous value.
pub fn howdy_set(key: &str, before: Option<&str>, after: &str, run: impl FnOnce() -> bool) -> bool {
    let Some(j) = current() else { return run() };
    let seq = j.start("howdy_set", json!({ "key": key, "before": before, "after": after }));
    let ok = run();
    if let Ok(seq) = seq {
        j.finish("howdy_set", seq, ok, json!({})).unwrap_or_else(warn);
    }
    ok
}

/// `systemctl enable --now unit`, with whether it was enabled / active before.
pub fn service_enable(unit: &str, was_enabled: bool, was_active: bool, run: impl FnOnce() -> bool) -> bool {
    let Some(j) = current() else { return run() };
    let seq = j.start("service_enable", json!({ "unit": unit, "was_enabled": was_enabled, "was_active": was_active }));
    let ok = run();
    if let Ok(seq) = seq {
        j.finish("service_enable", seq, ok, json!({})).unwrap_or_else(warn);
    }
    ok
}

/// A signing key imported and locally signed, and whether it was there before.
pub fn pacman_key(fpr: &str, was_present: bool, run: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    let Some(j) = current() else { return run() };
    let seq = j.start("pacman_key", json!({ "fpr": fpr, "was_present": was_present }));
    let r = run();
    if let Ok(seq) = seq {
        j.finish("pacman_key", seq, r.is_ok(), json!({})).unwrap_or_else(warn);
    }
    r
}

/// A package kast installed, as found after the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub repo: String,
    /// Asked for (else pulled in as a dependency)
    pub explicit: bool,
}

/// The packages a transaction added: in `after` but not in `before`. `targets` are the
/// names asked for; `info` gives version and repo.
pub fn new_packages(before: &[String], after: &[String], targets: &[&str], info: impl Fn(&str) -> (String, String)) -> Vec<Installed> {
    let target = |n: &str| targets.iter().any(|t| t.rsplit('/').next() == Some(n));
    after
        .iter()
        .filter(|n| !before.contains(n))
        .map(|n| {
            let (version, repo) = info(n);
            Installed { name: n.clone(), version, repo, explicit: target(n) }
        })
        .collect()
}

pub fn pkg_installs(pkgs: &[Installed]) {
    let Some(j) = current() else { return };
    for p in pkgs {
        j.note("pkg_install", json!({ "name": p.name, "version": p.version, "repo": p.repo, "reason": if p.explicit { "explicit" } else { "dependency" } }))
            .unwrap_or_else(warn);
    }
}

/// A legacy package a swap removed (`sine-bin` for `archrepo/sine`).
pub fn pkg_replace(name: &str, version: &str, by: &str) {
    if let Some(j) = current() {
        j.note("pkg_replace", json!({ "name": name, "version": version, "by": by })).unwrap_or_else(warn);
    }
}

/// A restore: which archive, which apps, and the safety snapshot (None when declined).
pub fn import(archive: &Path, apps: &[String], snapshot: Option<&Path>, fingerprint: Option<String>) {
    if let Some(j) = current() {
        j.note("import", json!({ "archive": archive, "apps": apps, "snapshot": snapshot, "snapshot_fingerprint": fingerprint })).unwrap_or_else(warn);
    }
}

/// A backup file written.
pub fn export(file: &Path) {
    if let Some(j) = current() {
        j.note("export", json!({ "file": file, "sha256": sha256_file(file) })).unwrap_or_else(warn);
    }
}

/// A backup uploaded to Proton Drive (undo moves it to Drive's trash).
pub fn remote_upload(remote: &str, sidecar: &str, size: u64, sha256: &str, folder: &str) {
    if let Some(j) = current() {
        j.note("remote_upload", json!({ "remote": remote, "sidecar": sidecar, "size": size, "sha256": sha256, "folder": folder })).unwrap_or_else(warn);
    }
}

/// The scheduled backup's user units written (enable) or removed (disable), with what was
/// there before.
pub fn user_timer(action: &str, service: &Path, timer: &Path, before_service: Option<&str>, before_timer: Option<&str>) {
    if let Some(j) = current() {
        j.note("user_timer", json!({ "action": action, "service": service, "timer": timer, "before_service": before_service, "before_timer": before_timer }))
            .unwrap_or_else(warn);
    }
}

/// The system upgrade: when, and the snapshots taken around it.
pub fn upgrade(start: &str, end: &str, snapshot: Value) {
    if let Some(j) = current() {
        j.note("upgrade", json!({ "start": start, "end": end, "snapshot": snapshot })).unwrap_or_else(warn);
    }
}

/// Mark a change as undone (the original lines are never rewritten).
pub fn mark_undone(dir: &Path, target_run: &str, target_seq: u64, user: &str, note: &str) -> io::Result<()> {
    let j = match current() {
        Some(j) => j,
        None => Arc::new(Journal::open(dir, &["undo"], user)?),
    };
    j.note("undo", json!({ "target_run": target_run, "target_seq": target_seq, "note": note }))
}

/// A fingerprint of a folder: names, sizes and times of everything in it, hashed. Changes
/// when the folder's content does, without reading every byte.
pub fn fingerprint(dir: &Path) -> Option<String> {
    if !dir.exists() {
        return None;
    }
    let mut lines: Vec<String> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .flatten()
        .map(|e| {
            let m = e.metadata().ok();
            let rel = e.path().strip_prefix(dir).unwrap_or(e.path()).display().to_string();
            format!("{rel}\t{}\t{}", m.as_ref().map_or(0, |m| m.len()), m.as_ref().map_or(0, |m| m.mtime()))
        })
        .collect();
    lines.sort();
    Some(sha256(lines.join("\n").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(dir: &Path) -> Vec<Entry> {
        load(dir).unwrap().entries
    }

    #[test]
    fn write_read_round_trip_and_runs() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("kast");
        let j = Journal::open(&dir, &["repo", "packages"], "p3ter").unwrap();
        let a = j.start("pacman_key", json!({ "fpr": "ABC", "was_present": false })).unwrap();
        j.finish("pacman_key", a, true, json!({})).unwrap();
        let b = j.start("service_enable", json!({ "unit": "valkey" })).unwrap();
        j.finish("service_enable", b, false, json!({ "error": "x" })).unwrap();
        j.note("pkg_install", json!({ "name": "zotero" })).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(dir.join("journal.jsonl")).unwrap().mode() & 0o777, 0o600);

        let runs = runs(&lines(&dir));
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!((r.id.as_str(), r.user.as_str(), r.steps.clone()), (j.run.as_str(), "p3ter", vec!["repo".to_string(), "packages".to_string()]));
        assert_eq!(r.version, env!("CARGO_PKG_VERSION"));
        let st: Vec<(String, Status)> = r.changes.iter().map(|c| (c.kind.clone(), c.status)).collect();
        assert_eq!(st, [("pacman_key".into(), Status::Done), ("service_enable".into(), Status::Failed), ("pkg_install".into(), Status::Done)]);
        assert_eq!(r.change(b).unwrap().data["error"], "x", "the outcome's data is merged in");
        assert_eq!(r.change(b).unwrap().data["unit"], "valkey");
    }

    #[test]
    fn a_crash_leaves_an_incomplete_change() {
        let t = tempfile::tempdir().unwrap();
        let j = Journal::open(t.path(), &["services"], "p3ter").unwrap();
        j.start("file_write", json!({ "path": "/etc/pam.d/sudo", "before": { "sha256": "aa" } })).unwrap();
        drop(j);
        // and a torn last line
        let mut f = OpenOptions::new().append(true).open(t.path().join("journal.jsonl")).unwrap();
        f.write_all(b"{\"v\":1,\"run\":\"x\",\"se").unwrap();
        let l = load(t.path()).unwrap();
        assert_eq!(l.problems.len(), 1);
        let r = &runs(&l.entries)[0];
        assert_eq!(r.changes[0].status, Status::Incomplete);
        assert_eq!(r.changes[0].data["before"]["sha256"], "aa");
    }

    #[test]
    fn unknown_fields_ignored_newer_versions_flagged() {
        let t = tempfile::tempdir().unwrap();
        let j = Journal::open(t.path(), &[], "p3ter").unwrap();
        let run = j.run.clone();
        drop(j);
        let mut f = OpenOptions::new().append(true).open(t.path().join("journal.jsonl")).unwrap();
        writeln!(f, r#"{{"v":1,"run":"{run}","seq":1,"kind":"export","status":"done","data":{{"file":"/x.zip"}},"future":true}}"#).unwrap();
        writeln!(f, r#"{{"v":2,"run":"{run}","seq":2,"kind":"teleport","status":"done","data":{{}}}}"#).unwrap();
        let r = &runs(&lines(t.path()))[0];
        assert_eq!(r.changes.len(), 2);
        assert!(!r.changes[0].newer && r.changes[1].newer);
    }

    #[test]
    fn unsafe_files_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let j = Journal::open(t.path(), &[], "p3ter").unwrap();
        let hash = j.store(b"pam").unwrap();
        drop(j);
        let journal = t.path().join("journal.jsonl");
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(t.path()).unwrap_err().contains("can be written by other users"));
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(t.path()).is_ok());
        let copy = copy_path(t.path(), &runs(&lines(t.path()))[0].id, &hash);
        assert_eq!(read_copy(t.path(), &runs(&lines(t.path()))[0].id, &hash).unwrap(), b"pam");
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o646)).unwrap();
        assert!(read_copy(t.path(), &runs(&lines(t.path()))[0].id, &hash).is_err());
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&copy, "tampered").unwrap();
        assert!(read_copy(t.path(), &runs(&lines(t.path()))[0].id, &hash).unwrap_err().contains("no longer matches"));
        // A symlink in place of the journal
        fs::remove_file(&journal).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &journal).unwrap();
        assert!(load(t.path()).unwrap_err().contains("symlink"));
    }

    #[test]
    fn copies_are_content_addressed() {
        let t = tempfile::tempdir().unwrap();
        let j = Journal::open(t.path(), &[], "p3ter").unwrap();
        let a = j.store(b"same").unwrap();
        let b = j.store(b"same").unwrap();
        let c = j.store(b"other").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, sha256(b"same"));
        assert_eq!(fs::read_dir(t.path().join("files").join(&j.run)).unwrap().count(), 2);
    }

    #[test]
    fn write_file_records_before_and_after() {
        let t = tempfile::tempdir().unwrap();
        with_root(t.path(), || {
            let conf = sys_path("/etc/pacman.conf");
            fs::create_dir_all(conf.parent().unwrap()).unwrap();
            fs::write(&conf, "[core]\n").unwrap();
            fs::set_permissions(&conf, fs::Permissions::from_mode(0o644)).unwrap();
            // Without a run: just written
            write_file(&sys_path("/etc/plain"), b"x", None).unwrap();
            begin_run(&["repo"], "p3ter").unwrap();
            write_file(&conf, b"[core]\n[archrepo]\n", None).unwrap();
            write_file(&sys_path("/usr/lib/hook.sh"), b"#!/bin/sh\n", Some(0o755)).unwrap();
            end_run();
            assert_eq!(fs::read_to_string(&conf).unwrap(), "[core]\n[archrepo]\n");
            assert_eq!(fs::metadata(sys_path("/usr/lib/hook.sh")).unwrap().mode() & 0o777, 0o755);
            let r = &runs(&lines(&state_dir()))[0];
            assert_eq!(r.changes.len(), 2);
            let w = &r.changes[0];
            assert_eq!((w.kind.as_str(), w.status), ("file_write", Status::Done));
            assert_eq!(w.data["before"]["mode"], 0o644);
            assert_eq!(w.data["after_sha256"], sha256(b"[core]\n[archrepo]\n"));
            let before = read_copy(&state_dir(), &r.id, w.data["before"]["sha256"].as_str().unwrap()).unwrap();
            assert_eq!(before, b"[core]\n");
            assert_eq!(r.changes[1].kind, "file_create");
            assert_eq!(r.changes[1].data["before"]["absent"], true);
        });
    }

    #[test]
    fn new_packages_are_the_difference() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let got = new_packages(&s(&["base", "zotero"]), &s(&["base", "zotero", "sine", "libdep"]), &["archrepo/sine", "zotero"], |n| {
            (format!("{n}-1"), "archrepo".into())
        });
        assert_eq!(
            got,
            [
                Installed { name: "sine".into(), version: "sine-1".into(), repo: "archrepo".into(), explicit: true },
                Installed { name: "libdep".into(), version: "libdep-1".into(), repo: "archrepo".into(), explicit: false },
            ]
        );
    }

    #[test]
    fn undo_lines_mark_their_change() {
        let t = tempfile::tempdir().unwrap();
        let j = Journal::open(t.path(), &[], "p3ter").unwrap();
        j.note("export", json!({ "file": "/x" })).unwrap();
        let run = j.run.clone();
        drop(j);
        mark_undone(t.path(), &run, 1, "p3ter", "deleted").unwrap();
        let rs = runs(&lines(t.path()));
        assert_eq!(rs.len(), 2, "the undo is a run of its own");
        assert_eq!(rs[0].changes[0].status, Status::Undone);
    }
}
