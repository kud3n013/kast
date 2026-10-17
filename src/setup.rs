//! System setup steps: upgrade, data partition, kud3n repo, packages, services, and the
//! automatic run that chains them.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use regex::Regex;

use crate::ctx::{self, add_issue, ctx, have, log, ok, ok_logged, output, run_as_user};
use crate::ui::{self, C_BOLD, C_GRAY, C_GREEN, C_RED, C_RESET, C_YELLOW, Dash};

pub const MOUNT_POINT: &str = "/media/data";
const DEFAULT_DATA_UUID: &str = "6C62ADEA62ADB96A";
pub const REPO_URL: &str = "https://github.com/kud3n013/archrepo/releases/download/packages";
const OLD_REPO_URL: &str = "https://kud3n013.github.io/archrepo/x86_64";
/// archrepo signs its database and packages with this key (see the archrepo README).
const REPO_KEY: &str = include_str!("../assets/archrepo.asc");
const REPO_KEY_FPR: &str = "CFB791543B4B2D2821E9127AC012A9476C170A37";
const PAM_RULE: &str = "auth      sufficient pam_howdy.so workaround=native";
const HOWDY_HOOK: &str = "/usr/lib/systemd/system-sleep/howdy-resume.sh";
const USER_DIRS: [&str; 9] = ["Desktop", "Documents", "Downloads", "Music", "Pictures", "Public", "Templates", "Videos", "Projects"];

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

pub fn system_upgrade(auto: bool, o_start: i64, o_step: i64, o_total: i64) {
    let title = "Systemwide Upgrade (pacman -Syuu)";
    let d = Dash::new(auto, o_start, o_step, o_total, title);
    log("INFO", "Starting systemwide upgrade...");
    let rc = d.exec_monitored(1, 2, "Refreshing databases & upgrading", pacman(&["-Syuu", "--noconfirm"]));
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
}

// --- State 2: Universal data partition --------------------------------------------------

pub fn fstab_line(uuid: &str, uid: u32, gid: u32) -> String {
    format!("UUID={uuid}                     {MOUNT_POINT}    ntfs3   defaults,nofail,uid={uid},gid={gid},rw,windows_names,iocharset=utf8,umask=000,exec 0 0")
}

fn mount_regex() -> Regex {
    Regex::new(&format!(r"[[:space:]]{}[[:space:]]", regex::escape(MOUNT_POINT))).unwrap()
}

/// New fstab content, or None when the existing entry is already right.
pub fn patch_fstab(content: &str, uuid: &str, uid: u32, gid: u32) -> Option<String> {
    let re = mount_regex();
    let line = fstab_line(uuid, uid, gid);
    match content.lines().rfind(|l| re.is_match(l)) {
        Some(cur) if cur.contains(uuid) && cur.contains("umask=000") && cur.contains(&format!("uid={uid}")) => None,
        Some(_) => {
            let mut res: String = content.lines().map(|l| if re.is_match(l) { line.as_str() } else { l }).collect::<Vec<_>>().join("\n");
            if content.ends_with('\n') {
                res.push('\n');
            }
            Some(res)
        }
        None => {
            let mut res = content.to_string();
            if !res.is_empty() && !res.ends_with('\n') {
                res.push('\n');
            }
            res += &line;
            res.push('\n');
            Some(res)
        }
    }
}

fn is_mounted(p: &str) -> bool {
    ok("findmnt", &[p])
}

pub fn setup_data_partition(auto: bool, o_start: i64, o_step: i64, o_total: i64) -> Result<()> {
    let c = ctx();
    let d = Dash::new(auto, o_start, o_step, o_total, "Setup Universal Data Partition");
    let s_total = 6;
    let step = |n: i64, item: &str, msg: &str| d.step("Partition", n, s_total, item, msg);

    step(0, "Initializing", "Verifying configuration...");
    let mut uuid = DEFAULT_DATA_UUID.to_string();
    if let Some(part) = output("blkid", &["-L", "Data"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        && let Some(u) = output("blkid", &["-s", "UUID", "-o", "value", &part]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    {
        uuid = u;
    }

    step(1, "Mount Directory", &format!("Ensuring {MOUNT_POINT} exists..."));
    fs::create_dir_all(MOUNT_POINT).with_context(|| format!("creating {MOUNT_POINT}"))?;
    ctx::chown_path(Path::new(MOUNT_POINT), c.target_uid, c.target_gid);

    step(2, "Fstab Entry", "Checking /etc/fstab configuration...");
    let fstab = fs::read_to_string("/etc/fstab").unwrap_or_default();
    match patch_fstab(&fstab, &uuid, c.target_uid, c.target_gid) {
        None => log("INFO", &format!("Fstab entry already up to date for {MOUNT_POINT}.")),
        Some(new) => {
            let backup = format!("/etc/fstab.bak.{}", ctx::now_str("%Y%m%d_%H%M%S"));
            fs::copy("/etc/fstab", &backup).context("backing up /etc/fstab")?;
            ctx::write_atomic(Path::new("/etc/fstab"), new.as_bytes()).context("writing /etc/fstab")?;
            log("INFO", &format!("Updated /etc/fstab entry for {MOUNT_POINT} (backup at {backup})."));
        }
    }

    step(3, "Mount Filesystem", "Verifying mount...");
    ok_logged("systemctl", &["daemon-reload"]);
    if !is_mounted(MOUNT_POINT) {
        let _ = ok_logged("mount", &["-a"]) || ok_logged("mount", &[MOUNT_POINT]);
    } else {
        ok_logged("mount", &["-o", "remount", MOUNT_POINT]);
    }
    if !is_mounted(MOUNT_POINT) {
        // Migrating now would move the user's files onto the root filesystem underneath
        // the mount point, where they get hidden as soon as the partition is mounted.
        log("ERROR", &format!("Mount point {MOUNT_POINT} is not mounted. Skipping user directory migration."));
        add_issue(format!("Data partition not mounted at {MOUNT_POINT}; user folders were not migrated"));
        step(6, "Aborted", &format!("{MOUNT_POINT} not mounted; user directories left untouched."));
        ui::sleep_secs(2.0);
        return Ok(());
    }
    // ntfs3 mounts read-only (or refuses rw) when Windows was hibernated / Fast Startup is on
    let probe = PathBuf::from(format!("{MOUNT_POINT}/.kast-write-test.{}", std::process::id()));
    if fs::write(&probe, b"").is_err() {
        log("ERROR", &format!("{MOUNT_POINT} is mounted but not writable (Windows Fast Startup/hibernation?). Skipping migration."));
        add_issue(format!("{MOUNT_POINT} is read-only (disable Windows Fast Startup, then re-run step 2)"));
        step(6, "Aborted", &format!("{MOUNT_POINT} is read-only; user directories left untouched."));
        ui::sleep_secs(2.0);
        return Ok(());
    }
    let _ = fs::remove_file(&probe);

    step(4, "Migrate User Directories", "Linking Desktop, Documents, Downloads, etc.");
    for (dir, dst) in migrate_user_dirs(Path::new(MOUNT_POINT), &c.target_home, &USER_DIRS, c.target_uid, c.target_gid, ctx::log_stdio)? {
        log("ERROR", &format!("Copying {} to {dst} failed; leaving it in place.", c.target_home.join(&dir).display()));
        add_issue(format!("Could not migrate ~/{dir} to {dst} (original kept)"));
    }

    step(5, "XDG User Dirs", &format!("Configuring user-dirs.dirs for {}...", c.target_user));
    let xdg_pairs = [
        ("DESKTOP", "Desktop"),
        ("DOCUMENTS", "Documents"),
        ("DOWNLOAD", "Downloads"),
        ("MUSIC", "Music"),
        ("PICTURES", "Pictures"),
        ("VIDEOS", "Videos"),
        ("PUBLICSHARE", "Public"),
        ("TEMPLATES", "Templates"),
    ];
    if have("xdg-user-dirs-update") {
        let mut args: Vec<String> = vec!["xdg-user-dirs-update".into()];
        for (key, dir) in xdg_pairs {
            args.extend(["--set".into(), key.into(), format!("{MOUNT_POINT}/{dir}")]);
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = run_as_user(&argv).stdout(ctx::log_stdio()).stderr(ctx::log_stdio()).status();
    }
    let cfg = c.target_home.join(".config/user-dirs.dirs");
    ctx::mkdir_user(cfg.parent().unwrap())?;
    let body: String = xdg_pairs.iter().map(|(key, dir)| format!("XDG_{key}_DIR=\"{MOUNT_POINT}/{dir}\"\n")).collect();
    fs::write(&cfg, body).with_context(|| format!("writing {}", cfg.display()))?;
    ctx::chown_path(&cfg, c.target_uid, c.target_gid);

    step(6, "Completed", "Data partition & directories configured successfully.");
    ui::sleep_secs(1.0);
    Ok(())
}

/// Move each home folder onto the data partition and leave a symlink in its place.
/// Returns (folder, destination) for every folder whose copy failed; those are left as-is.
pub fn migrate_user_dirs(
    mount: &Path,
    home: &Path,
    dirs: &[&str],
    uid: u32,
    gid: u32,
    cp_output: impl Fn() -> std::process::Stdio,
) -> Result<Vec<(String, String)>> {
    let owner = format!("{uid}:{gid}");
    let mut failed = Vec::new();
    for dir in dirs {
        let target = mount.join(dir);
        let home_dir = home.join(dir);
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
        ctx::chown_r(&target, &owner);

        let meta = home_dir.symlink_metadata();
        let is_link = meta.as_ref().is_ok_and(|m| m.file_type().is_symlink());
        if meta.is_ok() && !is_link {
            // NTFS can't hold Unix owners/modes, so only preserve timestamps; never delete
            // the original unless the copy fully succeeded.
            let mut cp = Command::new("cp");
            cp.args(["-RT", "--preserve=timestamps"]).arg(&home_dir).arg(&target).stdin(std::process::Stdio::null()).stdout(cp_output()).stderr(cp_output());
            let copied = crate::report::run_tracked(&mut cp).is_ok_and(|s| s.success());
            if !copied {
                failed.push((dir.to_string(), target.display().to_string()));
                continue;
            }
            if meta.as_ref().is_ok_and(|m| m.is_dir()) { fs::remove_dir_all(&home_dir) } else { fs::remove_file(&home_dir) }
                .with_context(|| format!("removing {}", home_dir.display()))?;
        } else if is_link {
            fs::remove_file(&home_dir).with_context(|| format!("replacing {}", home_dir.display()))?;
        }
        symlink(&target, &home_dir).with_context(|| format!("linking {}", home_dir.display()))?;
        ctx::lchown_path(&home_dir, uid, gid);
    }
    Ok(failed)
}

// --- State 3: kud3n's repository ----------------------------------------------------

/// New pacman.conf content with the [archrepo] block present, pointing at REPO_URL and
/// requiring signatures. Older versions wrote `SigLevel = Optional TrustAll`; that line is
/// upgraded, any other SigLevel the user set is kept.
pub fn patch_pacman_conf(content: &str) -> String {
    if !content.contains("[archrepo]") {
        return format!("{content}\n[archrepo]\nSigLevel = Required\nServer = {REPO_URL}\n");
    }
    let mut in_repo = false;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            in_repo = t == "[archrepo]";
        }
        if in_repo && t.split_once('=').is_some_and(|(k, v)| k.trim() == "SigLevel" && v.split_whitespace().eq(["Optional", "TrustAll"])) {
            out.push_str(&line.replace(t, "SigLevel = Required"));
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

pub fn add_kud3n_repo(auto: bool, o_start: i64, o_step: i64, o_total: i64) -> Result<()> {
    let d = Dash::new(auto, o_start, o_step, o_total, "Add kud3n's Repository");
    let s_total = 3;
    d.step("Repo", 0, s_total, "Inspecting /etc/pacman.conf", "Reading repository definitions...");

    d.step("Repo", 1, s_total, "Updating pacman.conf", "Configuring [archrepo]...");
    match import_repo_key() {
        Ok(()) => log("INFO", &format!("Imported and locally signed the archrepo key {REPO_KEY_FPR}")),
        Err(e) => {
            log("WARN", &format!("Could not import the archrepo signing key: {e:#}"));
            add_issue("archrepo signing key could not be imported");
        }
    }
    let conf = fs::read_to_string("/etc/pacman.conf").context("reading /etc/pacman.conf")?;
    let had_repo = conf.contains("[archrepo]");
    let new = patch_pacman_conf(&conf);
    if new != conf {
        ctx::write_atomic(Path::new("/etc/pacman.conf"), new.as_bytes()).context("writing /etc/pacman.conf")?;
    }
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

const PKGS_OFFICIAL: &[&str] = &[
    "zen-browser-bin",
    "zotero",
    "proton-vpn-cli",
    "inkscape",
    "chromium",
    "obsidian",
    "fcitx5",
    "fcitx5-configtool",
    "fcitx5-gtk",
    "fcitx5-qt",
    "fcitx5-unikey",
    "vesktop-bin",
    "valkey",
    "kdenlive",
    "steam",
    "zed-globalmenu",
    "wine",
    "wine-gecko",
    "wine-mono",
    "gwenview",
    "haruna",
    "libreoffice-fresh",
    "libreoffice-fresh-vi",
];
const PKGS_KUD3N: &[&str] = &["beeper", "howdy-next", "gdlauncher-carbon", "searxng", "archrepo/freedownloadmanager", "archrepo/sine"];
const PKGS_ADDONS: &[&str] =
    &["freedownloadmanager-elephant", "zotero-better-bibtex", "zotmoov", "zotero-better-notes", "zotero-ocr", "zotero-pdf-translate", "zotero-mcp"];

/// Ensure archrepo versions are used instead of legacy/AUR packages.
fn canonical_pkg(pkg: &str) -> &str {
    match pkg {
        "freedownloadmanager" => "archrepo/freedownloadmanager",
        "sine" | "sine-bin" => "archrepo/sine",
        "zed" => "zed-globalmenu",
        p => p,
    }
}

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

fn pkg_installed(pkg: &str) -> bool {
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

pub fn install_packages(auto: bool, o_start: i64, o_step: i64, o_total: i64) {
    let c = ctx();
    let d = Dash::new(auto, o_start, o_step, o_total, "Install Packages");
    let all: Vec<&str> = PKGS_OFFICIAL.iter().chain(PKGS_KUD3N).chain(PKGS_ADDONS).copied().collect();
    let s_total = all.len() as i64;
    log("INFO", &format!("Starting installation of {s_total} packages..."));

    for (i, raw) in all.iter().enumerate() {
        let idx = i as i64 + 1;
        let pkg = canonical_pkg(raw);
        let name = pkg.split_once('/').map_or(pkg, |(_, n)| n);

        // Replace a conflicting legacy/bin package, but only when the replacement is
        // actually available, otherwise the app would just disappear.
        let legacy = match name {
            "freedownloadmanager" => Some("freedownloadmanager-bin"),
            "sine" => Some("sine-bin"),
            "zed-globalmenu" => Some("zed"),
            _ => None,
        };
        // The swap happens in the install transaction itself (pacman removes the conflicting
        // package there), so a failed or canceled install leaves the legacy package in place.
        let mut swap = false;
        if let Some(legacy) = legacy.filter(|l| ok("pacman", &["-Qq", l])) {
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

        let install = if pkg == "vesktop-bin" && !ok("pacman", &["-Si", "vesktop-bin"]) && ok("pacman", &["-Si", "vesktop"]) { "vesktop" } else { pkg };

        // --ask=4 answers yes to "remove the conflicting package?", which --noconfirm declines
        let args: &[&str] = if swap { &["-S", "--needed", "--noconfirm", "--ask=4", install] } else { &["-S", "--needed", "--noconfirm", install] };
        let mut rc = d.exec_monitored(idx - 1, s_total, &format!("Installing {install} (pacman)"), pacman(args));
        let (mut status, mut mark) = (format!("[SUCCESS] {install} installed"), "✔");
        if rc != 0 {
            if c.target_user != "root" && have("yay") && ctx::grant_yay_sudo() {
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
        d.render(idx, s_total, install, &status, Some(mark));
    }
    log("INFO", "Completed package installation phase.");
    ui::sleep_secs(1.0);
}

// --- State 5: Services ---------------------------------------------------------------

/// Insert the Howdy rule after the `#%PAM-1.0` header (or at the top). None if already present.
pub fn patch_pam_content(content: &str) -> Option<String> {
    if content.contains("pam_howdy.so") {
        return None;
    }
    if !content.lines().any(|l| l.starts_with("#%PAM-1.0")) {
        return Some(format!("{PAM_RULE}\n{content}"));
    }
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
    Some(res)
}

fn patch_pam(pfile: &str, fallback: Option<&str>) -> Result<()> {
    let p = Path::new(pfile);
    if let Some(fb) = fallback.filter(|fb| !p.is_file() && Path::new(fb).is_file()) {
        fs::create_dir_all(p.parent().unwrap())?;
        fs::copy(fb, p).with_context(|| format!("copying {fb}"))?;
    }
    if !p.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(p).with_context(|| format!("reading {pfile}"))?;
    if let Some(new) = patch_pam_content(&content) {
        fs::copy(p, format!("{pfile}.bak.{}", ctx::now_str("%Y%m%d%H%M%S")))?;
        ctx::write_atomic(p, new.as_bytes()).with_context(|| format!("writing {pfile}"))?;
        log("INFO", &format!("Patched PAM file {pfile} for Howdy"));
    }
    Ok(())
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

pub fn setup_services(auto: bool, o_start: i64, o_step: i64, o_total: i64) -> Result<()> {
    let c = ctx();
    let d = Dash::new(auto, o_start, o_step, o_total, "Setup Services");
    let s_total = 4;

    // ProtonVPN and Fcitx5: packages only, no env/autostart files
    d.step("Services", 1, s_total, "ProtonVPN", &format!("Installing ProtonVPN CLI package for {}...", c.target_user));
    d.exec_monitored(1, s_total, "Installing proton-vpn-cli", pacman(&["-S", "--needed", "--noconfirm", "proton-vpn-cli"]));

    d.step("Services", 2, s_total, "Vietnamese Typing (Fcitx5)", "Installing Fcitx5 packages (no environment file changes)...");
    d.exec_monitored(
        2,
        s_total,
        "Installing Fcitx5 typing packages",
        pacman(&["-S", "--needed", "--noconfirm", "fcitx5", "fcitx5-configtool", "fcitx5-gtk", "fcitx5-qt", "fcitx5-unikey"]),
    );

    d.step("Services", 3, s_total, "Face Recognition (Howdy)", "Detecting sensor, models & PAM rules...");
    d.exec_monitored(3, s_total, "Installing v4l-utils for camera detection", pacman(&["-S", "--needed", "--noconfirm", "v4l-utils"]));

    let device = find_ir_camera();
    log("INFO", &format!("Howdy device node resolved to {device}"));

    let models = Path::new("/usr/share/howdy/models");
    if Path::new("/usr/share/howdy").is_dir()
        && !models.join("face_detection_yunet_2026may.onnx").is_file()
        && !models.join("face_detection_yunet_2023mar.onnx").is_file()
    {
        let mut dl = Command::new("howdy");
        dl.arg("download-models");
        d.exec_monitored(3, s_total, "Downloading Howdy AI face models", dl);
    }
    if Path::new("/etc/howdy/config.ini").is_file() {
        ok_logged("howdy", &["set", "device_path", &device]);
        ok_logged("howdy", &["set", "detection_notice", "true"]);
    }

    // Referencing a PAM module that does not exist breaks the whole auth stack
    // (sudo/login fail with "Module is unknown"), so only patch PAM once Howdy is installed.
    let module =
        ["/usr/lib/security/pam_howdy.so", "/lib/security/pam_howdy.so", "/usr/lib64/security/pam_howdy.so"].into_iter().find(|m| Path::new(m).exists());
    if module.is_none() {
        log("WARN", "pam_howdy.so not found (howdy-next not installed?). Skipping PAM patching and resume hook.");
        add_issue("Howdy not installed; face authentication was not configured");
    } else {
        patch_pam("/etc/pam.d/sudo", None)?;
        for name in ["polkit-1", "system-local-login", "plasmalogin", "sddm", "gdm-password", "hyprlock", "swaylock"] {
            patch_pam(&format!("/etc/pam.d/{name}"), Some(&format!("/usr/lib/pam.d/{name}")))?;
        }
        let hook = Path::new(HOWDY_HOOK);
        fs::create_dir_all(hook.parent().unwrap())?;
        fs::write(hook, howdy_hook(&device)).context("writing Howdy resume hook")?;
        let mode = fs::metadata(hook)?.permissions().mode() | 0o111;
        fs::set_permissions(hook, fs::Permissions::from_mode(mode))?;
    }

    d.step("Services", 4, s_total, "Search Services", "Enabling and starting valkey & searxng...");
    for svc in ["valkey", "searxng"] {
        if !ok_logged("systemctl", &["enable", "--now", svc]) {
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
    pub issues: Vec<String>,
    pub howdy_enrolled: bool,
}

pub fn run_automatic() -> Result<()> {
    let o_start = run_automatic_steps()?;
    ui::restore_terminal_interactive();
    ui::clear_screen();
    print_summary(&collect_summary(o_start));
    ui::read_choice("Press Enter to return to master menu...");
    Ok(())
}

/// The four steps of the automatic run; returns its start time.
pub fn run_automatic_steps() -> Result<i64> {
    let o_start = ui::now();
    let o_total = 4;
    ctx::clear_issues();
    log("INFO", "==========================================");
    log("INFO", "Starting Full Automated Setup Sequence...");
    log("INFO", "==========================================");

    system_upgrade(true, o_start, 1, o_total);
    add_kud3n_repo(true, o_start, 2, o_total)?;
    install_packages(true, o_start, 3, o_total);
    setup_services(true, o_start, 4, o_total)?;
    Ok(o_start)
}

/// The state after the automatic run (takes the recorded issues).
pub fn collect_summary(o_start: i64) -> Summary {
    let c = ctx();
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
        issues: ctx::take_issues(),
        howdy_enrolled: Path::new(&format!("/etc/howdy/models/{}.dat", c.target_user)).is_file(),
    }
}

pub const PARTITION_HINT: &str = "not set up (optional: menu option 2 or `sudo kast partition`)";
pub const HOWDY_RESTORE_HINT: &str = "(or restore a saved face model with menu option 6 / `sudo kast import`)";

fn print_summary(s: &Summary) {
    let partition = s.partition.clone().unwrap_or_else(|| format!("{C_GRAY}{PARTITION_HINT}{C_RESET}"));
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
    if !issues.is_empty() {
        println!("\n{C_YELLOW}{C_BOLD}Issues:{C_RESET}");
        for issue in issues {
            println!("  {C_RED}✘{C_RESET} {issue}");
        }
    }
    println!();
    if s.howdy_enrolled {
        println!("  • Howdy Face Model:   {C_GREEN}Enrolled / Restored for {}{C_RESET}", s.user);
    } else {
        println!("{C_YELLOW}Note:{C_RESET} If you have not enrolled your face in Howdy yet, run:");
        println!("  sudo howdy -U {} add", s.user);
        println!("  {HOWDY_RESTORE_HINT}");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fstab_appends_when_missing() {
        let out = patch_fstab("UUID=x / ext4 defaults 0 1", "ABC", 1000, 1000).unwrap();
        assert_eq!(out, format!("UUID=x / ext4 defaults 0 1\n{}\n", fstab_line("ABC", 1000, 1000)));
    }

    #[test]
    fn fstab_replaces_stale_entry_and_keeps_good_one() {
        let stale = "UUID=x / ext4 defaults 0 1\nUUID=OLD /media/data ntfs3 defaults 0 0\n";
        let out = patch_fstab(stale, "ABC", 1000, 1000).unwrap();
        assert_eq!(out, format!("UUID=x / ext4 defaults 0 1\n{}\n", fstab_line("ABC", 1000, 1000)));
        assert_eq!(patch_fstab(&out, "ABC", 1000, 1000), None);
        // /media/database must not be mistaken for the mount point
        assert!(patch_fstab("UUID=y /media/database ext4 d 0 0\n", "ABC", 1000, 1000).unwrap().contains("/media/database"));
    }

    #[test]
    fn pacman_conf_block() {
        let added = patch_pacman_conf("[core]\nInclude = x\n");
        assert_eq!(added, format!("[core]\nInclude = x\n\n[archrepo]\nSigLevel = Required\nServer = {REPO_URL}\n"));
        assert_eq!(patch_pacman_conf(&added), added);
        let old = format!("[archrepo]\nServer = {OLD_REPO_URL}\n");
        assert_eq!(patch_pacman_conf(&old), format!("[archrepo]\nServer = {REPO_URL}\n"));
        // the SigLevel older versions wrote is upgraded, only inside [archrepo]
        let trusting = format!("[extra]\nSigLevel = Optional TrustAll\n\n[archrepo]\nSigLevel = Optional  TrustAll\nServer = {REPO_URL}\n[x]\n");
        assert_eq!(
            patch_pacman_conf(&trusting),
            format!("[extra]\nSigLevel = Optional TrustAll\n\n[archrepo]\nSigLevel = Required\nServer = {REPO_URL}\n[x]\n")
        );
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
        assert_eq!(patch_pam_content("#%PAM-1.0\nauth include system-auth\n").unwrap(), format!("#%PAM-1.0\n{PAM_RULE}\nauth include system-auth\n"));
        assert_eq!(patch_pam_content("auth include x\n").unwrap(), format!("{PAM_RULE}\nauth include x\n"));
        assert_eq!(patch_pam_content(&format!("{PAM_RULE}\n")), None);
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
    fn legacy_names_map_to_archrepo() {
        assert_eq!(canonical_pkg("sine-bin"), "archrepo/sine");
        assert_eq!(canonical_pkg("zed"), "zed-globalmenu");
        assert_eq!(canonical_pkg("zotero"), "zotero");
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
        let failed = migrate_user_dirs(&mount, &home, &["Documents", "Music", "Videos", "Projects"], uid, gid, std::process::Stdio::null).unwrap();
        fs::set_permissions(mount.join("Videos"), fs::Permissions::from_mode(0o755)).unwrap();

        // Real folder: copied, removed, symlinked
        assert_eq!(fs::read_link(home.join("Documents")).unwrap(), mount.join("Documents"));
        assert_eq!(fs::read_to_string(mount.join("Documents/sub/a.txt")).unwrap(), "doc");
        // Existing symlink: repointed
        assert_eq!(fs::read_link(home.join("Music")).unwrap(), mount.join("Music"));
        // Missing folder: just linked
        assert_eq!(fs::read_link(home.join("Projects")).unwrap(), mount.join("Projects"));
        // Failed copy: original kept, no symlink
        assert_eq!(failed, vec![("Videos".to_string(), mount.join("Videos").display().to_string())]);
        assert!(!home.join("Videos").is_symlink());
        assert_eq!(fs::read_to_string(home.join("Videos/v.mkv")).unwrap(), "v");
    }
}
