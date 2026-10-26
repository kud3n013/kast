//! Ollama's models. A model store (`OLLAMA_MODELS`) holds `blobs/` (the weights, templates
//! and parameters, named by their sha256) and the manifests that name the models and list
//! their blobs: `manifests/<host>/<namespace>/<model>/<tag>` (JSON), and since Ollama 0.40
//! also `manifests-v2/...`, symlinks to the manifest's own blob.
//!
//! Arch's `ollama.service` keeps the store in /var/lib/ollama, owned by the `ollama` user
//! (readable by everyone, so a backup needs no root; a restore writes it through sudo). An
//! `ollama serve` the user runs themselves keeps it in ~/.ollama/models.
//!
//! Without media (`--no-media`), only the small blobs go in: the manifests, templates and
//! parameters. A restore then downloads the models whose weights are missing again
//! (`ollama pull`). The server's key (`~/.ollama/id_ed25519`, its identity on ollama.com) is
//! never backed up.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::ctx::{self, ctx, have, ok_priv, output, reported};

use super::backup::BackupOpts;
use super::import::{ImportOpts, user_cmd};
use super::{log_info, log_step, log_success, log_warn, rsync_as, rsync_dir, rsync_root, sudo_ready_for};

/// The manifest trees of a store (the old layout and Ollama 0.40's).
pub const MANIFEST_DIRS: [&str; 2] = ["manifests", "manifests-v2"];

/// Blobs larger than this are weights: left out of a backup without media.
const MEDIA_MAX_SIZE: &str = "--max-size=1M";

/// Where a store may be, most specific first: kast's own `OLLAMA_MODELS`, the user's
/// ~/.ollama/models, the store of the installed `ollama.service`, and the system stores of
/// Arch's package and of Ollama's own installer.
fn candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("OLLAMA_MODELS").filter(|s| !s.is_empty()).map(PathBuf::from).into_iter().collect();
    v.push(ctx().target_home.join(".ollama/models"));
    v.extend(service_store());
    v.extend(["/var/lib/ollama", "/usr/share/ollama/.ollama/models"].map(PathBuf::from));
    v.dedup();
    v
}

fn has_models(store: &Path) -> bool {
    MANIFEST_DIRS.iter().any(|d| store.join(d).is_dir())
}

/// The store that has models, if any.
pub fn find_store() -> Option<PathBuf> {
    candidates().into_iter().find(|p| has_models(p))
}

/// Where a restore puts the models: the store in use, else where Ollama would look first.
pub fn restore_store() -> PathBuf {
    find_store().unwrap_or_else(|| {
        let env = std::env::var_os("OLLAMA_MODELS").filter(|s| !s.is_empty()).map(PathBuf::from);
        env.or_else(service_store).unwrap_or_else(|| ctx().target_home.join(".ollama/models"))
    })
}

fn service_store() -> Option<PathBuf> {
    let out = output("systemctl", &["show", "ollama.service", "-p", "LoadState", "-p", "User", "-p", "Environment"])?;
    let home = |user: &str| nix::unistd::User::from_name(user).ok().flatten().map(|u| u.dir);
    parse_service_store(&out, home)
}

/// The store an installed `ollama.service` uses (`systemctl show` output): its
/// `OLLAMA_MODELS`, else ~/.ollama/models of its `HOME` or of its user.
pub fn parse_service_store(show: &str, home_of: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let field = |k: &str| show.lines().find_map(|l| l.strip_prefix(k)).unwrap_or("");
    if field("LoadState=") != "loaded" {
        return None;
    }
    let env = field("Environment=");
    let var = |k: &str| env.split_whitespace().find_map(|kv| kv.strip_prefix(k)).map(|v| v.trim_matches('"')).filter(|v| !v.is_empty());
    if let Some(models) = var("OLLAMA_MODELS=") {
        return Some(PathBuf::from(models));
    }
    let home = var("HOME=").map(PathBuf::from).or_else(|| Some(field("User=")).filter(|u| !u.is_empty()).and_then(home_of));
    home.map(|h| h.join(".ollama/models"))
}

/// A model's name from its manifest's path below the manifest tree (`host/ns/model/tag`),
/// the way `ollama list` shows it.
pub fn model_name(rel: &Path) -> Option<String> {
    let parts: Vec<&str> = rel.iter().map(|p| p.to_str()).collect::<Option<_>>()?;
    let [host, ns, model, tag] = parts[..] else { return None };
    let official = matches!(host, "registry.ollama.ai" | "ollama.com");
    Some(match (official, ns) {
        (true, "library") => format!("{model}:{tag}"),
        (true, _) => format!("{ns}/{model}:{tag}"),
        _ => format!("{host}/{ns}/{model}:{tag}"),
    })
}

/// The models of a store and their manifest files (one per manifest tree).
pub fn models(store: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for tree in MANIFEST_DIRS.map(|d| store.join(d)) {
        for e in WalkDir::new(&tree).min_depth(4).max_depth(4).follow_links(false).sort_by_file_name().into_iter().flatten() {
            if e.file_type().is_dir() {
                continue;
            }
            if let Some(name) = e.path().strip_prefix(&tree).ok().and_then(model_name) {
                out.entry(name).or_default().push(e.path().to_path_buf());
            }
        }
    }
    out
}

/// The blob files a manifest names (`sha256:x` is stored as `blobs/sha256-x`).
fn blobs_of(manifest: &Path) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(&fs::read_to_string(manifest).ok()?).ok()?;
    let layers = v.get("layers").and_then(Value::as_array).into_iter().flatten();
    v.get("config").into_iter().chain(layers).map(|l| l.get("digest").and_then(Value::as_str).map(|d| d.replace(':', "-"))).collect()
}

/// Every blob of every manifest of the model is in one of `blob_dirs`.
fn complete(manifests: &[PathBuf], blob_dirs: &[PathBuf]) -> bool {
    manifests.iter().all(|m| blobs_of(m).is_some_and(|bs| bs.iter().all(|b| blob_dirs.iter().any(|d| d.join(b).is_file()))))
}

/// Who a store's files belong to: its owner, else the user for a store in the home, else
/// the `ollama` service user.
fn store_owner(store: &Path) -> (u32, u32) {
    let c = ctx();
    if let Ok(m) = store.metadata() {
        return (m.uid(), m.gid());
    }
    if store.starts_with(&c.target_home) {
        return (c.target_uid, c.target_gid);
    }
    nix::unistd::User::from_name("ollama").ok().flatten().map_or((0, 0), |u| (u.uid.as_raw(), u.gid.as_raw()))
}

/// A store the user can't write: copied as root, handed to its owner.
fn needs_root(owner: (u32, u32)) -> bool {
    owner.0 != ctx().target_uid
}

const ROOT_WHY: &str = "Ollama's models are owned by the ollama user: writing them needs root access.";

/// Ask for the sudo password before a restore starts, when the store needs it (the answer
/// is kept for the restore).
pub fn ask_for_root() {
    if !ctx().is_root && needs_root(store_owner(&restore_store())) {
        sudo_ready_for(ROOT_WHY);
    }
}

fn copy_into(src: &Path, dest: &Path, owner: (u32, u32), delete: bool) -> Result<()> {
    let (s, d) = (format!("{}/", src.display()), format!("{}/", dest.display()));
    if needs_root(owner) {
        let chown = format!("--chown={}:{}", owner.0, owner.1);
        // Not -a: no devices, owners or modes from the archive. Links only within the tree
        // (manifests-v2 points into blobs/)
        let mut opts = vec!["-rlt", "--safe-links", "--chmod=Du=rwx,go=rx,Fu=rw,go=r", &chown];
        if delete {
            opts.push("--delete");
        }
        rsync_root(&opts, Path::new(&s), &d)
    } else {
        let c = ctx();
        ctx::mkdir_user(dest)?;
        let mut opts = vec!["-a", "--safe-links", "--chmod=Du+rwx,Fu+rw,go-w"];
        if delete {
            opts.push("--delete");
        }
        rsync_as(c.is_root && c.target_uid != 0, &opts, Path::new(&s), &d)
    }
}

fn mkdir_owned(dir: &Path, owner: (u32, u32)) -> bool {
    let d = dir.to_string_lossy();
    ok_priv("mkdir", &["-p", &d]) && ok_priv("chown", &[&format!("{}:{}", owner.0, owner.1), &d])
}

pub fn backup(staging: &Path, o: &BackupOpts) -> Result<Option<Value>> {
    let Some(store) = find_store() else {
        log_warn("Ollama models (~/.ollama/models or /var/lib/ollama) not found, skipping...");
        return Ok(None);
    };
    log_step(&format!("Processing Ollama models ({})...", store.display()));
    let names: Vec<String> = models(&store).into_keys().collect();
    // Only the blobs and manifests: the rest is Ollama's own (the service's home is the store
    // itself, with its key; metadata/ is a 0600 cache it rebuilds), and unfinished downloads
    let mut filters: Vec<String> = vec!["--exclude=*-partial*".into(), "--include=/blobs/***".into()];
    filters.extend(MANIFEST_DIRS.map(|d| format!("--include=/{d}/***")));
    filters.push("--exclude=*".into());
    if !o.include_media {
        filters.push(MEDIA_MAX_SIZE.to_string());
    }
    let dest = staging.join("ollama");
    fs::create_dir_all(&dest)?;
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would sync: {} -> ollama ({})", store.display(), names.join(", ")));
    } else {
        rsync_dir(&filters, &store, &dest)?;
    }
    if !o.include_media && !names.is_empty() {
        log_info("  Without media: the weights are left out; a restore downloads them again (ollama pull).");
    }
    Ok(Some(json!({
        "included": true,
        "folder": "ollama",
        "config_path": store.to_string_lossy(),
        "models": names,
        "include_media": o.include_media,
    })))
}

pub fn restore(staging: &Path, rollback: &Path, o: &ImportOpts) -> Result<()> {
    let src = staging.join("ollama");
    if !src.is_dir() {
        return Ok(());
    }
    let dest = restore_store();
    let blob_dirs = [src.join("blobs"), dest.join("blobs")];
    let mut pull = Vec::new();
    let mut names = Vec::new();
    for (name, manifests) in models(&src) {
        if complete(&manifests, &blob_dirs) {
            names.push(name);
        } else {
            // A manifest without its weights would list a model that can't run: download it instead
            if !o.dry_run {
                for m in &manifests {
                    fs::remove_file(m)?;
                }
            }
            pull.push(name);
        }
    }
    if o.dry_run {
        log_info(&format!("  [Dry-run] Would restore Ollama models to: {} ({})", dest.display(), names.join(", ")));
        for name in &pull {
            log_info(&format!("  [Dry-run] Would download again: ollama pull {name}"));
        }
        return Ok(());
    }
    let owner = store_owner(&dest);
    if needs_root(owner) && !ctx().is_root && !sudo_ready_for(ROOT_WHY) {
        log_warn(&format!("Skipping the Ollama models: {} needs root and sudo would ask for a password here. Run the command with sudo.", dest.display()));
        return Ok(());
    }
    // Undo takes the manifests back (an empty tree too: a model the restore added goes away
    // again); Ollama removes the blobs no model uses any more when it starts
    if o.safety_backup {
        for d in MANIFEST_DIRS {
            let snap = rollback.join("ollama").join(d);
            fs::create_dir_all(&snap)?;
            if dest.join(d).is_dir() {
                rsync_dir(&[], &dest.join(d), &snap)?;
            }
        }
    }
    if needs_root(owner) && !dest.is_dir() && !mkdir_owned(&dest, owner) {
        log_warn(&format!("Could not create {}", dest.display()));
        return Err(reported());
    }
    copy_into(&src, &dest, owner, false)?;
    log_success(&format!("Ollama models restored to {}: {}", dest.display(), if names.is_empty() { "none complete".into() } else { names.join(", ") }));
    pull_again(&pull);
    Ok(())
}

/// Download the models whose weights weren't in the archive (needs the Ollama server).
fn pull_again(names: &[String]) {
    if names.is_empty() {
        return;
    }
    if !have("ollama") {
        log_warn(&format!("The weights of {} weren't in the backup. Install Ollama, then: ollama pull <model>", names.join(", ")));
        return;
    }
    for name in names {
        log_info(&format!("Downloading {name} again (its weights weren't in the backup)..."));
        let mut cmd = user_cmd(&["ollama", "pull", name]);
        if crate::report::owns_terminal() {
            cmd.stdin(std::process::Stdio::null()).stdout(ctx::log_stdio()).stderr(ctx::log_stdio());
        }
        if crate::report::run_tracked(&mut cmd).is_ok_and(|s| s.success()) {
            log_success(&format!("Downloaded {name}"));
        } else {
            log_warn(&format!("Could not download {name} (is the Ollama server running?). Later: ollama pull {name}"));
        }
    }
}

/// What a restore overwrites: the manifests (the blobs are only ever added).
pub fn snapshot_paths() -> Vec<PathBuf> {
    let store = restore_store();
    MANIFEST_DIRS.iter().map(|d| store.join(d)).collect()
}

/// Put the manifests of the snapshot back. Into a store owned by the ollama user only while
/// the snapshot is `intact` (it's copied there as root). Returns whether anything went back.
pub fn undo(snapshot: &Path, intact: bool) -> Result<bool> {
    let src = snapshot.join("ollama");
    if !src.is_dir() {
        return Ok(false);
    }
    let store = restore_store();
    let owner = store_owner(&store);
    if needs_root(owner) {
        if !intact {
            log_warn("The snapshot changed since the restore: the Ollama models are not put back.");
            return Ok(false);
        }
        if !ctx().is_root && !sudo_ready_for(ROOT_WHY) {
            log_warn(&format!("Not putting back the Ollama models: {} needs root.", store.display()));
            return Ok(false);
        }
    }
    for d in MANIFEST_DIRS {
        if src.join(d).is_dir() {
            copy_into(&src.join(d), &store.join(d), owner, true)?;
        }
    }
    log_info("Ollama removes the weights no model uses any more the next time it starts.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_read_like_ollama_list() {
        let n = |p: &str| model_name(Path::new(p));
        assert_eq!(n("registry.ollama.ai/library/qwen3.5/4b").as_deref(), Some("qwen3.5:4b"));
        assert_eq!(n("ollama.com/library/llama3/latest").as_deref(), Some("llama3:latest"));
        assert_eq!(n("registry.ollama.ai/someone/tool/q4").as_deref(), Some("someone/tool:q4"));
        assert_eq!(n("hf.co/org/model/Q8_0").as_deref(), Some("hf.co/org/model:Q8_0"));
        assert_eq!(n("registry.ollama.ai/library/qwen3.5"), None);
    }

    #[test]
    fn the_service_store_comes_from_its_environment_then_its_home() {
        let no_home = |_: &str| None;
        let arch = "LoadState=loaded\nUser=ollama\nEnvironment=HOME=/var/lib/ollama OLLAMA_MODELS=/var/lib/ollama OLLAMA_ORIGINS=moz-extension://*\n";
        assert_eq!(parse_service_store(arch, no_home), Some(PathBuf::from("/var/lib/ollama")));
        let home_only = "LoadState=loaded\nUser=ollama\nEnvironment=HOME=/srv/o\n";
        assert_eq!(parse_service_store(home_only, no_home), Some(PathBuf::from("/srv/o/.ollama/models")));
        // Ollama's installer: no environment, the ollama user's home
        let installer = "LoadState=loaded\nUser=ollama\nEnvironment=\n";
        let home = |u: &str| (u == "ollama").then(|| PathBuf::from("/usr/share/ollama"));
        assert_eq!(parse_service_store(installer, home), Some(PathBuf::from("/usr/share/ollama/.ollama/models")));
        assert_eq!(parse_service_store("LoadState=not-found\nUser=\nEnvironment=\n", no_home), None);
    }

    #[test]
    fn a_model_is_complete_when_every_blob_its_manifest_names_is_there() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let m = t.path().join("m");
        fs::write(&m, r#"{"config":{"digest":"sha256:c"},"layers":[{"digest":"sha256:w"}]}"#).unwrap();
        fs::write(a.join("sha256-c"), "").unwrap();
        assert!(!complete(std::slice::from_ref(&m), &[a.clone(), b.clone()]));
        fs::write(b.join("sha256-w"), "").unwrap();
        assert!(complete(std::slice::from_ref(&m), &[a, b]), "blobs may be in the archive or already in the store");
        assert!(!complete(&[t.path().join("gone")], &[]), "a dangling manifest is not complete");
    }
}
