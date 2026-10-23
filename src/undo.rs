//! Undo: turn the journal of a run into the actions that revert it, show them, run them.
//!
//! `plan` is pure: it takes the run and a `View` of the system now (what's installed and
//! why, the current hash of each file) and returns the actions in the order they run:
//! package removals first (so archrepo packages go before [archrepo] does), then the
//! other changes newest first, then the optional reinstalls of swapped-out packages.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::journal::{self, Change, Run, Status, sys_path};

/// One thing undo can do.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// `pacman -Rns`: packages are removed together, in one transaction
    Remove(String),
    /// Reinstall a package a swap or an uninstall removed (off by default)
    Reinstall {
        name: String,
        version: String,
    },
    /// Put the file back as it was (`before` = the copy's hash, None = it didn't exist)
    RestoreFile {
        path: PathBuf,
        before: Option<String>,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    /// pacman.conf changed since kast wrote it: take only the [archrepo] block out
    RemoveRepoBlock(PathBuf),
    DeleteKey(String),
    HowdySet {
        key: String,
        value: String,
    },
    /// Give a system path its previous permission bits back
    ModeBack {
        path: PathBuf,
        mode: u32,
    },
    DisableUnit {
        unit: String,
        stop: bool,
    },
    RestoreImport {
        snapshot: PathBuf,
        apps: Vec<String>,
        fingerprint: Option<String>,
    },
    DeleteFile(PathBuf),
    /// Copy a home folder back from the data partition and drop the link kast made
    MoveBack {
        home: PathBuf,
        from: PathBuf,
        uid: u32,
        gid: u32,
        foreign: bool,
    },
    /// Give the partition's top folder its previous owner and mode back
    ChownBack {
        path: PathBuf,
        uid: u32,
        gid: u32,
        mode: u32,
    },
    /// Stop the mount (and automount) units and unmount; never lazy or forced
    Unmount {
        mountpoint: String,
        automount: bool,
    },
    /// Take kast's entry out of /etc/fstab: the saved copy when the file is unchanged since
    /// (`snapshot` = its hash), otherwise only kast's line and comment
    FstabRevert {
        path: PathBuf,
        line: String,
        comment: String,
        replaced: Vec<String>,
        snapshot: Option<String>,
    },
    /// Remove the folders kast made for the mount point (only while empty)
    RemoveDirs(Vec<PathBuf>),
    /// Move these Proton Drive paths to Drive's trash (as the user)
    TrashRemote(Vec<String>),
    /// Turn the scheduled backup off, putting back the units that were there before
    UndoTimer {
        before_service: Option<String>,
        before_timer: Option<String>,
    },
    /// Nothing to run: shown so the list is complete ("not undoable", "kept")
    None,
}

/// A line of the undo list.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub run: String,
    /// The journal changes this item reverts (marked undone when it succeeds)
    pub seqs: Vec<u64>,
    pub step: String,
    pub act: Act,
    pub label: String,
    /// Shown next to it: "kept: needed by X", "changed since kast", ...
    pub warning: Option<String>,
    /// Checked to start with
    pub default_on: bool,
    /// False: shown greyed out, never run
    pub undoable: bool,
}

impl Item {
    fn new(c: &Change, run: &str, act: Act, label: String) -> Self {
        Item { run: run.into(), seqs: vec![c.seq], step: c.step.clone(), act, label, warning: None, default_on: true, undoable: true }
    }

    fn not_undoable(c: &Change, run: &str, label: String, why: &str) -> Self {
        Item { warning: Some(why.into()), default_on: false, undoable: false, ..Item::new(c, run, Act::None, label) }
    }
}

/// A package as it is now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PkgNow {
    pub explicit: bool,
    pub required_by: Vec<String>,
    pub repo: String,
}

/// The system now, as far as undo needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub installed: HashMap<String, PkgNow>,
    /// Current hash of each file the journal names (None: it doesn't exist)
    pub files: HashMap<PathBuf, Option<String>>,
    /// Installed packages from archrepo (for the "stop receiving updates" warning)
    pub archrepo: Vec<String>,
    /// Current fingerprint of each import snapshot
    pub snapshots: HashMap<PathBuf, Option<String>>,
    /// Runs whose file copies are gone (pruned): their files can't be put back
    pub pruned: HashSet<String>,
    /// Home folders: where each is a symlink to now (None: it isn't one)
    pub links: HashMap<PathBuf, Option<PathBuf>>,
    /// Mount points with something mounted on them now (an idle automount counts)
    pub mounted: HashMap<String, bool>,
    /// Folders kast made: None = gone, Some(true) = empty, Some(false) = has content
    pub dirs: HashMap<PathBuf, Option<bool>>,
}

/// The write of /etc/fstab that went with a `fstab_entry`: the next one on the same path.
fn fstab_sibling<'a>(run: &'a Run, entry: &Change) -> Option<&'a Change> {
    run.changes
        .iter()
        .filter(|x| matches!(x.kind.as_str(), "file_write" | "file_create") && x.data["path"] == entry.data["path"] && x.seq > entry.seq)
        .min_by_key(|x| x.seq)
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// The actions that revert `run`, in the order they run.
pub fn plan(run: &Run, view: &View) -> Vec<Item> {
    let id = run.id.as_str();
    let live: Vec<&Change> = run.changes.iter().filter(|c| c.status != Status::Undone).collect();
    let mut removals: Vec<Item> = Vec::new();
    let mut rest: Vec<Item> = Vec::new();
    let mut reinstalls: Vec<Item> = Vec::new();
    let mut repo_touched = false;

    // Packages first: which can go, which must stay
    let candidates: HashSet<String> =
        live.iter().filter(|c| c.kind == "pkg_install" && !c.newer).map(|c| s(&c.data["name"])).filter(|n| view.installed.contains_key(n)).collect();
    // The write of /etc/fstab belongs to its `fstab_entry`: one item reverts both
    let absorbed: HashSet<u64> = live.iter().filter(|c| c.kind == "fstab_entry").filter_map(|c| fstab_sibling(run, c)).map(|f| f.seq).collect();
    for c in live.iter().rev() {
        if c.newer {
            rest.push(Item::not_undoable(c, id, format!("{} (from a newer kast)", c.kind), "made by a newer kast: update kast to undo it"));
            continue;
        }
        let incomplete = (c.status == Status::Incomplete).then(|| "incomplete: kast stopped in the middle of this change".to_string());
        match c.kind.as_str() {
            "pkg_install" => {
                let name = s(&c.data["name"]);
                let Some(now) = view.installed.get(&name) else { continue };
                let mut it = Item::new(c, id, Act::Remove(name.clone()), format!("Remove {name}"));
                let needed: Vec<&String> = now.required_by.iter().filter(|r| !candidates.contains(*r)).collect();
                if !needed.is_empty() {
                    it.act = Act::None;
                    it.undoable = false;
                    it.default_on = false;
                    it.label = format!("Keep {name}");
                    it.warning = Some(format!("kept: needed by {}", needed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                } else if now.explicit && c.data["reason"] == "dependency" {
                    it.act = Act::None;
                    it.undoable = false;
                    it.default_on = false;
                    it.label = format!("Keep {name}");
                    it.warning = Some("kept: marked as explicitly installed since".into());
                }
                removals.push(it);
            }
            "pkg_replace" | "pkg_remove" => {
                let name = s(&c.data["name"]);
                if view.installed.contains_key(&name) {
                    continue;
                }
                let version = s(&c.data["version"]);
                let mut it = Item::new(c, id, Act::Reinstall { name: name.clone(), version: version.clone() }, format!("Reinstall {name} {version}"));
                it.default_on = false;
                reinstalls.push(it);
            }
            "file_write" | "file_create" if absorbed.contains(&c.seq) => continue,
            "fstab_entry" => {
                let path = PathBuf::from(s(&c.data["path"]));
                let Some(f) = fstab_sibling(run, c) else { continue };
                let before = f.data["before"]["sha256"].as_str().map(String::from);
                let after = f.data["after_sha256"].as_str().map(String::from);
                let now = view.files.get(&path).cloned().flatten();
                if now == before {
                    continue; // already as it was (or the write never happened)
                }
                let line = s(&c.data["line"]);
                let replaced: Vec<String> = c.data["replaced"].as_array().map(|a| a.iter().map(s).collect()).unwrap_or_default();
                let mut label = format!("Remove from {}: {line}", path.display());
                for r in &replaced {
                    label += &format!(" (and put back: {r})");
                }
                if now.is_none() {
                    let mut it = Item::not_undoable(c, id, label, "/etc/fstab is gone");
                    it.seqs.push(f.seq);
                    rest.push(it);
                    continue;
                }
                let edited = now != after && f.status != Status::Incomplete;
                let snapshot = (!edited && !view.pruned.contains(id)).then(|| before.clone()).flatten();
                let act = Act::FstabRevert { path, line, comment: s(&c.data["comment"]), replaced, snapshot };
                let mut it = Item::new(c, id, act, label);
                it.seqs.push(f.seq);
                it.warning = if edited { Some("changed since kast wrote it: only kast's line is taken out, the rest stays".into()) } else { incomplete };
                rest.push(it);
            }
            "mount" => {
                let mp = s(&c.data["mountpoint"]);
                if c.status == Status::Failed || view.mounted.get(&mp) != Some(&true) {
                    continue;
                }
                rest.push(Item::new(c, id, Act::Unmount { mountpoint: mp.clone(), automount: c.data["mode"] == "automount" }, format!("Unmount {mp}")));
            }
            "chown_root" => {
                if c.status == Status::Failed {
                    continue;
                }
                let b = &c.data["before"];
                let path = PathBuf::from(s(&c.data["path"]));
                let (uid, gid, mode) =
                    (b["uid"].as_u64().unwrap_or(0) as u32, b["gid"].as_u64().unwrap_or(0) as u32, b["mode"].as_u64().unwrap_or(0o755) as u32);
                rest.push(Item::new(
                    c,
                    id,
                    Act::ChownBack { path: path.clone(), uid, gid, mode },
                    format!("Give {} back to {uid}:{gid} (mode {mode:04o})", path.display()),
                ));
            }
            "folder_move" => {
                if c.status == Status::Failed {
                    continue;
                }
                let home = PathBuf::from(s(&c.data["home"]));
                let new_path = PathBuf::from(s(&c.data["new_path"]));
                let name = s(&c.data["name"]);
                let label = format!("Copy {} back from {} ({})", home.display(), new_path.display(), human(c.data["size"].as_u64().unwrap_or(0)));
                let link = view.links.get(&home).cloned().flatten();
                if link.as_deref() == Some(new_path.as_path()) {
                    let act = Act::MoveBack {
                        home,
                        from: new_path,
                        uid: c.data["uid"].as_u64().unwrap_or(0) as u32,
                        gid: c.data["gid"].as_u64().unwrap_or(0) as u32,
                        foreign: c.data["foreign"] == true,
                    };
                    rest.push(Item::new(c, id, act, label));
                } else if c.status == Status::Done && c.data["symlink_created"] == true {
                    rest.push(Item::not_undoable(c, id, label, &format!("~/{name} is no longer the link kast made; left alone")));
                }
            }
            "dir_create" => {
                if c.status == Status::Failed {
                    continue;
                }
                let dirs: Vec<PathBuf> = c.data["created"].as_array().map(|a| a.iter().map(|d| PathBuf::from(s(d))).collect()).unwrap_or_default();
                let state = |d: &PathBuf| view.dirs.get(d).cloned().flatten();
                if dirs.iter().all(|d| state(d).is_none()) {
                    continue;
                }
                let label = format!("Remove the mount point {} (kast created it)", s(&c.data["path"]));
                if dirs.iter().any(|d| state(d) == Some(false)) {
                    rest.push(Item::not_undoable(c, id, label, "kept: it is not empty"));
                } else {
                    rest.push(Item::new(c, id, Act::RemoveDirs(dirs), label));
                }
            }
            "file_write" | "file_create" => {
                let path = PathBuf::from(s(&c.data["path"]));
                let before = &c.data["before"];
                let before_hash = before["sha256"].as_str().map(String::from);
                let after = c.data["after_sha256"].as_str().map(String::from);
                let now = view.files.get(&path).cloned().flatten();
                if now == before_hash {
                    continue; // already as it was (or the write never happened)
                }
                let is_conf = path.ends_with("pacman.conf");
                repo_touched |= is_conf;
                let shown = path.display().to_string();
                let edited = now.is_some() && after.is_some() && now != after && c.status != Status::Incomplete;
                let restore = Act::RestoreFile {
                    path: path.clone(),
                    before: before_hash.clone(),
                    mode: before["mode"].as_u64().unwrap_or(0o644) as u32,
                    uid: before["uid"].as_u64().unwrap_or(0) as u32,
                    gid: before["gid"].as_u64().unwrap_or(0) as u32,
                };
                if before_hash.is_some() && view.pruned.contains(id) && !(is_conf && edited) {
                    rest.push(Item::not_undoable(c, id, format!("Restore {shown}"), "not undoable (file copies pruned)"));
                    continue;
                }
                let mut it = match (&before_hash, is_conf && edited) {
                    (_, true) => Item::new(c, id, Act::RemoveRepoBlock(path.clone()), format!("Remove [archrepo] from {shown}")),
                    (None, false) => Item::new(c, id, restore, format!("Delete {shown} (kast created it)")),
                    (Some(_), false) => Item::new(c, id, restore, format!("Restore {shown}")),
                };
                if edited && !is_conf {
                    it.warning = Some("changed since kast wrote it".into());
                    it.default_on = false;
                } else if is_conf && edited {
                    it.warning = Some("changed since kast wrote it: only the [archrepo] block is taken out".into());
                }
                it.warning = it.warning.or(incomplete);
                rest.push(it);
            }
            "pacman_key" => {
                if c.data["was_present"] == true {
                    continue;
                }
                let fpr = s(&c.data["fpr"]);
                repo_touched = true;
                rest.push(Item::new(c, id, Act::DeleteKey(fpr.clone()), format!("Remove the archrepo signing key {}", &fpr[fpr.len().saturating_sub(8)..])));
            }
            "howdy_set" => match c.data["before"].as_str() {
                Some(v) => {
                    rest.push(Item::new(c, id, Act::HowdySet { key: s(&c.data["key"]), value: v.into() }, format!("howdy set {} {v}", s(&c.data["key"]))))
                }
                None => rest.push(Item::not_undoable(c, id, format!("howdy set {}", s(&c.data["key"])), "no previous value was set")),
            },
            "mode_change" => {
                if c.status == Status::Failed {
                    continue;
                }
                let path = PathBuf::from(s(&c.data["path"]));
                let mode = c.data["before"].as_u64().unwrap_or(0o755) as u32;
                rest.push(Item::new(c, id, Act::ModeBack { path: path.clone(), mode }, format!("Give {} its mode {mode:04o} back", path.display())));
            }
            "service_enable" => {
                if c.data["was_enabled"] == true {
                    continue;
                }
                let unit = s(&c.data["unit"]);
                let stop = c.data["was_active"] != true;
                rest.push(Item::new(c, id, Act::DisableUnit { unit: unit.clone(), stop }, format!("Disable {}{unit}", if stop { "and stop " } else { "" })));
            }
            "import" => {
                let archive = s(&c.data["archive"]);
                let name = Path::new(&archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(archive);
                let apps: Vec<String> = c.data["apps"].as_array().map(|a| a.iter().map(s).collect()).unwrap_or_default();
                match c.data["snapshot"].as_str() {
                    None => rest.push(Item::not_undoable(c, id, format!("Restore of {name}"), "not undoable (no snapshot was taken)")),
                    Some(snap) => {
                        let snapshot = PathBuf::from(snap);
                        let fingerprint = c.data["snapshot_fingerprint"].as_str().map(String::from);
                        let mut it = Item::new(
                            c,
                            id,
                            Act::RestoreImport { snapshot: snapshot.clone(), apps: apps.clone(), fingerprint: fingerprint.clone() },
                            format!("Put back the data the restore of {name} replaced ({})", apps.join(", ")),
                        );
                        match view.snapshots.get(&snapshot) {
                            Some(None) => {
                                it = Item::not_undoable(c, id, it.label, "not undoable: its snapshot is gone");
                            }
                            Some(now) if now != &fingerprint => it.warning = Some("the snapshot changed since; the face model part is skipped".into()),
                            _ => {}
                        }
                        rest.push(it);
                    }
                }
            }
            "export" => {
                let file = PathBuf::from(s(&c.data["file"]));
                let mut it = Item::new(c, id, Act::DeleteFile(file.clone()), format!("Delete the backup {}", file.display()));
                it.default_on = false;
                rest.push(it);
            }
            "remote_upload" => {
                let remote = s(&c.data["remote"]);
                let paths: Vec<String> = [remote.clone(), s(&c.data["sidecar"])].into_iter().filter(|p| !p.is_empty()).collect();
                let mut it = Item::new(c, id, Act::TrashRemote(paths), format!("Move the uploaded backup {remote} to Proton Drive's trash"));
                it.default_on = false;
                rest.push(it);
            }
            "user_timer" if c.data["action"] == "enable" => {
                let opt = |k: &str| c.data[k].as_str().map(String::from);
                rest.push(Item::new(
                    c,
                    id,
                    Act::UndoTimer { before_service: opt("before_service"), before_timer: opt("before_timer") },
                    "Turn the scheduled backup off and remove its timer".into(),
                ));
            }
            "user_timer" => rest.push(Item::not_undoable(c, id, "Scheduled backup turned off".into(), "turn it on again from Scheduled backup (s)")),
            "upgrade" => rest.push(Item::not_undoable(c, id, "System upgrade".into(), &upgrade_hint(&c.data["snapshot"]))),
            _ => rest.push(Item::not_undoable(c, id, c.kind.clone(), "unknown change")),
        }
    }
    // user-dirs.dirs goes back once the folders are back; unmounting with folders still
    // linked to the partition leaves them dangling
    let moves = |i: &Item| matches!(i.act, Act::MoveBack { .. }) || (!i.undoable && i.label.starts_with("Copy "));
    if let (Some(ui), Some(last)) =
        (rest.iter().position(|i| matches!(&i.act, Act::RestoreFile { path, .. } if path.ends_with(".config/user-dirs.dirs"))), rest.iter().rposition(moves))
        && ui < last
    {
        let it = rest.remove(ui);
        rest.insert(last, it);
    }
    let stuck: Vec<String> = rest.iter().filter(|i| !i.undoable && i.label.starts_with("Copy ")).map(|i| i.label.clone()).collect();
    if !stuck.is_empty() {
        for i in rest.iter_mut().filter(|i| matches!(i.act, Act::Unmount { .. })) {
            i.warning = Some("some home folders still link to the partition and will point at nothing".into());
        }
    }
    // Warn when [archrepo] goes while packages from it stay
    if repo_touched {
        let removing: HashSet<String> = removals.iter().filter_map(|i| if let Act::Remove(n) = &i.act { Some(n.clone()) } else { None }).collect();
        let stays: Vec<&String> = view.archrepo.iter().filter(|p| !removing.contains(*p)).collect();
        if !stays.is_empty() {
            if let Some(it) = rest.iter_mut().find(|i| matches!(i.act, Act::RestoreFile { .. } | Act::RemoveRepoBlock(_)) && i.label.contains("pacman.conf")) {
                let list = stays.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                let w = format!("still installed from archrepo, they stop receiving updates: {list}");
                it.warning = Some(it.warning.take().map_or(w.clone(), |o| format!("{o}; {w}")));
            }
            for p in stays {
                removals.push(Item {
                    run: id.into(),
                    seqs: Vec::new(),
                    step: "repo".into(),
                    act: Act::Remove(p.clone()),
                    label: format!("Also remove {p} (from archrepo)"),
                    warning: None,
                    default_on: false,
                    undoable: true,
                });
            }
        }
    }
    removals.into_iter().chain(rest).chain(reinstalls).collect()
}

/// How to roll the upgrade back, when a snapshot was taken.
pub fn upgrade_hint(snapshot: &Value) -> String {
    match crate::snapshot::rollback_hint(snapshot) {
        Some(h) => format!("not undoable by kast. {h}"),
        None => "System upgrades can't be undone by kast".into(),
    }
}

// --- The system now -----------------------------------------------------------------------

fn pacman_qi(name: &str) -> Option<PkgNow> {
    let out = std::process::Command::new("pacman").args(["-Qi", name]).env("LC_ALL", "C").stderr(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(parse_qi(&text))
}

/// `pacman -Qi` (C locale): install reason and what requires the package.
pub fn parse_qi(text: &str) -> PkgNow {
    let field =
        |key: &str| text.lines().filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let required = field("Required By");
    PkgNow {
        explicit: field("Install Reason").starts_with("Explicitly"),
        required_by: if required == "None" { Vec::new() } else { required.split_whitespace().map(String::from).collect() },
        repo: String::new(),
    }
}

/// Look at the system for what the runs' changes touch.
pub fn gather(runs: &[&Run]) -> View {
    let mut v = View::default();
    let files = journal::state_dir().join("files");
    v.pruned = runs.iter().filter(|r| !files.join(&r.id).is_dir()).map(|r| r.id.clone()).collect();
    for c in runs.iter().flat_map(|r| &r.changes) {
        match c.kind.as_str() {
            "pkg_install" | "pkg_replace" | "pkg_remove" => {
                let n = s(&c.data["name"]);
                if let Some(p) = pacman_qi(&n) {
                    v.installed.insert(n, p);
                }
            }
            "file_write" | "file_create" => {
                let p = PathBuf::from(s(&c.data["path"]));
                let h = journal::sha256_file(&p);
                v.files.insert(p, h);
            }
            "fstab_entry" => {
                let p = PathBuf::from(s(&c.data["path"]));
                let h = journal::sha256_file(&p);
                v.files.insert(p, h);
            }
            "mount" => {
                let mp = s(&c.data["mountpoint"]);
                let on = crate::partition::undo::anything_mounted(&crate::partition::SysRunner, &mp);
                v.mounted.insert(mp, on);
            }
            "folder_move" => {
                let home = PathBuf::from(s(&c.data["home"]));
                let link = fs::read_link(&home).ok();
                v.links.insert(home, link);
            }
            "dir_create" => {
                for d in c.data["created"].as_array().into_iter().flatten().map(|d| PathBuf::from(s(d))) {
                    let state = match fs::read_dir(&d) {
                        Ok(mut it) => Some(it.next().is_none()),
                        Err(_) => None,
                    };
                    v.dirs.insert(d, state);
                }
            }
            "import" => {
                if let Some(snap) = c.data["snapshot"].as_str() {
                    v.snapshots.insert(PathBuf::from(snap), journal::fingerprint(Path::new(snap)));
                }
            }
            _ => {}
        }
    }
    if let Some(list) = crate::ctx::output("pacman", &["-Sl", "archrepo"]) {
        v.archrepo = list.lines().filter(|l| l.ends_with("[installed]")).filter_map(|l| l.split_whitespace().nth(1).map(String::from)).collect();
    }
    v
}

// --- PAM sanity check ---------------------------------------------------------------------

const PAM_TYPES: [&str; 4] = ["auth", "account", "password", "session"];

/// One PAM line: (type, control, module), or None for blank lines and comments.
pub(crate) fn pam_line(line: &str) -> Result<Option<(String, String, String)>, String> {
    let l = line.split('#').next().unwrap_or("").trim();
    if l.is_empty() {
        return Ok(None);
    }
    if let Some(f) = l.strip_prefix("@include") {
        return Ok(Some(("@include".into(), "include".into(), f.trim().into())));
    }
    let (ty, rest) = l.split_once(char::is_whitespace).ok_or_else(|| format!("can't read {l:?}"))?;
    let bare = ty.trim_start_matches('-');
    if !PAM_TYPES.contains(&bare) {
        return Err(format!("unknown type {ty:?} in {l:?}"));
    }
    let rest = rest.trim_start();
    let (control, rest) = if rest.starts_with('[') {
        let end = rest.find(']').ok_or_else(|| format!("unclosed [ in {l:?}"))?;
        (rest[..=end].to_string(), rest[end + 1..].trim_start())
    } else {
        let (c, r) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        (c.to_string(), r.trim_start())
    };
    let module = rest.split_whitespace().next().ok_or_else(|| format!("no module in {l:?}"))?;
    Ok(Some((ty.to_string(), control, module.to_string())))
}

/// The PAM files under `pam_dir` parse, every module and include exists, and sudo still has
/// an auth stack. `files`: the ones to check (all of /etc/pam.d when empty).
pub fn pam_check(pam_dir: &Path, module_dirs: &[PathBuf], files: &[String]) -> Result<(), String> {
    let names: Vec<String> = if files.is_empty() {
        fs::read_dir(pam_dir)
            .map(|rd| rd.flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    } else {
        files.to_vec()
    };
    for name in names.iter().chain(std::iter::once(&"sudo".to_string())) {
        let p = pam_dir.join(name);
        let Ok(text) = fs::read_to_string(&p) else {
            if name == "sudo" {
                return Err(format!("{} is missing", p.display()));
            }
            continue;
        };
        for line in text.lines() {
            let Some((ty, control, module)) = pam_line(line).map_err(|e| format!("{}: {e}", p.display()))? else { continue };
            if matches!(control.as_str(), "include" | "substack") || ty == "@include" {
                if !pam_dir.join(&module).is_file() {
                    return Err(format!("{}: includes {module}, which doesn't exist", p.display()));
                }
            } else if !ty.starts_with('-') {
                let found = if module.starts_with('/') { sys_path(&module).is_file() } else { module_dirs.iter().any(|d| d.join(&module).is_file()) };
                if !found {
                    return Err(format!("{}: module {module} doesn't exist", p.display()));
                }
            }
        }
    }
    if !has_auth(pam_dir, "sudo", 0) {
        return Err(format!("{} has no auth stack left", pam_dir.join("sudo").display()));
    }
    Ok(())
}

fn has_auth(pam_dir: &Path, name: &str, depth: u8) -> bool {
    let Ok(text) = fs::read_to_string(pam_dir.join(name)) else { return false };
    depth < 8
        && text.lines().filter_map(|l| pam_line(l).ok().flatten()).any(|(ty, control, module)| {
            (ty.trim_start_matches('-') == "auth" && !matches!(control.as_str(), "include" | "substack"))
                || ((ty == "auth" && matches!(control.as_str(), "include" | "substack")) || ty == "@include") && has_auth(pam_dir, &module, depth + 1)
        })
}

pub fn module_dirs() -> Vec<PathBuf> {
    ["/usr/lib/security", "/lib/security", "/usr/lib64/security"].iter().map(sys_path).collect()
}

// --- Running it ---------------------------------------------------------------------------

/// What one undo did, for the summary.
#[derive(Debug, Default)]
pub struct Outcome {
    pub done: Vec<String>,
    pub failed: Vec<String>,
    /// Things to tell the user (e.g. "the copy on the partition can be removed by hand")
    pub notes: Vec<String>,
}

fn ok_cmd(prog: &str, args: &[&str]) -> bool {
    crate::ctx::ok_logged(prog, args)
}

/// Take the [archrepo] block (header to the next section) out of pacman.conf content.
pub fn without_repo_block(conf: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in conf.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            skip = t == "[archrepo]";
        }
        if !skip {
            out.push_str(line);
        }
    }
    out.trim_end_matches('\n').to_string() + "\n"
}

/// Run the chosen items (in plan order). PAM files are checked after they're restored; if
/// the check fails they go back to how they were before the undo and the undo stops.
pub fn execute(items: &[Item], dir: &Path, user: &str, report: &mut dyn FnMut(usize, &str)) -> Result<Outcome> {
    execute_with(items, dir, user, &crate::partition::SysRunner, report)
}

/// `execute`, with the runner for mount, umount, findmnt and systemctl.
pub fn execute_with(items: &[Item], dir: &Path, user: &str, rn: &dyn crate::partition::Runner, report: &mut dyn FnMut(usize, &str)) -> Result<Outcome> {
    let mut out = Outcome::default();
    // A busy partition stops the undo before anything is touched
    for it in items {
        if let Act::Unmount { mountpoint, .. } = &it.act {
            crate::partition::undo::check_not_busy(rn, mountpoint)?;
        }
    }
    let mark = |it: &Item| {
        for s in &it.seqs {
            let _ = journal::mark_undone(dir, &it.run, *s, user, &it.label);
        }
    };
    // 1. Packages, in one transaction
    let removals: Vec<&Item> = items.iter().filter(|i| matches!(i.act, Act::Remove(_))).collect();
    if !removals.is_empty() {
        let names: Vec<&str> = removals.iter().filter_map(|i| if let Act::Remove(n) = &i.act { Some(n.as_str()) } else { None }).collect();
        report(0, &format!("Removing {}", names.join(" ")));
        let mut args = vec!["-Rns", "--noconfirm"];
        args.extend(&names);
        let ok = ok_cmd("pacman", &args);
        for it in removals {
            let Act::Remove(n) = &it.act else { continue };
            if !crate::ctx::ok("pacman", &["-Qq", n]) {
                mark(it);
                out.done.push(it.label.clone());
            } else {
                out.failed.push(format!("{} (pacman -Rns failed{})", it.label, if ok { "" } else { "; see the log" }));
            }
        }
    }
    // 2. Everything else. PAM files are checked right after the last one is restored (and
    // marked undone only once the check passes); a failed check stops the undo there
    let pam = |it: &Item| matches!(&it.act, Act::RestoreFile { path, .. } if path.starts_with(sys_path("/etc/pam.d")));
    let last_pam = items.iter().rposition(pam);
    let mut pam_before: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut pam_items: Vec<&Item> = Vec::new();
    for (i, it) in items.iter().enumerate().filter(|(_, i)| !matches!(i.act, Act::Remove(_))) {
        report(i, &it.label);
        let r: Result<()> = match &it.act {
            Act::RestoreFile { path, before, mode, uid, gid } => {
                if path.starts_with(sys_path("/etc/pam.d")) {
                    pam_before.push((path.clone(), fs::read(path).ok()));
                }
                restore_file(dir, &it.run, path, before.as_deref(), *mode, *uid, *gid)
            }
            Act::RemoveRepoBlock(path) => {
                let conf = fs::read_to_string(path)?;
                crate::ctx::write_atomic(path, without_repo_block(&conf).as_bytes()).map_err(Into::into)
            }
            Act::DeleteKey(fpr) => {
                if ok_cmd("pacman-key", &["--delete", fpr]) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("pacman-key --delete failed"))
                }
            }
            Act::HowdySet { key, value } => {
                if ok_cmd("howdy", &["set", key, value]) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("howdy set failed"))
                }
            }
            Act::ModeBack { path, mode } => {
                fs::set_permissions(path, fs::Permissions::from_mode(*mode)).map_err(|e| anyhow::anyhow!("restoring the mode of {}: {e}", path.display()))
            }
            Act::DisableUnit { unit, stop } => {
                let args: &[&str] = if *stop { &["disable", "--now", unit] } else { &["disable", unit] };
                if ok_cmd("systemctl", args) { Ok(()) } else { Err(anyhow::anyhow!("systemctl disable failed")) }
            }
            Act::RestoreImport { snapshot, apps, fingerprint } => crate::vault::import::undo_restore(snapshot, apps, fingerprint.as_deref()),
            Act::DeleteFile(p) => fs::remove_file(p).map_err(Into::into),
            Act::MoveBack { home, from, uid, gid, foreign } => {
                crate::partition::undo::move_back(rn, home, from, *uid, *gid, *foreign, &|p| crate::partition::apply::avail(p)).map(|n| out.notes.push(n))
            }
            Act::ChownBack { path, uid, gid, mode } => crate::partition::undo::chown_back(rn, path, &path.to_string_lossy(), *uid, *gid, *mode).map(|n| {
                if !n.is_empty() {
                    out.notes.push(n);
                }
            }),
            Act::Unmount { mountpoint, automount } => crate::partition::undo::unmount_partition(rn, mountpoint, *automount),
            Act::FstabRevert { path, line, comment, replaced, snapshot } => {
                let snap = match snapshot {
                    Some(h) => journal::read_copy(dir, &it.run, h).map(Some).map_err(|e| anyhow::anyhow!(e)),
                    None => Ok(None),
                };
                snap.and_then(|snapshot| {
                    crate::partition::undo::fstab_revert(rn, &crate::partition::undo::FstabRevert { path, line, comment, replaced, snapshot })
                })
            }
            Act::RemoveDirs(dirs) => {
                let kept = crate::partition::undo::remove_dirs(dirs);
                if !kept.is_empty() {
                    out.notes.push(format!("kept {}: not empty", kept.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
                }
                Ok(())
            }
            Act::TrashRemote(paths) => trash_remote(paths),
            Act::UndoTimer { before_service, before_timer } => crate::schedule::undo_enable(before_service.as_deref(), before_timer.as_deref()),
            Act::Reinstall { name, .. } => {
                if ok_cmd("pacman", &["-S", "--needed", "--noconfirm", name]) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("pacman -S failed (it may only be in the AUR: yay -S {name})"))
                }
            }
            Act::Remove(_) | Act::None => continue,
        };
        match r {
            Ok(()) if pam(it) => pam_items.push(it),
            Ok(()) => {
                mark(it);
                out.done.push(it.label.clone());
            }
            Err(e) if matches!(it.act, Act::MoveBack { .. } | Act::Unmount { .. } | Act::FstabRevert { .. }) => {
                // The data partition steps depend on each other: stop here, nothing half-done
                let done = if out.done.is_empty() { "nothing".to_string() } else { out.done.join("; ") };
                bail!("{}: {e}\nThe undo stopped there. Already undone: {done}.", it.label);
            }
            Err(e) => out.failed.push(format!("{}: {e}", it.label)),
        }
        // Never leave sudo broken
        if Some(i) == last_pam && !pam_before.is_empty() {
            let names: Vec<String> = pam_before.iter().filter_map(|(p, _)| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
            if let Err(e) = pam_check(&sys_path("/etc/pam.d"), &module_dirs(), &names) {
                for (p, data) in &pam_before {
                    let _ = match data {
                        Some(d) => crate::ctx::write_atomic(p, d),
                        None => fs::remove_file(p),
                    };
                }
                let done = if out.done.is_empty() { "nothing".to_string() } else { out.done.join("; ") };
                bail!(
                    "The restored PAM files failed the sanity check ({e}); they were put back as they were before this undo, so sudo works as it did. \
                     The undo stopped there. Already undone: {done}."
                );
            }
            for it in pam_items.drain(..) {
                mark(it);
                out.done.push(it.label.clone());
            }
        }
    }
    Ok(out)
}

/// A short diff for a file changed since kast wrote it: lines only in kast's before-copy
/// (-) and lines only in the file now (+).
pub fn diff_lines(dir: &Path, it: &Item, max: usize) -> Vec<String> {
    let Act::RestoreFile { path, before: Some(hash), .. } = &it.act else { return Vec::new() };
    let (Ok(old), Ok(now)) = (journal::read_copy(dir, &it.run, hash), fs::read(path)) else { return Vec::new() };
    let (old, now) = (String::from_utf8_lossy(&old).into_owned(), String::from_utf8_lossy(&now).into_owned());
    let olds: HashSet<&str> = old.lines().collect();
    let nows: HashSet<&str> = now.lines().collect();
    let mut out: Vec<String> = old.lines().filter(|l| !nows.contains(l)).map(|l| format!("- {l}")).collect();
    out.extend(now.lines().filter(|l| !olds.contains(l)).map(|l| format!("+ {l}")));
    if out.len() > max {
        let more = out.len() - max;
        out.truncate(max);
        out.push(format!("… {more} more"));
    }
    out
}

fn restore_file(dir: &Path, run: &str, path: &Path, before: Option<&str>, mode: u32, uid: u32, gid: u32) -> Result<()> {
    match before {
        None => match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
        Some(hash) => {
            let data = journal::read_copy(dir, run, hash).map_err(|e| anyhow::anyhow!(e))?;
            crate::ctx::write_atomic(path, &data)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            crate::ctx::chown_path(path, uid, gid);
            Ok(())
        }
    }
}

// --- Listing and pruning ------------------------------------------------------------------

/// The runs worth listing (not the bookkeeping runs of undo itself), newest first.
pub fn listed(runs: &[Run]) -> Vec<&Run> {
    runs.iter().rev().filter(|r| !r.changes.is_empty() && r.steps != ["undo"]).collect()
}

/// "N changes, M undoable"
pub fn counts(run: &Run, items: &[Item]) -> (usize, usize) {
    (run.changes.len(), items.iter().filter(|i| i.undoable && !matches!(i.act, Act::None)).count())
}

pub fn run_json(run: &Run) -> Value {
    json!({
        "run": run.id,
        "time": run.ts,
        "version": run.version,
        "host": run.host,
        "user": run.user,
        "steps": run.steps,
        "changes": run.changes.iter().map(|c| json!({
            "seq": c.seq,
            "kind": c.kind,
            "step": c.step,
            "status": c.status,
            "newer": c.newer,
            "data": c.data,
        })).collect::<Vec<_>>(),
    })
}

/// Bytes of file copies kast keeps.
pub fn stored_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir.join("files")).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

/// Runs whose copies `--prune` deletes: beyond the newest `keep`, or with nothing left to undo.
pub fn prunable(runs: &[Run], keep: usize) -> Vec<String> {
    let listed = listed(runs);
    listed.iter().enumerate().filter(|(i, r)| *i >= keep || r.changes.iter().all(|c| c.status == Status::Undone)).map(|(_, r)| r.id.clone()).collect()
}

pub fn prune(dir: &Path, ids: &[String]) -> u64 {
    let mut freed = 0;
    for id in ids {
        let p = dir.join("files").join(id);
        freed += walkdir::WalkDir::new(&p).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum::<u64>();
        let _ = fs::remove_dir_all(&p);
    }
    freed
}

/// How many runs to keep (`/var/lib/kast/settings.json`, `keep_runs`; 10 by default).
pub fn keep_runs(dir: &Path) -> usize {
    let p = dir.join("settings.json");
    if journal::check_secure(&p).is_err() {
        return 10;
    }
    fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["keep_runs"].as_u64()).map_or(10, |n| n as usize)
}

/// Move uploaded files to Proton Drive's trash, as the user. Ones already gone are fine.
fn trash_remote(paths: &[String]) -> Result<()> {
    let cli = crate::drive::installed_cli().map_err(|p| anyhow::anyhow!("{p}"))?;
    for p in paths {
        match cli.trash(std::slice::from_ref(p)) {
            Ok(()) | Err(crate::proton::Problem::NotFound(_)) => {}
            Err(e) => anyhow::bail!("{e}"),
        }
    }
    Ok(())
}

// --- The command line ---------------------------------------------------------------------

/// One line for a change: "pkg_install zotero 1.0", "file_write /etc/pam.d/sudo".
pub fn describe(c: &Change) -> String {
    let d = &c.data;
    let what = match c.kind.as_str() {
        "pkg_install" | "pkg_replace" | "pkg_remove" => format!("{} {}", s(&d["name"]), s(&d["version"])),
        "file_write" | "file_create" => s(&d["path"]),
        "fstab_entry" => s(&d["line"]),
        "mount" => format!("{} ({})", s(&d["mountpoint"]), s(&d["mode"])),
        "chown_root" => s(&d["path"]),
        "folder_move" => format!("{} -> {}", s(&d["home"]), s(&d["new_path"])),
        "dir_create" => s(&d["path"]),
        "howdy_set" => format!("{} = {}", s(&d["key"]), s(&d["after"])),
        "mode_change" => format!("{} {:04o} -> {:04o}", s(&d["path"]), d["before"].as_u64().unwrap_or(0), d["after"].as_u64().unwrap_or(0)),
        "service_enable" => s(&d["unit"]),
        "pacman_key" => s(&d["fpr"]),
        "import" => format!("{} ({})", s(&d["archive"]), d["apps"].as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join(", ")).unwrap_or_default()),
        "export" => s(&d["file"]),
        "remote_upload" => s(&d["remote"]),
        "user_timer" => format!("{} {}", s(&d["action"]), s(&d["timer"])),
        _ => String::new(),
    };
    format!("{} {what}", c.kind)
}

pub fn status_word(c: &Change) -> &'static str {
    match (c.newer, c.status) {
        (true, _) => "newer kast",
        (_, Status::Done) => "done",
        (_, Status::Failed) => "failed",
        (_, Status::Incomplete) => "incomplete",
        (_, Status::Undone) => "undone",
    }
}

pub fn human(bytes: u64) -> String {
    let mut v = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if v < 1024.0 || unit == "GiB" {
            return if unit == "B" { format!("{bytes} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

pub fn load_runs(dir: &Path) -> Result<(Vec<Run>, Vec<String>)> {
    let l = journal::load(dir).map_err(|e| anyhow::anyhow!("Can't read the journal: {e}"))?;
    Ok((journal::runs(&l.entries), l.problems))
}

/// What `kast undo` was asked.
#[derive(Debug, Default)]
pub struct UndoArgs<'a> {
    pub list: bool,
    pub json: bool,
    pub run: Option<&'a str>,
    pub last: bool,
    pub step: Option<&'a str>,
    pub dry_run: bool,
    pub force: bool,
    pub prune: bool,
    pub keep: Option<usize>,
}

/// The items `kast undo` runs: the default ones, and with --force files changed since.
pub fn cli_choice(items: &[Item], force: bool) -> Vec<Item> {
    items.iter().filter(|i| i.undoable && (i.default_on || force && i.warning.as_deref().is_some_and(|w| w.contains("changed since")))).cloned().collect()
}

/// `kast undo …`
pub fn cmd_undo(a: &UndoArgs) -> Result<()> {
    let dir = journal::state_dir();
    let (runs, problems) = load_runs(&dir)?;
    for p in &problems {
        eprintln!("warning: {p}");
    }
    let shown = listed(&runs);
    if a.prune {
        let keep = a.keep.unwrap_or_else(|| keep_runs(&dir));
        let ids = prunable(&runs, keep);
        if ids.is_empty() {
            println!("Nothing to prune (keeping the last {keep} runs).");
            return Ok(());
        }
        let freed = if a.dry_run { 0 } else { prune(&dir, &ids) };
        let verb = if a.dry_run { "Would delete" } else { "Deleted" };
        let tail = if a.dry_run { String::new() } else { format!(" ({} freed)", human(freed)) };
        println!("{verb} the file copies of {} run(s): {}{tail}", ids.len(), ids.join(", "));
        return Ok(());
    }
    if a.list || (a.run.is_none() && !a.last) {
        if a.json {
            let out = json!({ "dir": dir, "stored_bytes": stored_size(&dir), "runs": shown.iter().map(|r| run_json(r)).collect::<Vec<_>>() });
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(());
        }
        if shown.is_empty() {
            println!("No changes recorded yet in {}.", dir.display());
            return Ok(());
        }
        println!("Changes recorded by kast in {} ({} of file copies), newest first:\n", dir.display(), human(stored_size(&dir)));
        let view = gather(&shown);
        for r in &shown {
            let items = plan(r, &view);
            let (n, m) = counts(r, &items);
            println!(
                "{}  {}  kast {}  {}  ({n} changes, {m} undoable)",
                r.id,
                r.ts.get(..16).unwrap_or(&r.ts).replace('T', " "),
                r.version,
                r.steps.join(", ")
            );
            for c in &r.changes {
                println!("    #{:<3} {:<9} {:<10} {}", c.seq, c.step, status_word(c), describe(c));
            }
        }
        println!("\nUndo one with: sudo kast undo <run> [--step STEP] [--dry-run]");
        return Ok(());
    }
    let target = if a.last {
        shown.iter().find(|r| r.changes.iter().any(|c| c.status != Status::Undone)).copied()
    } else {
        let id = a.run.unwrap_or_default();
        let found: Vec<&&Run> = shown.iter().filter(|r| r.id.starts_with(id)).collect();
        if found.len() > 1 {
            bail!("{id} matches {} runs; give more of it", found.len());
        }
        found.first().map(|r| **r)
    };
    let Some(target) = target else { bail!("No such run (see kast undo --list)") };
    let view = gather(&[target]);
    let items: Vec<Item> = plan(target, &view).into_iter().filter(|i| a.step.is_none_or(|s| i.step == s)).collect();
    let chosen = cli_choice(&items, a.force);
    println!("Undo of run {} (kast {}, {}):", target.id, target.version, target.steps.join(", "));
    for it in &items {
        let mark = if !it.undoable {
            "-"
        } else if chosen.contains(it) {
            "✔"
        } else {
            "·"
        };
        let warn = it.warning.as_ref().map_or(String::new(), |w| format!("  ({w})"));
        println!("  {mark} {}{warn}", it.label);
        if it.warning.as_deref().is_some_and(|w| w.contains("changed since")) {
            for l in diff_lines(&dir, it, 6) {
                println!("        {l}");
            }
            if !a.force && !matches!(it.act, Act::RemoveRepoBlock(_)) {
                println!("        skipped: changed since kast wrote it (--force restores it anyway)");
            }
        }
    }
    println!("  (✔ will be undone, · left as it is, - can't be undone)");
    if chosen.is_empty() {
        println!("Nothing to undo.");
        return Ok(());
    }
    if a.dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(());
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin())
        && !crate::report::ask(&crate::report::Prompt::yes_no(format!("Undo these {} change(s)?", chosen.len()), false))
    {
        println!("Nothing was changed.");
        return Ok(());
    }
    let user = crate::ctx::ctx().target_user.clone();
    let out = journal::recorded(&["undo"], false, || execute(&chosen, &dir, &user, &mut |_, l| println!("==> {l}")))?;
    println!("Undone: {}", out.done.len());
    for n in &out.notes {
        println!("  note: {n}");
    }
    if !out.failed.is_empty() {
        for f in &out.failed {
            println!("  ✘ {f}");
        }
        bail!("{} change(s) could not be undone", out.failed.len());
    }
    Ok(())
}

// --- Uninstall apps -----------------------------------------------------------------------

/// A package of the list that is installed now.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledApp {
    pub name: String,
    pub group: String,
    /// The date of the run that installed it; None: installed before kast, or outside it
    pub by_kast: Option<String>,
}

/// The list's packages that are installed, and which of them kast installed.
pub fn installed_apps(list: &crate::pkglist::Parsed, installed: &HashSet<String>, runs: &[Run]) -> Vec<InstalledApp> {
    let mut out = Vec::new();
    for g in &list.groups {
        for p in &g.packages {
            let name = p.bare().to_string();
            if !installed.contains(&name) {
                continue;
            }
            let by_kast = runs
                .iter()
                .rev()
                .find(|r| r.changes.iter().any(|c| c.kind == "pkg_install" && c.status == Status::Done && c.data["name"] == name.as_str()))
                .map(|r| r.ts.get(..10).unwrap_or(&r.ts).to_string());
            out.push(InstalledApp { name, group: g.name.clone(), by_kast });
        }
    }
    out
}

/// What removing `names` does: removals, or "kept: needed by X".
pub fn uninstall_plan(names: &[String], view: &View) -> Vec<Item> {
    let set: HashSet<&String> = names.iter().collect();
    names
        .iter()
        .filter_map(|n| {
            let now = view.installed.get(n)?;
            let needed: Vec<&String> = now.required_by.iter().filter(|r| !set.contains(r)).collect();
            let mut it = Item {
                run: String::new(),
                seqs: Vec::new(),
                step: "uninstall".into(),
                act: Act::Remove(n.clone()),
                label: format!("Remove {n}"),
                warning: None,
                default_on: true,
                undoable: true,
            };
            if !needed.is_empty() {
                it.act = Act::None;
                it.undoable = false;
                it.label = format!("Keep {n}");
                it.warning = Some(format!("kept: needed by {}", needed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            }
            Some(it)
        })
        .collect()
}

/// The system view for an uninstall of `names`.
pub fn uninstall_view(names: &[String]) -> View {
    let mut view = View::default();
    for n in names {
        if let Some(p) = pacman_qi(n) {
            view.installed.insert(n.clone(), p);
        }
    }
    view
}

/// Remove the packages (journaled as `pkg_remove`, so undo can offer to reinstall them).
pub fn run_uninstall(items: &[Item], report: &mut dyn FnMut(&str)) -> Outcome {
    let mut out = Outcome::default();
    let names: Vec<&str> = items.iter().filter_map(|i| if let Act::Remove(n) = &i.act { Some(n.as_str()) } else { None }).collect();
    if names.is_empty() {
        return out;
    }
    let version = |n: &str| crate::ctx::output("pacman", &["-Q", n]).and_then(|o| o.split_whitespace().nth(1).map(String::from)).unwrap_or_default();
    let versions: HashMap<&str, String> = names.iter().map(|n| (*n, version(n))).collect();
    report(&format!("Removing {}", names.join(" ")));
    let mut args = vec!["-Rns", "--noconfirm"];
    args.extend(&names);
    ok_cmd("pacman", &args);
    for n in names {
        if crate::ctx::ok("pacman", &["-Qq", n]) {
            out.failed.push(format!("{n} (pacman -Rns failed; see the log)"));
        } else {
            if let Some(j) = journal::current() {
                let _ = j.note("pkg_remove", json!({ "name": n, "version": versions[n] }));
            }
            out.done.push(format!("Removed {n}"));
        }
    }
    out
}

/// `kast uninstall <pkg…>`
pub fn cmd_uninstall(pkgs: &[String], dry_run: bool) -> Result<()> {
    let list = crate::pkglist::builtin();
    let names: Vec<String> = pkgs
        .iter()
        .map(|p| {
            list.packages()
                .find(|x| x.name == *p || x.bare() == p)
                .map(|x| x.bare().to_string())
                .ok_or_else(|| anyhow::anyhow!("{p} is not in kast's package list"))
        })
        .collect::<Result<_>>()?;
    let view = uninstall_view(&names);
    for n in names.iter().filter(|n| !view.installed.contains_key(*n)) {
        println!("{n} is not installed");
    }
    let items = uninstall_plan(&names, &view);
    for it in &items {
        println!("  {} {}{}", if it.undoable { "✔" } else { "-" }, it.label, it.warning.as_ref().map_or(String::new(), |w| format!("  ({w})")));
    }
    if dry_run || items.iter().all(|i| !i.undoable) {
        println!("{}", if dry_run { "Dry run: nothing was changed." } else { "Nothing to remove." });
        return Ok(());
    }
    let out = journal::recorded(&["uninstall"], false, || run_uninstall(&items, &mut |l| println!("==> {l}")));
    for f in &out.failed {
        println!("  ✘ {f}");
    }
    if out.failed.is_empty() { Ok(()) } else { bail!("{} package(s) could not be removed", out.failed.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn change(seq: u64, kind: &str, step: &str, data: Value) -> Change {
        Change { seq, kind: kind.into(), step: step.into(), status: Status::Done, data, newer: false }
    }

    fn run(changes: Vec<Change>) -> Run {
        Run { id: "r1".into(), ts: String::new(), version: "0.4.0".into(), host: "h".into(), user: "p3ter".into(), steps: vec![], changes }
    }

    fn pkg(name: &str, reason: &str) -> Value {
        json!({ "name": name, "version": "1", "repo": "extra", "reason": reason })
    }

    fn file(path: &str, before: Option<&str>, after: &str) -> Value {
        let b = match before {
            Some(h) => json!({ "sha256": h, "mode": 0o644, "uid": 0, "gid": 0 }),
            None => json!({ "absent": true }),
        };
        json!({ "path": path, "before": b, "after_sha256": after })
    }

    fn view(installed: &[(&str, bool, &[&str])], files: &[(&str, Option<&str>)]) -> View {
        View {
            installed: installed
                .iter()
                .map(|(n, e, r)| (n.to_string(), PkgNow { explicit: *e, required_by: r.iter().map(|s| s.to_string()).collect(), repo: String::new() }))
                .collect(),
            files: files.iter().map(|(p, h)| (PathBuf::from(p), h.map(String::from))).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn packages_first_then_newest_first_and_dependencies_kept() {
        let r = run(vec![
            change(1, "pacman_key", "repo", json!({ "fpr": "AAAABBBBCCCCDDDD", "was_present": false })),
            change(2, "file_write", "repo", file("/etc/pacman.conf", Some("old"), "new")),
            change(3, "pkg_install", "packages", pkg("sine", "explicit")),
            change(4, "pkg_install", "packages", pkg("libdep", "dependency")),
            change(5, "pkg_install", "packages", pkg("zotero", "explicit")),
            change(6, "pkg_install", "packages", pkg("gone", "explicit")),
            change(7, "service_enable", "services", json!({ "unit": "valkey", "was_enabled": false, "was_active": false })),
            change(8, "service_enable", "services", json!({ "unit": "sshd", "was_enabled": true, "was_active": true })),
            change(9, "pkg_install", "services", pkg("v4l-utils", "dependency")),
        ]);
        let v = view(
            &[("sine", true, &[]), ("libdep", false, &["sine"]), ("zotero", true, &["zotero-ocr"]), ("v4l-utils", true, &[])],
            &[("/etc/pacman.conf", Some("new"))],
        );
        let p = plan(&r, &v);
        let labels: Vec<&str> = p.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Keep v4l-utils",
                "Keep zotero",
                "Remove libdep",
                "Remove sine",
                "Disable and stop valkey",
                "Restore /etc/pacman.conf",
                "Remove the archrepo signing key CCCCDDDD",
            ]
        );
        assert_eq!(p[0].warning.as_deref(), Some("kept: marked as explicitly installed since"));
        assert_eq!(p[1].warning.as_deref(), Some("kept: needed by zotero-ocr"));
        assert!(p[2].warning.is_none(), "libdep is only needed by sine, which goes too");
        assert!(!p[0].undoable && p[2].undoable && p[2].default_on);
    }

    #[test]
    fn edited_files_are_flagged_and_pacman_conf_is_trimmed() {
        let r = run(vec![
            change(1, "file_write", "repo", file("/etc/pacman.conf", Some("old"), "new")),
            change(2, "file_write", "services", file("/etc/pam.d/sudo", Some("p0"), "p1")),
            change(3, "file_create", "services", file("/usr/lib/systemd/system-sleep/howdy-resume.sh", None, "h1")),
            change(4, "file_write", "services", file("/etc/pam.d/sddm", Some("s0"), "s1")),
        ]);
        let mut v = view(
            &[],
            &[
                ("/etc/pacman.conf", Some("edited")),
                ("/etc/pam.d/sudo", Some("edited")),
                ("/usr/lib/systemd/system-sleep/howdy-resume.sh", Some("h1")),
                ("/etc/pam.d/sddm", Some("s0")),
            ],
        );
        v.archrepo = vec!["beeper".into()];
        let p = plan(&r, &v);
        let labels: Vec<&str> = p.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Also remove beeper (from archrepo)",
                "Delete /usr/lib/systemd/system-sleep/howdy-resume.sh (kast created it)",
                "Restore /etc/pam.d/sudo",
                "Remove [archrepo] from /etc/pacman.conf",
            ],
            "sddm is already as it was"
        );
        assert!(!p[0].default_on);
        assert_eq!((p[2].warning.as_deref(), p[2].default_on), (Some("changed since kast wrote it"), false));
        assert!(p[3].warning.as_deref().unwrap().contains("stop receiving updates: beeper"));
        assert!(matches!(p[3].act, Act::RemoveRepoBlock(_)));
    }

    #[test]
    fn the_schedules_timer_can_be_removed() {
        let r = run(vec![
            change(
                1,
                "user_timer",
                "schedule",
                json!({ "action": "enable", "timer": "/home/p3ter/.config/systemd/user/kast-backup.timer", "before_service": null, "before_timer": null }),
            ),
            change(2, "user_timer", "schedule", json!({ "action": "disable", "timer": "/home/p3ter/.config/systemd/user/kast-backup.timer" })),
        ]);
        let p = plan(&r, &view(&[], &[]));
        assert_eq!(p.len(), 2);
        assert!(!p[0].undoable && p[0].label == "Scheduled backup turned off");
        assert_eq!(p[1].act, Act::UndoTimer { before_service: None, before_timer: None });
        assert!(p[1].default_on);
        assert_eq!(describe(&r.changes[0]), "user_timer enable /home/p3ter/.config/systemd/user/kast-backup.timer");
    }

    #[test]
    fn uploads_go_to_drives_trash_unchecked() {
        let r = run(vec![change(
            1,
            "remote_upload",
            "export",
            json!({ "remote": "/my-files/kast-backups/h/all_backup_x.zip", "sidecar": "/my-files/kast-backups/h/all_backup_x.zip.manifest.json", "size": 5 }),
        )]);
        let p = plan(&r, &view(&[], &[]));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].label, "Move the uploaded backup /my-files/kast-backups/h/all_backup_x.zip to Proton Drive's trash");
        assert!(!p[0].default_on && p[0].undoable);
        assert_eq!(
            p[0].act,
            Act::TrashRemote(vec!["/my-files/kast-backups/h/all_backup_x.zip".into(), "/my-files/kast-backups/h/all_backup_x.zip.manifest.json".into()])
        );
        assert_eq!(describe(&r.changes[0]), "remote_upload /my-files/kast-backups/h/all_backup_x.zip");
    }

    #[test]
    fn imports_exports_upgrades_and_undone_changes() {
        let mut undone = change(5, "pkg_install", "packages", pkg("zotero", "explicit"));
        undone.status = Status::Undone;
        let mut incomplete = change(6, "file_write", "services", file("/etc/pam.d/sudo", Some("p0"), "p1"));
        incomplete.status = Status::Incomplete;
        let r = run(vec![
            change(1, "export", "export", json!({ "file": "/media/data/backups/all.zip" })),
            change(2, "upgrade", "upgrade", json!({ "snapshot": { "tool": "snapper", "pre": 41, "post": 42 } })),
            change(3, "import", "import", json!({ "archive": "/b/all.zip", "apps": ["zen"], "snapshot": null })),
            change(4, "import", "import", json!({ "archive": "/b/kde.zip", "apps": ["kde"], "snapshot": "/home/p3ter/snap", "snapshot_fingerprint": "f1" })),
            undone,
            incomplete,
            change(7, "pkg_replace", "packages", json!({ "name": "sine-bin", "version": "2.0", "by": "archrepo/sine" })),
        ]);
        let mut v = view(&[("zotero", true, &[])], &[("/etc/pam.d/sudo", Some("p1"))]);
        v.snapshots.insert("/home/p3ter/snap".into(), Some("f1".into()));
        let p = plan(&r, &v);
        let labels: Vec<&str> = p.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Restore /etc/pam.d/sudo",
                "Put back the data the restore of kde.zip replaced (kde)",
                "Restore of all.zip",
                "System upgrade",
                "Delete the backup /media/data/backups/all.zip",
                "Reinstall sine-bin 2.0",
            ],
            "the undone zotero install is gone from the list"
        );
        assert_eq!(p[0].warning.as_deref(), Some("incomplete: kast stopped in the middle of this change"));
        assert_eq!((p[2].undoable, p[2].warning.as_deref()), (false, Some("not undoable (no snapshot was taken)")));
        assert!(p[3].warning.as_deref().unwrap().contains("snapper -c root rollback 41"));
        assert!(p[3].warning.as_deref().unwrap().contains("undochange 41..42"));
        assert!(!p[4].default_on && !p[5].default_on, "deleting user files and reinstalls start unchecked");
        // A changed snapshot is flagged, a missing one makes it not undoable
        v.snapshots.insert("/home/p3ter/snap".into(), Some("f2".into()));
        assert!(plan(&r, &v)[1].warning.as_deref().unwrap().contains("changed since"));
        v.snapshots.insert("/home/p3ter/snap".into(), None);
        assert!(!plan(&r, &v)[1].undoable);
    }

    #[test]
    fn an_undone_run_has_nothing_left() {
        let t = tempfile::tempdir().unwrap();
        let j = journal::Journal::open(t.path(), &["services"], "p3ter").unwrap();
        j.note("service_enable", json!({ "unit": "valkey", "was_enabled": false, "was_active": false })).unwrap();
        let id = j.run.clone();
        drop(j);
        let runs = journal::runs(&journal::load(t.path()).unwrap().entries);
        let p = plan(&runs[0], &View::default());
        assert_eq!(p.len(), 1);
        journal::mark_undone(t.path(), &id, p[0].seqs[0], "p3ter", "x").unwrap();
        let runs = journal::runs(&journal::load(t.path()).unwrap().entries);
        assert!(plan(&runs[0], &View::default()).is_empty());
        assert_eq!(listed(&runs).len(), 1, "the undo's own run isn't listed");
        assert_eq!(prunable(&runs, 10), [id], "fully undone: its copies can go");
    }

    #[test]
    fn pam_check_catches_broken_files() {
        let t = tempfile::tempdir().unwrap();
        let pam = t.path().join("pam.d");
        let mods = t.path().join("security");
        fs::create_dir_all(&pam).unwrap();
        fs::create_dir_all(&mods).unwrap();
        fs::write(mods.join("pam_unix.so"), "").unwrap();
        fs::write(pam.join("system-auth"), "#%PAM-1.0\nauth [success=1 default=bad] pam_unix.so try_first_pass\naccount required pam_unix.so\n").unwrap();
        fs::write(pam.join("sudo"), "#%PAM-1.0\nauth include system-auth\n-auth optional pam_gone.so\n").unwrap();
        let dirs = vec![mods.clone()];
        assert_eq!(pam_check(&pam, &dirs, &[]), Ok(()));
        fs::write(pam.join("sudo"), "auth sufficient pam_howdy.so\nauth include system-auth\n").unwrap();
        assert!(pam_check(&pam, &dirs, &["sudo".into()]).unwrap_err().contains("module pam_howdy.so doesn't exist"));
        fs::write(pam.join("sudo"), "auth include nowhere\n").unwrap();
        assert!(pam_check(&pam, &dirs, &[]).unwrap_err().contains("includes nowhere"));
        fs::write(pam.join("sudo"), "account required pam_unix.so\n").unwrap();
        assert!(pam_check(&pam, &dirs, &[]).unwrap_err().contains("no auth stack"));
        fs::write(pam.join("sudo"), "auth\n").unwrap();
        assert!(pam_check(&pam, &dirs, &[]).is_err());
        fs::write(pam.join("sudo"), "bogus required pam_unix.so\n").unwrap();
        assert!(pam_check(&pam, &dirs, &[]).unwrap_err().contains("unknown type"));
    }

    #[test]
    fn broken_pam_restore_is_rolled_back() {
        let t = tempfile::tempdir().unwrap();
        journal::with_root(t.path(), || {
            fs::create_dir_all(sys_path("/etc/pam.d")).unwrap();
            fs::create_dir_all(sys_path("/usr/lib/security")).unwrap();
            fs::write(sys_path("/usr/lib/security/pam_unix.so"), "").unwrap();
            fs::write(sys_path("/etc/pam.d/sudo"), "auth required pam_unix.so\n").unwrap();
            // The journal's "before" copy is broken (say, from a machine with another module)
            journal::begin_run(&["services"], "p3ter").unwrap();
            let dir = journal::state_dir();
            fs::write(sys_path("/etc/pam.d/sudo"), "auth required pam_missing.so\n").unwrap();
            journal::write_file(&sys_path("/etc/pam.d/sudo"), b"auth required pam_unix.so\n", None).unwrap();
            journal::end_run();
            let runs = journal::runs(&journal::load(&dir).unwrap().entries);
            let v = gather(&[&runs[0]]);
            let items = plan(&runs[0], &v);
            assert_eq!(items[0].label, format!("Restore {}", sys_path("/etc/pam.d/sudo").display()));
            let err = execute(&items, &dir, "p3ter", &mut |_, _| {}).unwrap_err().to_string();
            assert!(err.contains("failed the sanity check"), "{err}");
            assert_eq!(fs::read_to_string(sys_path("/etc/pam.d/sudo")).unwrap(), "auth required pam_unix.so\n", "put back");
        });
    }

    #[test]
    fn repo_block_is_cut_out() {
        assert_eq!(without_repo_block("[core]\nA=1\n\n[archrepo]\nSigLevel = Required\nServer = x\n\n[extra]\nB=2\n"), "[core]\nA=1\n\n[extra]\nB=2\n");
        assert_eq!(without_repo_block("[core]\n[archrepo]\nServer = x\n"), "[core]\n");
    }

    #[test]
    fn qi_is_read() {
        let qi = "Name            : libdep\nRequired By     : sine  zotero\nInstall Reason  : Installed as a dependency for another package\n";
        assert_eq!(parse_qi(qi), PkgNow { explicit: false, required_by: vec!["sine".into(), "zotero".into()], repo: String::new() });
        assert!(parse_qi("Required By : None\nInstall Reason : Explicitly installed\n").explicit);
    }

    #[test]
    fn uninstall_lists_and_keeps_what_is_needed() {
        let list = crate::pkglist::builtin();
        let installed: HashSet<String> = ["zotero", "sine", "steam"].map(String::from).into();
        let r = run(vec![change(1, "pkg_install", "packages", pkg("sine", "explicit"))]);
        let apps = installed_apps(&list, &installed, &[r]);
        let names: Vec<(&str, bool)> = apps.iter().map(|a| (a.name.as_str(), a.by_kast.is_some())).collect();
        assert_eq!(names, [("zotero", false), ("steam", false), ("sine", true)]);
        let v = view(&[("zotero", true, &["zotero-ocr"]), ("zotero-ocr", true, &[]), ("steam", true, &[])], &[]);
        let p = uninstall_plan(&["zotero".into(), "steam".into()], &v);
        assert_eq!(p.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Keep zotero", "Remove steam"]);
        assert_eq!(p[0].warning.as_deref(), Some("kept: needed by zotero-ocr"));
        let p = uninstall_plan(&["zotero".into(), "zotero-ocr".into()], &v);
        assert!(p.iter().all(|i| i.undoable), "removed together");
    }

    #[test]
    fn short_diff_of_an_edited_file_and_the_force_rule() {
        let t = tempfile::tempdir().unwrap();
        let j = journal::Journal::open(t.path(), &[], "p3ter").unwrap();
        let hash = j.store(b"a\nb\nc\n").unwrap();
        let f = t.path().join("f");
        fs::write(&f, "a\nc\nd\n").unwrap();
        let it = Item {
            run: j.run.clone(),
            seqs: vec![],
            step: String::new(),
            act: Act::RestoreFile { path: f, before: Some(hash), mode: 0o644, uid: 0, gid: 0 },
            label: "Restore f".into(),
            warning: Some("changed since kast wrote it".into()),
            default_on: false,
            undoable: true,
        };
        assert_eq!(diff_lines(t.path(), &it, 6), ["- b", "+ d"]);
        assert_eq!(diff_lines(t.path(), &it, 1), ["- b", "… 1 more"]);
        assert!(cli_choice(std::slice::from_ref(&it), false).is_empty(), "the CLI skips it");
        assert_eq!(cli_choice(std::slice::from_ref(&it), true).len(), 1, "unless --force");
    }

    #[test]
    fn pruned_runs_cant_restore_files() {
        let r = run(vec![
            change(1, "file_write", "services", file("/etc/pam.d/sudo", Some("p0"), "p1")),
            change(2, "file_create", "services", file("/usr/lib/hook.sh", None, "h1")),
            change(3, "service_enable", "services", json!({ "unit": "valkey", "was_enabled": false, "was_active": false })),
        ]);
        let mut v = view(&[], &[("/etc/pam.d/sudo", Some("p1")), ("/usr/lib/hook.sh", Some("h1"))]);
        v.pruned.insert("r1".into());
        let p = plan(&r, &v);
        let st: Vec<(&str, bool)> = p.iter().map(|i| (i.label.as_str(), i.undoable)).collect();
        assert_eq!(st, [("Disable and stop valkey", true), ("Delete /usr/lib/hook.sh (kast created it)", true), ("Restore /etc/pam.d/sudo", false)]);
        assert_eq!(p[2].warning.as_deref(), Some("not undoable (file copies pruned)"));
    }

    #[test]
    fn a_failed_pam_check_stops_the_undo_there() {
        let t = tempfile::tempdir().unwrap();
        journal::with_root(t.path(), || {
            fs::create_dir_all(sys_path("/etc/pam.d")).unwrap();
            fs::create_dir_all(sys_path("/usr/lib/security")).unwrap();
            fs::write(sys_path("/usr/lib/security/pam_unix.so"), "").unwrap();
            journal::begin_run(&["services"], "p3ter").unwrap();
            let dir = journal::state_dir();
            fs::write(sys_path("/etc/pam.d/sudo"), "auth required pam_gone.so\n").unwrap();
            journal::write_file(&sys_path("/etc/pam.d/sudo"), b"auth required pam_unix.so\n", None).unwrap();
            journal::write_file(&sys_path("/usr/lib/hook.sh"), b"#!/bin/sh\n", Some(0o755)).unwrap();
            journal::end_run();
            let runs = journal::runs(&journal::load(&dir).unwrap().entries);
            let items = plan(&runs[0], &gather(&[&runs[0]]));
            // Newest first: the hook, then sudo
            assert_eq!(items.len(), 2);
            let err = execute(&items, &dir, "p3ter", &mut |_, _| {}).unwrap_err().to_string();
            assert!(err.contains("Already undone: Delete") && err.contains("hook.sh"), "{err}");
            assert!(!sys_path("/usr/lib/hook.sh").exists());
            assert_eq!(fs::read_to_string(sys_path("/etc/pam.d/sudo")).unwrap(), "auth required pam_unix.so\n");
        });
    }
}
