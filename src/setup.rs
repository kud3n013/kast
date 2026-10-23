//! System setup steps: upgrade, data partition, kud3n repo, packages, services, and the
//! automatic run that chains them.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::ctx::{self, add_issue, ctx, have, log, ok, ok_logged, output, run_as_user};
use crate::ir;
use crate::journal::{self, sys_path};
use crate::pkglist::{Parsed, Pkg, PkgList};
use crate::ui::{self, C_BOLD, C_GRAY, C_GREEN, C_RED, C_RESET, C_YELLOW, Dash};

pub const MOUNT_POINT: &str = "/media/data";
pub const REPO_URL: &str = "https://github.com/kud3n013/archrepo/releases/download/packages";
const OLD_REPO_URL: &str = "https://kud3n013.github.io/archrepo/x86_64";
/// archrepo signs its database and packages with this key (see the archrepo README).
pub(crate) const REPO_KEY: &str = include_str!("../assets/archrepo.asc");
pub(crate) const REPO_KEY_FPR: &str = "CFB791543B4B2D2821E9127AC012A9476C170A37";
const PAM_RULE: &str = "auth      sufficient pam_howdy.so workaround=native";
/// Directly above PAM_RULE, aligned with it: `{kast} ir-emitter enable --device {device}`.
const EMITTER_RULE: &str = "auth      optional   pam_exec.so quiet";
const HOWDY_HOOK: &str = "/usr/lib/systemd/system-sleep/howdy-resume.sh";
pub const USER_DIRS: [&str; 9] = ["Desktop", "Documents", "Downloads", "Music", "Pictures", "Public", "Templates", "Videos", "Projects"];

fn pacman(args: &[&str]) -> Command {
    let mut c = Command::new("pacman");
    c.args(args);
    c
}

impl Dash {
    /// Log a sub-step and redraw the dashboard (`update_p` in the shell version).
    fn step(&self, tag: &str, s_step: i64, s_total: i64, item: &str, msg: &str) {
        log("INFO", &format!("{tag} Step {s_step}: {item} ({msg})"));
        self.render(s_step, s_total, item, msg, None);
    }
}

// --- State 0: Systemwide upgrade ------------------------------------------------------

/// The upgrade, with a snapshot before (and after) it when `snapshot` and the machine has a
/// tool for it. If the snapshot fails, the user is asked whether to go on without one; with
/// no one to ask, the upgrade (and the run) stops. Returns the snapshots taken.
pub fn system_upgrade(auto: bool, o_start: i64, o_step: i64, o_total: i64, snapshot: bool) -> Result<serde_json::Value> {
    let title = "Systemwide Upgrade (pacman -Syuu)";
    let d = Dash::new(auto, o_start, o_step, o_total, title);
    let det = crate::snapshot::detect();
    for w in det.warning() {
        log("INFO", &w);
    }
    let mut pre = None;
    if snapshot && det.offered() {
        d.render(0, 2, "Taking a snapshot", &format!("Taking a {} snapshot before upgrading...", det.tool_name()), None);
        match crate::snapshot::take_pre(&det) {
            Ok(n) => {
                log("INFO", &format!("Took {} snapshot {n} before the upgrade", det.tool_name()));
                pre = Some(n);
            }
            Err(e) => {
                log("WARN", &format!("Snapshot before the upgrade failed: {e:#}"));
                add_issue(format!("Snapshot before the upgrade failed: {e}"));
                let go = crate::report::ask(&crate::report::Prompt::yes_no("Taking a snapshot before the upgrade failed. Upgrade without one?", false));
                if !go {
                    anyhow::bail!("Upgrade stopped: no snapshot could be taken (kast auto --no-snapshot upgrades without one)");
                }
            }
        }
    }
    log("INFO", "Starting systemwide upgrade...");
    let started = ctx::now_str("%Y-%m-%dT%H:%M:%S");
    let rc = d.exec_monitored(1, 2, "Refreshing databases & upgrading", pacman(&["-Syuu", "--noconfirm"]));
    let snap = match pre {
        Some(p) => crate::snapshot::record(&det, p, crate::snapshot::take_post(&det, p)),
        None => serde_json::Value::Null,
    };
    // Not undoable by kast: recorded with the snapshots, so the undo screen can say how to roll back
    journal::upgrade(&started, &ctx::now_str("%Y-%m-%dT%H:%M:%S"), snap.clone());
    let finish = if rc != 0 {
        log("WARN", &format!("pacman -Syuu returned non-zero code ({rc}). Check {} for details.", ctx().log_file.display()));
        add_issue(format!("System upgrade (pacman -Syuu) failed with exit code {rc}"));
        "Warning: pacman -Syuu finished with warnings/errors"
    } else {
        "System packages up to date."
    };
    d.render(2, 2, "Upgrade Complete", finish, Some("✔"));
    ui::sleep_secs(1.0);
    log("INFO", "Systemwide upgrade completed.");
    if let Some(h) = crate::snapshot::rollback_hint(&snap) {
        log("INFO", &h);
    }
    Ok(snap)
}

// --- State 2: Universal data partition --------------------------------------------------

fn is_mounted(p: &str) -> bool {
    ok("findmnt", &[p])
}

/// The data partition step: the plan was decided (and shown) before the run started. Every
/// change is journaled by `partition::apply`, and a failure rolls back what it did.
pub fn setup_data_partition(
    plan: &crate::partition::apply::Plan,
    auto: bool,
    o_start: i64,
    o_step: i64,
    o_total: i64,
) -> Result<crate::partition::apply::Report> {
    use crate::partition::apply::{self, Env};
    let d = Dash::new(auto, o_start, o_step, o_total, "Setup Data Partition");
    let n = std::cell::Cell::new(0);
    let progress = |m: &str| {
        n.set((n.get() + 1).min(9));
        d.step("Partition", n.get(), 10, "Data partition", m);
    };
    let env = Env::new(&crate::partition::SysRunner, &progress);
    let result = apply::apply(plan, &env);
    apply::cleanup_probes();
    let r = result?;
    for note in &r.notes {
        log("INFO", note);
    }
    for (dir, why) in &r.kept {
        log("WARN", &format!("~/{dir} kept: {why}"));
    }
    for (dir, why) in &r.failed {
        log("ERROR", &format!("~/{dir} not moved: {why}"));
        add_issue(format!("Could not move ~/{dir} to {} ({why}); the original was kept", r.mountpoint));
    }
    if r.writable_as_user == Some(false) {
        add_issue(format!("{} is mounted but your user cannot write to it", r.mountpoint));
    }
    d.render(10, 10, "Completed", &format!("{} mounted: {}", r.mountpoint, r.line), Some("✔"));
    ui::sleep_secs(1.0);
    Ok(r)
}

/// What a home folder is now.
#[derive(Debug, Clone, PartialEq)]
pub enum HomeState {
    Dir,
    File,
    /// A symlink, to here
    Link(PathBuf),
    Absent,
}

pub fn home_state(p: &Path) -> HomeState {
    match p.symlink_metadata() {
        Err(_) => HomeState::Absent,
        Ok(m) if m.file_type().is_symlink() => HomeState::Link(fs::read_link(p).unwrap_or_default()),
        Ok(m) if m.is_dir() => HomeState::Dir,
        Ok(_) => HomeState::File,
    }
}

/// How moving one home folder went.
#[derive(Debug, Clone, PartialEq)]
pub enum Moved {
    /// Copied (if it was there) and linked: the home path is now a symlink kast made
    Linked,
    /// Already a link to the right place: nothing to do
    AlreadyLinked,
    /// Left alone, and why
    Kept(String),
    /// The copy failed; the original is untouched
    Failed,
}

/// Move one home folder onto the data partition and leave a symlink in its place. The
/// original goes only after a fully successful copy, and a folder that is already a symlink
/// to somewhere else is never touched. `foreign`: the partition can't hold Unix owners, so
/// only timestamps are preserved.
pub fn migrate_one(mount: &Path, home: &Path, dir: &str, uid: u32, gid: u32, foreign: bool, cp_output: impl Fn() -> std::process::Stdio) -> Result<Moved> {
    let target = mount.join(dir);
    let home_dir = home.join(dir);
    let state = home_state(&home_dir);
    match &state {
        HomeState::Link(t) if *t == target => return Ok(Moved::AlreadyLinked),
        HomeState::Link(t) => return Ok(Moved::Kept(format!("~/{dir} is already a link to {}", t.display()))),
        HomeState::File => return Ok(Moved::Kept(format!("~/{dir} is a file, not a folder"))),
        HomeState::Dir | HomeState::Absent => {}
    }
    let made = !target.exists();
    fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    if made {
        ctx::chown_path(&target, uid, gid);
    }
    if state == HomeState::Dir {
        let preserve = if foreign { "--preserve=timestamps" } else { "--preserve=mode,ownership,timestamps,links" };
        let mut cp = Command::new("cp");
        cp.args(["-RT", preserve]).arg(&home_dir).arg(&target).stdin(std::process::Stdio::null()).stdout(cp_output()).stderr(cp_output());
        if !crate::report::run_tracked(&mut cp).is_ok_and(|s| s.success()) {
            return Ok(Moved::Failed);
        }
        fs::remove_dir_all(&home_dir).with_context(|| format!("removing {}", home_dir.display()))?;
    }
    symlink(&target, &home_dir).with_context(|| format!("linking {}", home_dir.display()))?;
    ctx::lchown_path(&home_dir, uid, gid);
    Ok(Moved::Linked)
}

// --- State 3: kud3n's repository ----------------------------------------------------

/// New pacman.conf content with the [archrepo] block present, pointing at REPO_URL. The block
/// is written with `SigLevel = Optional TrustAll`. A `SigLevel = Required` that an older
/// version wrote is upgraded to that; any other SigLevel the user set is kept. The block sits
/// before every other repository (pacman takes a package from the first repo that has it), so
/// one an older version appended at the end is moved up.
pub fn patch_pacman_conf(content: &str) -> String {
    let patched = if content.contains("[archrepo]") {
        upgrade_repo_block(content)
    } else {
        format!("{content}\n[archrepo]\nSigLevel = Optional TrustAll\nServer = {REPO_URL}\n")
    };
    repo_first(&patched)
}

/// Move the [archrepo] block above the first repository section (after `[options]`).
fn repo_first(content: &str) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let header = |l: &str| {
        let t = l.trim();
        t.starts_with('[') && t.ends_with(']')
    };
    let Some(start) = lines.iter().position(|l| l.trim() == "[archrepo]") else { return content.to_string() };
    let end = lines[start + 1..].iter().position(|l| header(l)).map_or(lines.len(), |i| start + 1 + i);
    let Some(first) = lines.iter().position(|l| header(l) && l.trim() != "[options]") else { return content.to_string() };
    if first == start {
        return content.to_string();
    }
    let mut block: Vec<String> = lines[start..end].iter().map(|l| l.to_string()).collect();
    if !block.last().is_some_and(|l| l.ends_with('\n')) {
        block.last_mut().unwrap().push('\n');
    }
    if !block.last().is_some_and(|l| l.trim().is_empty()) {
        block.push("\n".into());
    }
    let rest: Vec<&str> = lines[..start].iter().chain(&lines[end..]).copied().collect();
    let at = rest.iter().position(|l| header(l) && l.trim() != "[options]").unwrap_or(rest.len());
    let mut out: String = rest[..at].concat();
    out.push_str(&block.concat());
    out.push_str(&rest[at..].concat());
    // the block that was last leaves its separator blank line behind
    out.trim_end_matches('\n').to_string() + "\n"
}

fn upgrade_repo_block(content: &str) -> String {
    let mut in_repo = false;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            in_repo = t == "[archrepo]";
        }
        if in_repo && t.split_once('=').is_some_and(|(k, v)| k.trim() == "SigLevel" && v.split_whitespace().eq(["Required"])) {
            out.push_str(&line.replace(t, "SigLevel = Optional TrustAll"));
        } else if in_repo && t == format!("Server = {OLD_REPO_URL}") {
            out.push_str(&line.replace(t, &format!("Server = {REPO_URL}")));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Import archrepo's signing key into pacman's keyring and sign it locally, so pacman
/// trusts the repo's signed database and packages. Safe to repeat.
fn import_repo_key() -> Result<()> {
    if !Path::new("/etc/pacman.d/gnupg/pubring.gpg").exists() && !ok_logged("pacman-key", &["--init"]) {
        anyhow::bail!("pacman-key --init failed");
    }
    let mut key = tempfile::NamedTempFile::new().context("creating a temp file for the archrepo key")?;
    std::io::Write::write_all(&mut key, REPO_KEY.as_bytes()).context("writing the archrepo key")?;
    let path = key.path().to_string_lossy().into_owned();
    if !ok_logged("pacman-key", &["--add", &path]) {
        anyhow::bail!("pacman-key --add failed");
    }
    if !ok_logged("pacman-key", &["--lsign-key", REPO_KEY_FPR]) {
        anyhow::bail!("pacman-key --lsign-key {REPO_KEY_FPR} failed");
    }
    Ok(())
}

/// Add or update [archrepo] in /etc/pacman.conf (recorded for undo). Whether it was there.
fn configure_pacman_conf() -> Result<bool> {
    let path = sys_path("/etc/pacman.conf");
    let conf = fs::read_to_string(&path).context("reading /etc/pacman.conf")?;
    let new = patch_pacman_conf(&conf);
    if new != conf {
        journal::write_file(&path, new.as_bytes(), None).context("writing /etc/pacman.conf")?;
    }
    Ok(conf.contains("[archrepo]"))
}

pub fn add_kud3n_repo(auto: bool, o_start: i64, o_step: i64, o_total: i64) -> Result<()> {
    let d = Dash::new(auto, o_start, o_step, o_total, "Add kud3n's Repository");
    let s_total = 3;
    d.step("Repo", 0, s_total, "Inspecting /etc/pacman.conf", "Reading repository definitions...");

    d.step("Repo", 1, s_total, "Updating pacman.conf", "Configuring [archrepo]...");
    let key_present = ok("pacman-key", &["--list-keys", REPO_KEY_FPR]);
    match journal::pacman_key(REPO_KEY_FPR, key_present, import_repo_key) {
        Ok(()) => log("INFO", &format!("Imported and locally signed the archrepo key {REPO_KEY_FPR}")),
        Err(e) => {
            log("WARN", &format!("Could not import the archrepo signing key: {e:#}"));
            add_issue("archrepo signing key could not be imported");
        }
    }
    let had_repo = configure_pacman_conf()?;
    if had_repo {
        log("INFO", "Updated existing [archrepo] in /etc/pacman.conf");
    } else {
        log("INFO", "Appended new [archrepo] block to /etc/pacman.conf");
    }

    // Sync + upgrade together: a bare -Syy followed by later `pacman -S <pkg>` is a partial upgrade
    d.step("Repo", 2, s_total, "Syncing Pacman", "Running pacman -Syyu...");
    if d.exec_monitored(2, s_total, "Syncing databases & upgrading (pacman -Syyu)", pacman(&["-Syyu", "--noconfirm"])) != 0 {
        log("WARN", &format!("pacman -Syyu finished with warnings. See {}", ctx().log_file.display()));
        add_issue("Repository sync (pacman -Syyu) failed");
    }

    d.step("Repo", 3, s_total, "Verifying", "Checking archrepo package list...");
    if ok("pacman", &["-Sl", "archrepo"]) {
        log("INFO", "Repository archrepo verified successfully.");
    } else {
        log("WARN", "Could not verify archrepo packages. Check internet connection.");
        add_issue("[archrepo] repository could not be verified");
    }
    ui::sleep_secs(1.0);
    Ok(())
}

// --- State 4: Packages ----------------------------------------------------------------

/// `pacman -Si` output in the C locale, so the field labels are the English ones.
fn sync_info(pkg: &str) -> Option<String> {
    let out = pacman(&["-Si", pkg]).env("LC_ALL", "C").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether `pacman -Si` output lists `legacy` under "Conflicts With" (version constraints ignored).
fn conflicts_with(si: &str, legacy: &str) -> bool {
    si.lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim() == "Conflicts With")
        .any(|(_, v)| v.split_whitespace().any(|c| c.split(['<', '>', '=']).next() == Some(legacy)))
}

/// The installed package names (`pacman -Qq`).
fn installed_names() -> Vec<String> {
    output("pacman", &["-Qq"]).map(|o| o.lines().map(str::to_string).collect()).unwrap_or_default()
}

/// Version and repository of an installed package, for the journal.
fn pkg_origin(name: &str) -> (String, String) {
    let field = |out: Option<String>, key: &str| {
        out.and_then(|o| o.lines().filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()))
    };
    let qi = pacman(&["-Qi", name]).env("LC_ALL", "C").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
    let version = field(qi.filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()), "Version").unwrap_or_default();
    let repo = field(sync_info(name), "Repository").unwrap_or_else(|| "local".into());
    (version, repo)
}

/// The installed set before a transaction, when the run is recorded.
fn before_install() -> Option<Vec<String>> {
    journal::current().map(|_| installed_names())
}

/// Record what a transaction installed that wasn't there before.
fn record_installed(before: Option<Vec<String>>, targets: &[&str]) {
    if let Some(before) = before {
        journal::pkg_installs(&journal::new_packages(&before, &installed_names(), targets, pkg_origin));
    }
}

/// Install one package with pacman (recorded for undo like any package install).
pub fn install_one(pkg: &str, title: &str) -> Result<()> {
    let d = Dash::new(false, 0, 1, 1, title);
    d.step("Install", 1, 1, pkg, &format!("Installing {pkg}..."));
    let before = before_install();
    let rc = d.exec_monitored(1, 1, &format!("Installing {pkg} (pacman)"), pacman(&["-S", "--needed", "--noconfirm", pkg]));
    record_installed(before, &[pkg]);
    if rc != 0 {
        anyhow::bail!("pacman could not install {pkg} (exit {rc})");
    }
    log("SUCCESS", &format!("{pkg} installed"));
    Ok(())
}

pub fn pkg_installed(pkg: &str) -> bool {
    match pkg.split_once('/') {
        Some((repo, name)) => output("pacman", &["-Sl", repo]).is_some_and(|list| {
            list.lines().any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() >= 2 && f[0] == repo && f[1] == name && l.ends_with("[installed]")
            })
        }),
        None => ok("pacman", &["-Q", pkg]),
    }
}

/// Whether /etc/pacman.conf has an `[archrepo]` section.
pub fn repo_configured() -> bool {
    fs::read_to_string(sys_path("/etc/pacman.conf")).is_ok_and(|c| c.lines().any(|l| l.trim() == "[archrepo]"))
}

/// The packages that can be installed: the ones discovered in archrepo's database need
/// `[archrepo]` in pacman.conf (`repo_ready`: it is there, or this run adds it first).
pub fn installable(list: &Parsed, repo_ready: bool) -> std::borrow::Cow<'_, Parsed> {
    if repo_ready || list.discovered() == 0 {
        std::borrow::Cow::Borrowed(list)
    } else {
        log("WARN", &format!("[archrepo] isn't in pacman.conf: leaving out {} packages found in its database.", list.discovered()));
        std::borrow::Cow::Owned(list.without_discovered())
    }
}

/// Install the list's packages in order; `selection` (package names) narrows it, None = all.
pub fn install_packages(list: &Parsed, selection: Option<&[String]>, auto: bool, o_start: i64, o_step: i64, o_total: i64) {
    let c = ctx();
    let d = Dash::new(auto, o_start, o_step, o_total, "Install Packages");
    let all: Vec<&Pkg> = list.packages().filter(|p| selection.is_none_or(|s| s.contains(&p.name))).collect();
    let s_total = all.len() as i64;
    log("INFO", &format!("Starting installation of {s_total} packages..."));

    for (i, p) in all.iter().enumerate() {
        let idx = i as i64 + 1;
        let pkg = p.name.as_str();

        // Replace a conflicting legacy/bin package, but only when the replacement is
        // actually available, otherwise the app would just disappear.
        // The swap happens in the install transaction itself (pacman removes the conflicting
        // package there), so a failed or canceled install leaves the legacy package in place.
        let mut swap = false;
        if let Some(legacy) = p.replaces.as_deref().filter(|l| ok("pacman", &["-Qq", l])) {
            match sync_info(pkg) {
                Some(info) if conflicts_with(&info, legacy) => {
                    log("INFO", &format!("Detected {legacy}; replacing it with {pkg}..."));
                    swap = true;
                }
                Some(_) => log("WARN", &format!("{pkg} does not declare a conflict with {legacy}; keeping installed {legacy}.")),
                None => log("WARN", &format!("{pkg} is not available in the sync databases; keeping installed {legacy}.")),
            }
        }

        if pkg_installed(pkg) {
            log("INFO", &format!("Package {pkg} already installed."));
            d.render(idx, s_total, &format!("{pkg} (Already installed)"), &format!("[UP-TO-DATE] {pkg}"), Some("✔"));
            continue;
        }

        let install = match p.fallback.as_deref() {
            Some(f) if !ok("pacman", &["-Si", pkg]) && ok("pacman", &["-Si", f]) => f,
            _ => pkg,
        };

        // --ask=4 answers yes to "remove the conflicting package?", which --noconfirm declines
        let args: &[&str] = if swap { &["-S", "--needed", "--noconfirm", "--ask=4", install] } else { &["-S", "--needed", "--noconfirm", install] };
        let before = before_install();
        let legacy_version =
            p.replaces.as_deref().filter(|_| swap).and_then(|l| output("pacman", &["-Q", l])).and_then(|o| o.split_whitespace().nth(1).map(String::from));
        let mut rc = d.exec_monitored(idx - 1, s_total, &format!("Installing {install} (pacman)"), pacman(args));
        let (mut status, mut mark) = (format!("[SUCCESS] {install} installed"), "✔");
        if rc != 0 {
            // A downloaded list can name any package: only the built-in list's may be built from the AUR
            let aur_ok = crate::pkglist::builtin().allows_aur(install);
            if !aur_ok && c.target_user != "root" && have("yay") {
                log("WARN", &format!("pacman failed for {install}; not building it with yay: it isn't in kast's built-in package list."));
            }
            if aur_ok && c.target_user != "root" && have("yay") && ctx::grant_yay_sudo() {
                log("WARN", &format!("pacman failed for {install}, attempting fallback with yay as {}...", c.target_user));
                let yay =
                    run_as_user(&["yay", "-S", "--needed", "--noconfirm", "--answerclean", "None", "--answerdiff", "None", "--answeredit", "None", install]);
                rc = d.exec_monitored(idx - 1, s_total, &format!("Building {install} (yay)"), yay);
                ctx::revoke_yay_sudo();
                if rc == 0 {
                    log("INFO", &format!("Successfully installed {install} via yay"));
                    status = format!("[SUCCESS] {install} installed (yay)");
                } else {
                    log("ERROR", &format!("Failed to install {install} via yay as well."));
                }
            } else {
                log("ERROR", &format!("Failed to install {install}. Recorded in {}", c.log_file.display()));
            }
            if rc != 0 {
                add_issue(format!("Package failed to install: {install}"));
                status = format!("[FAILED] {install} (see log)");
                mark = "✘";
            }
        }
        // What this transaction (pacman, or yay) added, and the legacy package it swapped out
        record_installed(before, &[install]);
        if let (Some(legacy), Some(v)) = (p.replaces.as_deref(), legacy_version.as_deref())
            && !ok("pacman", &["-Qq", legacy])
        {
            journal::pkg_replace(legacy, v, pkg);
        }
        d.render(idx, s_total, install, &status, Some(mark));
    }
    log("INFO", "Completed package installation phase.");
    ui::sleep_secs(1.0);
}

// --- State 5: Services ---------------------------------------------------------------

/// Insert the Howdy rule after the `#%PAM-1.0` header (or at the top), unless a file mentions
/// pam_howdy.so already. With `emitter` (a whole PAM line), also make it the one emitter line,
/// directly above the first pam_howdy.so rule: older kast lines and hand-written `ir-msxu`
/// lines go. Every other byte stays. None when nothing changes.
pub fn patch_pam_content(content: &str, emitter: Option<&str>) -> Option<String> {
    let with_howdy = if content.contains("pam_howdy.so") {
        content.to_string()
    } else if !content.lines().any(|l| l.starts_with("#%PAM-1.0")) {
        format!("{PAM_RULE}\n{content}")
    } else {
        let mut res = String::with_capacity(content.len() + PAM_RULE.len() + 1);
        for line in content.split_inclusive('\n') {
            res.push_str(line);
            if line.starts_with("#%PAM-1.0") {
                if !line.ends_with('\n') {
                    res.push('\n');
                }
                res.push_str(PAM_RULE);
                res.push('\n');
            }
        }
        res
    };
    let is_howdy = |l: &str| !l.trim_start().starts_with('#') && l.split_whitespace().any(|w| w == "pam_howdy.so" || w.ends_with("/pam_howdy.so"));
    let res = match emitter {
        Some(rule) if with_howdy.lines().any(is_howdy) => {
            let mut res = String::with_capacity(with_howdy.len() + rule.len() + 1);
            let mut placed = false;
            for line in with_howdy.split_inclusive('\n') {
                if ir::emitter_line(line).is_some() {
                    continue;
                }
                if !placed && is_howdy(line) {
                    res.push_str(rule);
                    res.push('\n');
                    placed = true;
                }
                res.push_str(line);
            }
            res
        }
        _ => with_howdy,
    };
    (res != content).then_some(res)
}

/// The PAM line that switches the IR emitter on, or why kast can't add it. PAM runs `kast` as
/// root, so it and every folder above it must be root's and writable by no one else.
pub fn emitter_rule(kast: &Path, device: &str) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let exe = fs::canonicalize(kast).map_err(|e| format!("{}: {e}", kast.display()))?;
    for p in exe.ancestors() {
        let m = fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if m.uid() != 0 {
            return Err(format!("{} is owned by uid {}, not root", p.display(), m.uid()));
        }
        if m.mode() & 0o022 != 0 {
            return Err(format!("{} can be written by other users (mode {:o})", p.display(), m.mode() & 0o777));
        }
    }
    let exe = exe.display().to_string();
    if exe.contains(char::is_whitespace) || device.contains(char::is_whitespace) || device.is_empty() {
        return Err(format!("a PAM argument can't contain spaces ({exe}, {device})"));
    }
    Ok(format!("{EMITTER_RULE} {exe} ir-emitter enable --device {device}"))
}

/// Add the Howdy rule (and `emitter`, see patch_pam_content) to a PAM file (recorded for
/// undo). A file only the distribution's `fallback` has is created from it.
fn patch_pam(pfile: &str, fallback: Option<&str>, emitter: Option<&str>) -> Result<()> {
    let p = sys_path(pfile);
    let content = match fs::read_to_string(&p) {
        Ok(c) => Some(c),
        Err(_) => fallback
            .map(sys_path)
            .filter(|fb| fb.is_file())
            .map(|fb| fs::read_to_string(&fb).with_context(|| format!("reading {}", fb.display())))
            .transpose()?,
    };
    let Some(content) = content else { return Ok(()) };
    if !content.contains("pam_howdy.so") && includes_howdy(&content, 0) {
        log("INFO", &format!("{pfile} already reaches Howdy through an include; left alone"));
        return Ok(());
    }
    if let Some(new) = patch_pam_content(&content, emitter) {
        if p.is_file() {
            fs::copy(&p, format!("{}.bak.{}", p.display(), ctx::now_str("%Y%m%d%H%M%S")))?;
        }
        journal::write_file(&p, new.as_bytes(), Some(0o644)).with_context(|| format!("writing {pfile}"))?;
        log("INFO", &format!("Patched PAM file {pfile} for Howdy{}", if emitter.is_some() { " and the IR emitter" } else { "" }));
    }
    Ok(())
}

/// Whether an auth include (or substack) of this PAM text leads to a file with pam_howdy.so,
/// looked up like libpam does: /etc/pam.d first, then the distribution's /usr/lib/pam.d.
/// Patching such a file too would run Howdy twice (KDE's lock screen, `kde`, includes
/// system-local-login).
fn includes_howdy(content: &str, depth: u8) -> bool {
    depth < 8
        && content.lines().filter_map(|l| crate::undo::pam_line(l).ok().flatten()).any(|(ty, control, module)| {
            let include = (ty == "auth" && matches!(control.as_str(), "include" | "substack")) || ty == "@include";
            include
                && !module.contains('/')
                && ["/etc/pam.d", "/usr/lib/pam.d"].iter().find_map(|d| fs::read_to_string(sys_path(d).join(&module)).ok()).is_some_and(|text| {
                    text.lines().any(|l| crate::undo::pam_line(l).ok().flatten().is_some_and(|(_, _, m)| m == "pam_howdy.so" || m.ends_with("/pam_howdy.so")))
                        || includes_howdy(&text, depth + 1)
                })
        })
}

/// Howdy's config folder and file as Howdy installs them (0750, 0640). When other users can
/// read config.ini, pam_howdy reads it directly instead of through its setuid helper, and a
/// lock screen (which authenticates as the user) then can't open the root-only face model.
const HOWDY_CONFIG: [&str; 2] = ["/etc/howdy", "/etc/howdy/config.ini"];

/// The Howdy config paths other users can get at: their mode, and the one that closes them
/// (the other users' bits off, and the group's too when the group isn't root's).
pub fn howdy_config_open() -> Vec<(PathBuf, u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    HOWDY_CONFIG
        .iter()
        .map(sys_path)
        .filter_map(|p| fs::symlink_metadata(&p).ok().filter(|m| !m.file_type().is_symlink()).map(|m| (p, m.mode() & 0o7777, m.gid())))
        .map(|(p, mode, gid)| (p, mode, mode & if gid == 0 { !0o007 } else { !0o077 }))
        .filter(|(_, mode, after)| mode != after)
        .collect()
}

/// Close Howdy's config folder and file to other users (recorded for undo).
fn lock_howdy_config() {
    for (p, mode, after) in howdy_config_open() {
        let done = journal::mode_change(&p, mode, after, || fs::set_permissions(&p, fs::Permissions::from_mode(after)).is_ok());
        if done {
            log("INFO", &format!("{}: mode {mode:04o} -> {after:04o} (lock screens use Howdy's helper)", p.display()));
        } else {
            add_issue(format!("Could not change the mode of {} to {after:04o}; lock screens can't use Howdy", p.display()));
        }
    }
}

/// The resume hook that reloads the IR camera after sleep (recorded for undo).
fn write_hook(device: &str) -> Result<()> {
    let hook = sys_path(HOWDY_HOOK);
    journal::write_file(&hook, howdy_hook(device).as_bytes(), Some(0o755)).context("writing Howdy resume hook")?;
    let mode = fs::metadata(&hook)?.permissions().mode() | 0o111;
    fs::set_permissions(&hook, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Howdy's dark_threshold is raised to at least this (a higher one is kept).
const DARK_THRESHOLD: u32 = 90;

/// Whether `current` (missing, not a number, or lower) should be raised to `min`.
fn needs_raise(current: Option<&str>, min: u32) -> bool {
    current.and_then(|v| v.parse::<f64>().ok()).is_none_or(|v| v < f64::from(min))
}

/// `key = value` in an ini file (Howdy's config.ini), any section.
pub fn ini_value(ini: &str, key: &str) -> Option<String> {
    ini.lines().filter_map(|l| l.split_once('=')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string())
}

pub fn howdy_hook(device: &str) -> String {
    format!(
        r#"#!/bin/sh
case $1/$2 in
  pre/*)
    /usr/bin/modprobe -r uvcvideo 2>/dev/null
    ;;
  post/*)
    /usr/bin/modprobe uvcvideo 2>/dev/null
    /usr/bin/udevadm settle --timeout=5 2>/dev/null
    count=0
    while [ ! -c {device} ] && [ $count -lt 30 ]; do
        sleep 0.1
        count=$((count + 1))
    done
    sleep 0.5
    ;;
esac
"#
    )
}

/// First /dev/video* node offering an IR format (GREY / Y16), else /dev/video2.
fn find_ir_camera() -> String {
    let mut devs: Vec<PathBuf> = fs::read_dir("/dev")
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("video"))).collect())
        .unwrap_or_default();
    devs.sort();
    devs.iter()
        .find(|dev| {
            output("v4l2-ctl", &[&format!("--device={}", dev.display()), "--list-formats"]).is_some_and(|o| o.contains("'GREY'") || o.contains("'Y16 '"))
        })
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "/dev/video2".into())
}

/// How the Services step left the IR emitter (the summary's line), and whether the camera has
/// one kast can switch on.
static IR_OUTCOME: std::sync::Mutex<Option<(String, bool)>> = std::sync::Mutex::new(None);

fn ir_outcome(line: String, emitter: bool) {
    log("INFO", &format!("IR emitter: {line}"));
    *IR_OUTCOME.lock().unwrap_or_else(|e| e.into_inner()) = Some((line, emitter));
}

fn take_ir_outcome() -> Option<(String, bool)> {
    IR_OUTCOME.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// `kast services` has no summary: what to know about the IR emitter and the face model.
pub fn services_footer() {
    let c = ctx();
    let Some((line, emitter)) = take_ir_outcome() else { return };
    println!("IR emitter: {line}");
    if Path::new(&format!("/etc/howdy/models/{}.dat", c.target_user)).is_file() {
        if emitter {
            println!("{}", ir::reenroll_note());
        }
    } else if Path::new("/usr/share/howdy").is_dir() {
        println!("No face model enrolled yet. Run: {}", ir::enroll_command(emitter));
    }
}

pub fn setup_services(auto: bool, o_start: i64, o_step: i64, o_total: i64) -> Result<()> {
    let c = ctx();
    let d = Dash::new(auto, o_start, o_step, o_total, "Setup Services");
    let s_total = 4;

    // ProtonVPN and Fcitx5: packages only, no env/autostart files
    d.step("Services", 1, s_total, "ProtonVPN", &format!("Installing ProtonVPN CLI package for {}...", c.target_user));
    let before = before_install();
    d.exec_monitored(1, s_total, "Installing proton-vpn-cli", pacman(&["-S", "--needed", "--noconfirm", "proton-vpn-cli"]));
    record_installed(before, &["proton-vpn-cli"]);

    d.step("Services", 2, s_total, "Vietnamese Typing (Fcitx5)", "Installing Fcitx5 packages (no environment file changes)...");
    let fcitx = ["fcitx5", "fcitx5-configtool", "fcitx5-gtk", "fcitx5-qt", "fcitx5-unikey"];
    let before = before_install();
    d.exec_monitored(2, s_total, "Installing Fcitx5 typing packages", pacman(&[&["-S", "--needed", "--noconfirm"][..], &fcitx[..]].concat()));
    record_installed(before, &fcitx);

    d.step("Services", 3, s_total, "Face Recognition (Howdy)", "Detecting sensor & IR emitter, models & PAM rules...");
    let before = before_install();
    d.exec_monitored(3, s_total, "Installing v4l-utils for camera detection", pacman(&["-S", "--needed", "--noconfirm", "v4l-utils"]));
    record_installed(before, &["v4l-utils"]);

    let node = find_ir_camera();
    log("INFO", &format!("Howdy device node resolved to {node}"));
    // Read-only: whether the camera can light its IR emitter, and the node's stable name
    let ir_check = ir::check(Path::new(&node));
    let device = ir_check.device.clone();
    log("INFO", &format!("IR emitter of {device}: {}", ir_check.verdict_text()));

    let models = Path::new("/usr/share/howdy/models");
    if Path::new("/usr/share/howdy").is_dir()
        && !models.join("face_detection_yunet_2026may.onnx").is_file()
        && !models.join("face_detection_yunet_2023mar.onnx").is_file()
    {
        let mut dl = Command::new("howdy");
        dl.arg("download-models");
        d.exec_monitored(3, s_total, "Downloading Howdy AI face models", dl);
    }
    if let Ok(ini) = fs::read_to_string("/etc/howdy/config.ini") {
        for (key, value) in [("device_path", device.as_str()), ("detection_notice", "true")] {
            journal::howdy_set(key, ini_value(&ini, key).as_deref(), value, || ok_logged("howdy", &["set", key, value]));
        }
        let dark = ini_value(&ini, "dark_threshold");
        if needs_raise(dark.as_deref(), DARK_THRESHOLD) {
            let value = DARK_THRESHOLD.to_string();
            journal::howdy_set("dark_threshold", dark.as_deref(), &value, || ok_logged("howdy", &["set", "dark_threshold", &value]));
        }
    }
    // After `howdy set`, which rewrites config.ini
    lock_howdy_config();

    // Referencing a PAM module that does not exist breaks the whole auth stack
    // (sudo/login fail with "Module is unknown"), so only patch PAM once Howdy is installed.
    let module =
        ["/usr/lib/security/pam_howdy.so", "/lib/security/pam_howdy.so", "/usr/lib64/security/pam_howdy.so"].into_iter().find(|m| Path::new(m).exists());
    if module.is_none() {
        log("WARN", "pam_howdy.so not found (howdy-next not installed?). Skipping PAM patching and resume hook.");
        add_issue("Howdy not installed; face authentication was not configured");
    } else {
        let rule = match &ir_check.verdict {
            ir::Verdict::Usable(_) => match std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| emitter_rule(&exe, &device)) {
                Ok(rule) => Some(rule),
                Err(why) => {
                    log("WARN", &format!("Not adding the IR emitter to PAM: {why}"));
                    add_issue(format!(
                        "IR emitter not added to PAM: {why}. PAM runs kast as root, so it has to be a root-owned file no one else \
                         can change: install kast to /usr/local/bin (install.sh) and run `sudo kast services` again"
                    ));
                    ir_outcome(format!("not set up: {why}"), true);
                    None
                }
            },
            ir::Verdict::NotSupported => {
                ir_outcome("not supported by this camera".into(), false);
                None
            }
            ir::Verdict::Unusable(why) => {
                ir_outcome(format!("not set up: {why}"), false);
                None
            }
        };
        patch_pam("/etc/pam.d/sudo", None, rule.as_deref())?;
        for name in ["polkit-1", "system-local-login", "plasmalogin", "sddm", "gdm-password", "hyprlock", "swaylock", "kde"] {
            patch_pam(&format!("/etc/pam.d/{name}"), Some(&format!("/usr/lib/pam.d/{name}")), rule.as_deref())?;
        }
        if let (Some(_), ir::Verdict::Usable(mode)) = (&rule, &ir_check.verdict) {
            ir_outcome(format!("switched on before Howdy ({})", mode.name()), true);
        }
        write_hook(&device)?;
    }
    if module.is_none() {
        ir_outcome("not set up: Howdy isn't installed".into(), ir_check.verdict.usable());
    }

    d.step("Services", 4, s_total, "Search Services", "Enabling and starting valkey & searxng...");
    for svc in ["valkey", "searxng"] {
        let (enabled, active) = (ok("systemctl", &["is-enabled", "--quiet", svc]), ok("systemctl", &["is-active", "--quiet", svc]));
        if !journal::service_enable(svc, enabled, active, || ok_logged("systemctl", &["enable", "--now", svc])) {
            add_issue(format!("Could not enable/start {svc}"));
        }
    }
    ui::sleep_secs(1.0);
    log("INFO", "Services setup completed.");
    Ok(())
}

// --- State 1: Automatic run -------------------------------------------------------------
// The data partition and the data import are optional and deliberately not part of it
// (menu options 2 and 6 / `kast partition`, `kast import`).

/// What the end of the automatic run reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub elapsed: String,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    /// None when the data partition is not set up
    pub partition: Option<String>,
    pub repo_configured: bool,
    pub log_file: String,
    /// Where the package list came from (`PkgList::source_line`), or "skipped"
    pub packages: String,
    /// Steps of the run left out (the app's step list); empty for `kast auto`
    pub skipped: Vec<Step>,
    /// How the import / export steps went, when they ran
    pub import: Option<String>,
    pub export: Option<String>,
    /// How to roll the upgrade back, when a snapshot was taken
    pub rollback: Option<String>,
    pub issues: Vec<String>,
    pub howdy_enrolled: bool,
    /// How the Services step left the IR emitter, when it ran
    pub ir_emitter: Option<String>,
    /// The camera has an IR emitter kast can switch on (face models are added with it on)
    pub ir_supported: bool,
}

/// `kast auto`: the default steps, with a snapshot before the upgrade unless `snapshot` is false.
pub fn run_automatic(list: &PkgList, snapshot: bool) -> Result<()> {
    let plan = AutoPlan { snapshot, ..AutoPlan::standard() };
    let out = run_plan(&plan, Some(list))?;
    ui::restore_terminal_interactive();
    ui::clear_screen();
    let mut s = collect_summary(out.start, Some(list));
    s.rollback = out.rollback;
    print_summary(&s);
    ui::read_choice("Press Enter to return to master menu...");
    Ok(())
}

/// A step the automatic run can include, in run order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// First: backups usually live on /media/data, and the other steps can use it
    Partition,
    /// Before anything below changes the apps or their settings
    Export,
    Upgrade,
    Repo,
    Packages,
    Services,
    /// Last: the apps are installed and /media/data is mounted by then
    Import,
}

impl Step {
    pub const ALL: [Step; 7] = [Step::Partition, Step::Export, Step::Upgrade, Step::Repo, Step::Packages, Step::Services, Step::Import];

    pub fn label(self) -> &'static str {
        match self {
            Step::Partition => "Data partition",
            Step::Export => "Export data",
            Step::Upgrade => "System upgrade",
            Step::Repo => "kud3n's repository",
            Step::Packages => "Install packages",
            Step::Services => "Services",
            Step::Import => "Import data",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Step::Partition => "partition",
            Step::Export => "export",
            Step::Upgrade => "upgrade",
            Step::Repo => "repo",
            Step::Packages => "packages",
            Step::Services => "services",
            Step::Import => "import",
        }
    }

    pub fn from_key(key: &str) -> Option<Step> {
        Step::ALL.into_iter().find(|s| s.key() == key)
    }

    /// Share of the overall progress bar (normalised over the steps that run). Upgrade,
    /// repo, packages and services keep the old 25 / 5 / 60 / 10.
    pub fn weight(self) -> i64 {
        match self {
            Step::Partition => 10,
            Step::Export => 15,
            Step::Upgrade => 25,
            Step::Repo => 5,
            Step::Packages => 60,
            Step::Services => 10,
            Step::Import => 15,
        }
    }

    /// Part of `kast auto` (the default plan)
    pub fn standard(self) -> bool {
        matches!(self, Step::Upgrade | Step::Repo | Step::Packages | Step::Services)
    }
}

/// What an automatic run does, all decided before it starts.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoPlan {
    /// In run order (`Step::ALL` order)
    pub steps: Vec<Step>,
    /// Package names to install; None = the whole list
    pub packages: Option<Vec<String>>,
    pub import: Option<crate::vault::import::ImportOpts>,
    pub export: Option<crate::vault::backup::BackupOpts>,
    /// The export goes to Proton Drive like this
    pub upload: Option<crate::drive::UploadOpts>,
    /// Take a snapshot before the upgrade (when the machine has a tool for it)
    pub snapshot: bool,
    /// The data partition step's decisions
    pub partition: Option<Box<crate::partition::apply::Plan>>,
}

impl AutoPlan {
    /// `kast auto`: upgrade, repo, packages, services.
    pub fn standard() -> Self {
        AutoPlan {
            steps: Step::ALL.into_iter().filter(|s| s.standard()).collect(),
            packages: None,
            import: None,
            export: None,
            upload: None,
            snapshot: false,
            partition: None,
        }
    }

    pub fn has(&self, s: Step) -> bool {
        self.steps.contains(&s)
    }

    /// Weights of the steps, for the overall progress bar.
    pub fn weights(&self) -> Vec<i64> {
        self.steps.iter().map(|s| s.weight()).collect()
    }
}

/// How the import and export steps of a run went, for the summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanOutcome {
    pub start: i64,
    pub import: Option<String>,
    pub export: Option<String>,
    /// How to roll the upgrade back (when a snapshot was taken)
    pub rollback: Option<String>,
    pub partition: Option<String>,
}

/// Run the plan's steps in order. A failing step records an issue and the run goes on,
/// except where a step itself stops the run (the repository step, as before).
pub fn run_plan(plan: &AutoPlan, list: Option<&PkgList>) -> Result<PlanOutcome> {
    let mut out = PlanOutcome { start: ui::now(), ..Default::default() };
    ui::set_step_weights(plan.weights());
    ctx::clear_issues();
    let keys: Vec<&str> = plan.steps.iter().map(|s| s.key()).collect();
    journal::recorded(&keys, false, || run_steps(plan, list, &mut out))?;
    Ok(out)
}

fn run_steps(plan: &AutoPlan, list: Option<&PkgList>, out: &mut PlanOutcome) -> Result<()> {
    let o_start = out.start;
    let o_total = plan.steps.len() as i64;
    log("INFO", "==========================================");
    log("INFO", "Starting Full Automated Setup Sequence...");
    log("INFO", "==========================================");

    for (i, step) in plan.steps.iter().enumerate() {
        let o_step = i as i64 + 1;
        journal::set_step(step.key());
        match step {
            Step::Partition => match &plan.partition {
                Some(p) => match setup_data_partition(p, true, o_start, o_step, o_total) {
                    Ok(r) => out.partition = Some(format!("{} mounted ({})", r.mountpoint, p.part.title())),
                    // Rolled back: the machine is as it was, and the rest of the run can go on
                    Err(e) => add_issue(format!("Data partition step failed and was rolled back: {e:#}")),
                },
                None => add_issue("Data partition step skipped: no partition was chosen"),
            },
            Step::Upgrade => out.rollback = crate::snapshot::rollback_hint(&system_upgrade(true, o_start, o_step, o_total, plan.snapshot)?),
            Step::Repo => add_kud3n_repo(true, o_start, o_step, o_total)?,
            Step::Packages => match list {
                Some(l) => {
                    // Repo comes before Packages in a run, so a run with it has the repo by then
                    let ready = plan.has(Step::Repo) || repo_configured();
                    install_packages(&installable(&l.list, ready), plan.packages.as_deref(), true, o_start, o_step, o_total)
                }
                None => add_issue("Packages step skipped: no package list"),
            },
            Step::Services => setup_services(true, o_start, o_step, o_total)?,
            Step::Export => out.export = plan.export.clone().map(|o| run_export(o, plan.upload.as_ref(), o_start, o_step, o_total)),
            Step::Import => out.import = plan.import.clone().map(|o| run_import(o, o_start, o_step, o_total)),
        }
    }
    Ok(())
}

/// The backup step of a run: what to show in the summary.
fn run_export(o: crate::vault::backup::BackupOpts, upload: Option<&crate::drive::UploadOpts>, o_start: i64, o_step: i64, o_total: i64) -> String {
    let d = Dash::new(true, o_start, o_step, o_total, "Export Data");
    if let Some(up) = upload {
        use crate::drive::Uploaded;
        d.render(0, 1, &format!("Backing up {} for Proton Drive", o.target), "", None);
        let result = match crate::drive::backup_upload(o, up) {
            Ok(Uploaded::Done { remote, identical, .. }) => format!("{} {remote}", if identical { "already on Proton Drive:" } else { "uploaded to" }),
            Ok(Uploaded::Unchanged { since }) => format!("unchanged since the last upload ({since}), not uploaded"),
            Ok(Uploaded::Nothing) => "nothing was backed up".to_string(),
            Ok(Uploaded::Canceled) => "canceled, nothing was uploaded".to_string(),
            Err(e) => {
                add_issue(format!("Upload to Proton Drive failed: {e}"));
                format!("failed: {e}")
            }
        };
        ctx::set_canceled(false);
        let ok = !result.starts_with("failed") && !result.starts_with("canceled");
        d.render(1, 1, &result, &result, Some(if ok { "✔" } else { "✘" }));
        return result;
    }
    let dest = o.output.clone().map(|p| p.display().to_string()).unwrap_or_default();
    d.render(0, 1, &format!("Backing up {}", o.target), &format!("Backing up {} to {dest}", o.target), None);
    let result = match crate::vault::backup::cmd_backup(o) {
        Ok(()) if ctx::canceled() => "canceled, no backup was written".to_string(),
        Ok(()) => format!("saved to {dest}"),
        Err(e) => {
            add_issue(format!("Backup failed: {e}"));
            format!("failed: {e}")
        }
    };
    // A declined "close the apps?" stops this step only
    ctx::set_canceled(false);
    d.render(1, 1, &result, &result, Some(if result.starts_with("saved") { "✔" } else { "✘" }));
    result
}

/// The restore step of a run: what to show in the summary.
fn run_import(o: crate::vault::import::ImportOpts, o_start: i64, o_step: i64, o_total: i64) -> String {
    let d = Dash::new(true, o_start, o_step, o_total, "Import Data");
    let file = o.file.display().to_string();
    let what = o.app.clone().unwrap_or_else(|| "everything".into());
    d.render(0, 1, &format!("Restoring {what}"), &format!("Restoring {what} from {file}"), None);
    let result = match crate::vault::import::cmd_import(o) {
        Ok(()) if ctx::canceled() => "canceled, nothing was restored".to_string(),
        Ok(()) => format!("{what} restored from {file}"),
        Err(e) => {
            add_issue(format!("Restore failed: {e}"));
            format!("failed: {e}")
        }
    };
    ctx::set_canceled(false);
    d.render(1, 1, &result, &result, Some(if result.contains("restored from") { "✔" } else { "✘" }));
    result
}

/// The summary of a run the app planned: what was skipped and how import/export went.
pub fn plan_summary(plan: &AutoPlan, out: &PlanOutcome, list: Option<&PkgList>) -> Summary {
    let mut s = collect_summary(out.start, list.filter(|_| plan.has(Step::Packages)));
    s.skipped = Step::ALL.into_iter().filter(|st| !plan.has(*st)).collect();
    s.import = out.import.clone();
    s.export = out.export.clone();
    s.rollback = out.rollback.clone();
    if plan.has(Step::Partition) {
        s.partition = out.partition.clone();
    }
    s
}

/// The state after the automatic run (takes the recorded issues).
pub fn collect_summary(o_start: i64, list: Option<&PkgList>) -> Summary {
    let c = ctx();
    let ir_outcome = take_ir_outcome();
    let partition = is_mounted(MOUNT_POINT).then(|| {
        let mut p = format!("{MOUNT_POINT} mounted");
        if c.target_home.join("Documents").is_symlink() {
            p += " & user folders symlinked";
        }
        p
    });
    Summary {
        elapsed: ui::format_seconds(ui::now() - o_start),
        user: c.target_user.clone(),
        uid: c.target_uid,
        gid: c.target_gid,
        partition,
        repo_configured: fs::read_to_string("/etc/pacman.conf").is_ok_and(|s| s.lines().any(|l| l.starts_with("[archrepo]"))),
        log_file: c.log_file.display().to_string(),
        packages: list.map_or_else(|| "skipped".to_string(), PkgList::source_line),
        skipped: Vec::new(),
        import: None,
        export: None,
        rollback: None,
        issues: ctx::take_issues(),
        howdy_enrolled: Path::new(&format!("/etc/howdy/models/{}.dat", c.target_user)).is_file(),
        ir_supported: ir_outcome.as_ref().map_or_else(ir::any_candidate, |o| o.1),
        ir_emitter: ir_outcome.map(|o| o.0),
    }
}

pub const PARTITION_HINT: &str = "not set up (menu option 2 or `sudo kast partition`)";

/// The summary's data partition line when it isn't mounted.
pub fn partition_hint() -> &'static str {
    PARTITION_HINT
}
pub const HOWDY_RESTORE_HINT: &str = "(or restore a saved face model with menu option 6 / `sudo kast import`)";

fn print_summary(s: &Summary) {
    let partition = s.partition.clone().unwrap_or_else(|| format!("{C_GRAY}{}{C_RESET}", partition_hint()));
    let repo = if s.repo_configured { "Configured in /etc/pacman.conf".to_string() } else { format!("{C_YELLOW}not configured{C_RESET}") };
    let issues = &s.issues;
    let rule = "=".repeat(80);
    if issues.is_empty() {
        println!("{C_GREEN}{C_BOLD}\n{rule}\n           🎉 AUTOMATED ARCH LINUX SETUP COMPLETED SUCCESSFULLY! 🎉           \n{rule}");
    } else {
        println!("{C_YELLOW}{C_BOLD}\n{rule}\n           ⚠  AUTOMATED SETUP FINISHED WITH {} ISSUE(S)  ⚠\n{rule}", issues.len());
    }
    println!("{C_RESET}");
    println!("  • Total Time Elapsed: {C_BOLD}{}{C_RESET}", s.elapsed);
    println!("  • Target User:        {C_BOLD}{}{C_RESET} (UID: {}, GID: {})", s.user, s.uid, s.gid);
    println!("  • Data Partition:     {partition}");
    println!("  • Kud3n Repo:         {repo}");
    println!("  • Log File:           {}", s.log_file);
    println!("  • Package List:       {}", s.packages);
    for (name, r) in [("Backup:      ", &s.export), ("Restore:     ", &s.import)] {
        if let Some(r) = r {
            println!("  • {name}       {r}");
        }
    }
    if !s.skipped.is_empty() {
        println!("  • Skipped Steps:      {}", s.skipped.iter().map(|st| st.label()).collect::<Vec<_>>().join(", "));
    }
    if let Some(r) = &s.rollback {
        println!("  • Upgrade Snapshot:   {r}");
    }
    if !issues.is_empty() {
        println!("\n{C_YELLOW}{C_BOLD}Issues:{C_RESET}");
        for issue in issues {
            println!("  {C_RED}✘{C_RESET} {issue}");
        }
    }
    println!();
    if let Some(ir) = &s.ir_emitter {
        println!("  • IR Emitter:         {ir}");
    }
    if s.howdy_enrolled {
        println!("  • Howdy Face Model:   {C_GREEN}Enrolled / Restored for {}{C_RESET}", s.user);
        if s.ir_supported {
            println!("{C_YELLOW}Note:{C_RESET} {}", ir::reenroll_note());
        }
    } else {
        println!("{C_YELLOW}Note:{C_RESET} If you have not enrolled your face in Howdy yet, run:");
        println!("  {}", ir::enroll_command(s.ir_supported));
        println!("  {HOWDY_RESTORE_HINT}");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacman_conf_block() {
        let added = patch_pacman_conf("[core]\nInclude = x\n");
        assert_eq!(added, format!("[archrepo]\nSigLevel = Optional TrustAll\nServer = {REPO_URL}\n\n[core]\nInclude = x\n"));
        assert_eq!(patch_pacman_conf(&added), added);
        // after [options] and its settings, before the first repository
        let opts = patch_pacman_conf("[options]\nParallelDownloads = 5\n\n[core]\nInclude = x\n\n[extra]\nInclude = y\n");
        assert_eq!(
            opts,
            format!(
                "[options]\nParallelDownloads = 5\n\n[archrepo]\nSigLevel = Optional TrustAll\nServer = {REPO_URL}\n\n[core]\nInclude = x\n\n[extra]\nInclude = y\n"
            )
        );
        assert_eq!(patch_pacman_conf(&opts), opts);
        let old = format!("[archrepo]\nServer = {OLD_REPO_URL}\n");
        assert_eq!(patch_pacman_conf(&old), format!("[archrepo]\nServer = {REPO_URL}\n"));
        // the SigLevel an older version wrote is upgraded, only inside [archrepo]
        let required = format!("[extra]\nSigLevel = Required\n\n[archrepo]\nSigLevel = Required\nServer = {REPO_URL}\n[x]\n");
        assert_eq!(
            patch_pacman_conf(&required),
            format!("[archrepo]\nSigLevel = Optional TrustAll\nServer = {REPO_URL}\n\n[extra]\nSigLevel = Required\n\n[x]\n")
        );
        // a block that already trusts is left as it is
        let trusting = format!("[archrepo]\nSigLevel = Optional  TrustAll\nServer = {REPO_URL}\n");
        assert_eq!(patch_pacman_conf(&trusting), trusting);
        // a SigLevel the user chose is left alone
        let custom = format!("[archrepo]\nSigLevel = Required DatabaseOptional\nServer = {REPO_URL}\n");
        assert_eq!(patch_pacman_conf(&custom), custom);
    }

    #[test]
    fn embedded_repo_key_is_armored() {
        assert!(REPO_KEY.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----"));
        assert_eq!(REPO_KEY_FPR.len(), 40);
    }

    #[test]
    fn pam_rule_placement() {
        assert_eq!(patch_pam_content("#%PAM-1.0\nauth include system-auth\n", None).unwrap(), format!("#%PAM-1.0\n{PAM_RULE}\nauth include system-auth\n"));
        assert_eq!(patch_pam_content("auth include x\n", None).unwrap(), format!("{PAM_RULE}\nauth include x\n"));
        assert_eq!(patch_pam_content(&format!("{PAM_RULE}\n"), None), None);
    }

    const BY_PATH: &str = "/dev/v4l/by-path/pci-0000:63:00.4-usb-0:1:1.2-video-index0";

    fn kast_rule(exe: &str) -> String {
        format!("{EMITTER_RULE} {exe} ir-emitter enable --device {BY_PATH}")
    }

    #[test]
    fn pam_emitter_line() {
        let rule = kast_rule("/usr/local/bin/kast");
        assert_eq!(rule, format!("auth      optional   pam_exec.so quiet /usr/local/bin/kast ir-emitter enable --device {BY_PATH}"));
        let once = |c: &str| patch_pam_content(c, Some(&rule)).unwrap_or_else(|| c.to_string());

        // /etc/pam.d/sudo as it is on the laptop: the hand-written ir-msxu line goes
        let laptop = format!(
            "#%PAM-1.0\nauth      optional   pam_exec.so log=/tmp/ir-pam.log /usr/local/bin/ir-msxu --device {BY_PATH} set --mode alternate --yes\n\
             {PAM_RULE}\nauth\t\tinclude\t\tsystem-auth\naccount\t\tinclude\t\tsystem-auth\nsession\t\tinclude\t\tsystem-auth\n\
             session\t\toptional\tpam_systemd.so class=none\n"
        );
        let want = format!(
            "#%PAM-1.0\n{rule}\n{PAM_RULE}\nauth\t\tinclude\t\tsystem-auth\naccount\t\tinclude\t\tsystem-auth\nsession\t\tinclude\t\tsystem-auth\n\
             session\t\toptional\tpam_systemd.so class=none\n"
        );
        assert_eq!(once(&laptop), want);
        assert_eq!(want.lines().filter(|l| ir::emitter_line(l).is_some()).count(), 1);
        // fresh, and patched by an older kast
        assert_eq!(once("#%PAM-1.0\nauth include system-auth\n"), format!("#%PAM-1.0\n{rule}\n{PAM_RULE}\nauth include system-auth\n"));
        assert_eq!(once(&format!("#%PAM-1.0\n{PAM_RULE}\nauth include system-auth\n")), format!("#%PAM-1.0\n{rule}\n{PAM_RULE}\nauth include system-auth\n"));
        assert_eq!(once("auth include x"), format!("{rule}\n{PAM_RULE}\nauth include x"));
        // a stale kast line (another path, another device, elsewhere in the file) is replaced
        let stale = format!(
            "#%PAM-1.0\n{PAM_RULE}\nauth optional pam_exec.so quiet /home/p3ter/.local/bin/kast ir-emitter enable --device /dev/video2\nauth include x\n"
        );
        assert_eq!(once(&stale), format!("#%PAM-1.0\n{rule}\n{PAM_RULE}\nauth include x\n"));
        // the second run changes nothing
        for c in [&laptop, &stale, &format!("{PAM_RULE}\n")] {
            assert_eq!(patch_pam_content(&once(c), Some(&rule)), None);
        }
        // comments are left alone; only a commented-out Howdy rule: no emitter line either
        let commented = format!("#%PAM-1.0\n# {rule}\n{PAM_RULE}\n");
        assert_eq!(once(&commented), format!("#%PAM-1.0\n# {rule}\n{rule}\n{PAM_RULE}\n"));
        assert_eq!(patch_pam_content(&format!("#{PAM_RULE}\nauth include x\n"), Some(&rule)), None);
        // without a rule to add, hand-written lines stay
        assert_eq!(patch_pam_content(&laptop, None), None);
    }

    #[test]
    fn emitter_rule_needs_a_root_owned_kast() {
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join("kast");
        fs::write(&exe, "").unwrap();
        // not root's, or (tests running as root) in a folder others can write
        assert!(emitter_rule(&exe, BY_PATH).is_err());
        let env = Path::new("/usr/bin/env");
        if fs::metadata(env).is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0) {
            let real = fs::canonicalize(env).unwrap().display().to_string();
            assert_eq!(emitter_rule(env, BY_PATH).unwrap(), kast_rule(&real));
            assert!(emitter_rule(env, "/dev/v4l/by-path/a b").is_err());
        }
    }

    #[test]
    fn hook_mentions_device() {
        let h = howdy_hook("/dev/video4");
        assert!(h.starts_with("#!/bin/sh\ncase $1/$2 in\n"));
        assert!(h.contains("while [ ! -c /dev/video4 ] && [ $count -lt 30 ]; do"));
    }

    #[test]
    fn conflict_parsed_from_sync_info() {
        let si = "Repository      : archrepo\nName            : sine\nProvides        : sine-bin\nConflicts With  : sine-bin>=1  fdm\nReplaces        : None\n";
        assert!(conflicts_with(si, "sine-bin"));
        assert!(conflicts_with(si, "fdm"));
        assert!(!conflicts_with(si, "sine"));
        assert!(!conflicts_with("Conflicts With  : None\n", "sine-bin"));
    }

    #[test]
    fn migration_moves_relinks_and_keeps_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let (mount, home) = (t.path().join("data"), t.path().join("home"));
        fs::create_dir_all(home.join("Documents/sub")).unwrap();
        fs::write(home.join("Documents/sub/a.txt"), "doc").unwrap();
        fs::create_dir_all(t.path().join("elsewhere")).unwrap();
        symlink(t.path().join("elsewhere"), home.join("Music")).unwrap();
        fs::create_dir_all(home.join("Videos")).unwrap();
        fs::write(home.join("Videos/v.mkv"), "v").unwrap();
        // Copy into a read-only destination fails
        fs::create_dir_all(mount.join("Videos")).unwrap();
        fs::set_permissions(mount.join("Videos"), fs::Permissions::from_mode(0o555)).unwrap();

        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let mut failed = Vec::new();
        for dir in ["Documents", "Music", "Videos", "Projects"] {
            if migrate_one(&mount, &home, dir, uid, gid, true, std::process::Stdio::null).unwrap() == Moved::Failed {
                failed.push((dir.to_string(), mount.join(dir).display().to_string()));
            }
        }
        fs::set_permissions(mount.join("Videos"), fs::Permissions::from_mode(0o755)).unwrap();

        // Real folder: copied, removed, symlinked
        assert_eq!(fs::read_link(home.join("Documents")).unwrap(), mount.join("Documents"));
        assert_eq!(fs::read_to_string(mount.join("Documents/sub/a.txt")).unwrap(), "doc");
        // A symlink to somewhere else is not touched
        assert_eq!(fs::read_link(home.join("Music")).unwrap(), t.path().join("elsewhere"));
        assert!(!mount.join("Music").exists());
        // Missing folder: just linked
        assert_eq!(fs::read_link(home.join("Projects")).unwrap(), mount.join("Projects"));
        // Failed copy: original kept, no symlink
        assert_eq!(failed, vec![("Videos".to_string(), mount.join("Videos").display().to_string())]);
        assert!(!home.join("Videos").is_symlink());
        assert_eq!(fs::read_to_string(home.join("Videos/v.mkv")).unwrap(), "v");
    }

    #[test]
    fn engine_steps_record_what_they_change() {
        use crate::journal::{self, Status, state_dir, sys_path, with_root};
        let t = tempfile::tempdir().unwrap();
        with_root(t.path(), || {
            fs::create_dir_all(sys_path("/etc/pam.d")).unwrap();
            fs::create_dir_all(sys_path("/usr/lib/pam.d")).unwrap();
            fs::write(sys_path("/etc/pacman.conf"), "[core]\nInclude = /etc/pacman.d/mirrorlist\n").unwrap();
            fs::write(sys_path("/etc/pam.d/sudo"), "#%PAM-1.0\nauth include system-auth\n").unwrap();
            fs::write(sys_path("/usr/lib/pam.d/sddm"), "#%PAM-1.0\nauth include system-login\n").unwrap();
            journal::begin_run(&["repo", "services"], "p3ter").unwrap();
            assert!(!configure_pacman_conf().unwrap());
            assert!(configure_pacman_conf().unwrap(), "the second time nothing changes");
            // a kast that isn't root-owned gets no emitter line
            let unsafe_rule = emitter_rule(&t.path().join("etc/pacman.conf"), "/dev/video2").ok();
            assert_eq!(unsafe_rule, None);
            patch_pam("/etc/pam.d/sudo", None, unsafe_rule.as_deref()).unwrap();
            assert_eq!(fs::read_to_string(sys_path("/etc/pam.d/sudo")).unwrap(), format!("#%PAM-1.0\n{PAM_RULE}\nauth include system-auth\n"));
            let rule = kast_rule("/usr/local/bin/kast");
            patch_pam("/etc/pam.d/sudo", None, Some(&rule)).unwrap();
            patch_pam("/etc/pam.d/sudo", None, Some(&rule)).unwrap();
            patch_pam("/etc/pam.d/sddm", Some("/usr/lib/pam.d/sddm"), Some(&rule)).unwrap();
            patch_pam("/etc/pam.d/missing", Some("/usr/lib/pam.d/missing"), Some(&rule)).unwrap();
            write_hook("/dev/video2").unwrap();
            journal::end_run();

            let runs = journal::runs(&journal::load(&state_dir()).unwrap().entries);
            let r = &runs[0];
            let changes: Vec<(String, String)> = r.changes.iter().map(|c| (c.kind.clone(), c.data["path"].as_str().unwrap_or_default().to_string())).collect();
            let p = |s: &str| sys_path(s).display().to_string();
            assert_eq!(
                changes,
                [
                    ("file_write".into(), p("/etc/pacman.conf")),
                    ("file_write".into(), p("/etc/pam.d/sudo")),
                    ("file_write".into(), p("/etc/pam.d/sudo")),
                    ("file_create".into(), p("/etc/pam.d/sddm")),
                    ("file_create".into(), p(HOWDY_HOOK)),
                ]
            );
            assert!(r.changes.iter().all(|c| c.status == Status::Done));
            assert!(fs::read_to_string(sys_path("/etc/pacman.conf")).unwrap().contains("[archrepo]"));
            assert!(fs::read_to_string(sys_path("/etc/pam.d/sddm")).unwrap().contains("pam_howdy.so"));
            assert_eq!(fs::read_to_string(sys_path("/etc/pam.d/sudo")).unwrap(), format!("#%PAM-1.0\n{rule}\n{PAM_RULE}\nauth include system-auth\n"));
            assert_eq!(fs::metadata(sys_path(HOWDY_HOOK)).unwrap().permissions().mode() & 0o111, 0o111);
        });
    }

    #[test]
    fn pam_file_reaching_howdy_by_include_is_left_alone() {
        use crate::journal::{sys_path, with_root};
        let t = tempfile::tempdir().unwrap();
        with_root(t.path(), || {
            fs::create_dir_all(sys_path("/etc/pam.d")).unwrap();
            fs::create_dir_all(sys_path("/usr/lib/pam.d")).unwrap();
            let rule = kast_rule("/usr/local/bin/kast");
            fs::write(sys_path("/etc/pam.d/system-local-login"), "#%PAM-1.0\nauth include system-login\n").unwrap();
            fs::write(sys_path("/usr/lib/pam.d/kde"), "#%PAM-1.0\n\nauth       include                     system-local-login\n").unwrap();
            fs::write(sys_path("/usr/lib/pam.d/kde-other"), "#%PAM-1.0\nauth include system-login\n").unwrap();
            assert!(!includes_howdy(&fs::read_to_string(sys_path("/usr/lib/pam.d/kde")).unwrap(), 0));
            patch_pam("/etc/pam.d/system-local-login", None, Some(&rule)).unwrap();
            // kde gets Howdy through system-local-login: a copy with its own rule would scan twice
            assert!(includes_howdy(&fs::read_to_string(sys_path("/usr/lib/pam.d/kde")).unwrap(), 0));
            patch_pam("/etc/pam.d/kde", Some("/usr/lib/pam.d/kde"), Some(&rule)).unwrap();
            assert!(!sys_path("/etc/pam.d/kde").exists());
            // one that doesn't reach it is patched as before
            patch_pam("/etc/pam.d/kde-other", Some("/usr/lib/pam.d/kde-other"), Some(&rule)).unwrap();
            assert!(fs::read_to_string(sys_path("/etc/pam.d/kde-other")).unwrap().contains("pam_howdy.so"));
            // a commented-out rule doesn't count
            fs::write(sys_path("/etc/pam.d/x"), format!("#{PAM_RULE}\n")).unwrap();
            assert!(!includes_howdy("auth include x\n", 0));
            // include loops end
            fs::write(sys_path("/etc/pam.d/a"), "auth include b\n").unwrap();
            fs::write(sys_path("/etc/pam.d/b"), "auth include a\n").unwrap();
            assert!(!includes_howdy("auth include a\n", 0));
        });
    }

    #[test]
    fn howdy_config_is_closed_to_other_users_and_undoable() {
        use crate::journal::{self, state_dir, sys_path, with_root};
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        with_root(t.path(), || {
            let (dir, ini) = (sys_path("/etc/howdy"), sys_path("/etc/howdy/config.ini"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(&ini, "[core]\n").unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&ini, fs::Permissions::from_mode(0o644)).unwrap();
            // the test's files belong to the user's group, not root's: the group bits go too
            let g = if fs::metadata(&dir).unwrap().gid() == 0 { (0o750, 0o640) } else { (0o700, 0o600) };
            assert_eq!(howdy_config_open(), [(dir.clone(), 0o755, g.0), (ini.clone(), 0o644, g.1)]);
            journal::begin_run(&["services"], "p3ter").unwrap();
            lock_howdy_config();
            lock_howdy_config();
            journal::end_run();
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
            assert_eq!((mode(&dir), mode(&ini)), g);
            assert!(howdy_config_open().is_empty());

            let runs = journal::runs(&journal::load(&state_dir()).unwrap().entries);
            let kinds: Vec<&str> = runs[0].changes.iter().map(|c| c.kind.as_str()).collect();
            assert_eq!(kinds, ["mode_change", "mode_change"], "the second time nothing changes");
            let items = crate::undo::plan(&runs[0], &crate::undo::View::default());
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert_eq!(labels, [format!("Give {} its mode 0644 back", ini.display()), format!("Give {} its mode 0755 back", dir.display())]);
            crate::undo::execute(&items, &state_dir(), "p3ter", &mut |_, _| {}).unwrap();
            assert_eq!((mode(&dir), mode(&ini)), (0o755, 0o644));
        });
    }

    #[test]
    fn howdy_values_are_read_for_the_journal() {
        let ini = "[core]\ndetection_notice = false\n[video]\ndevice_path = /dev/video0\n";
        assert_eq!(ini_value(ini, "device_path").as_deref(), Some("/dev/video0"));
        assert_eq!(ini_value(ini, "detection_notice").as_deref(), Some("false"));
        assert_eq!(ini_value(ini, "nope"), None);
        assert!(needs_raise(Some("75"), 90));
        assert!(needs_raise(None, 90));
        assert!(needs_raise(Some("x"), 90));
        assert!(!needs_raise(Some("90"), 90));
        assert!(!needs_raise(Some("95.5"), 90));
    }
}
