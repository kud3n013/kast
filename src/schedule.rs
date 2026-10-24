//! Scheduled backups to Proton Drive: a systemd *user* timer for the target user.
//!
//! A user timer runs inside the user's session, where the keyring that holds the Proton
//! Drive session is normally unlocked, and never as root. It only fires while the user is
//! logged in; `Persistent=true` catches a missed run up at the next login. (Lingering would
//! run it with no unlocked keyring, so kast doesn't turn it on.)
//!
//! The run itself is `kast backup <targets…> --upload proton --scheduled`: no prompts ever,
//! running apps left out (never closed), no face model, nothing uploaded when nothing
//! changed, retention applied, the result in `~/.local/state/kast/schedule.json`, the log
//! and (on failure) a desktop notification.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ctx::{self, ctx};
use crate::drive::{self, Uploaded};
use crate::report::{Level, Progress, Prompt, Reporter};
use crate::settings;

pub const SERVICE: &str = "kast-backup.service";
pub const TIMER: &str = "kast-backup.timer";
/// The targets a schedule may cover (the face model needs sudo: never)
pub const TARGETS: [&str; 16] =
    ["all", "beeper", "zalo", "vesktop", "zen", "helium", "obsidian", "fcitx5", "antigravity", "fdm", "fish", "fastfetch", "scripts", "zotero", "kde", "hypr"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub targets: Vec<String>,
    /// "daily" or "weekly"
    pub every: String,
    pub keep: u32,
}

impl Spec {
    pub fn validate(&self) -> Result<(), String> {
        if self.targets.is_empty() {
            return Err("Pick at least one thing to back up".into());
        }
        if let Some(t) = self.targets.iter().find(|t| !TARGETS.contains(&t.as_str())) {
            return Err(format!("{t} can't be scheduled (the face model needs sudo)"));
        }
        if !matches!(self.every.as_str(), "daily" | "weekly") {
            return Err(format!("--every is daily or weekly, not {}", self.every));
        }
        if self.keep == 0 {
            return Err("--keep is at least 1".into());
        }
        Ok(())
    }
}

/// `~/.config/systemd/user`
pub fn unit_dir(config_home: &Path) -> PathBuf {
    config_home.join("systemd/user")
}

/// The service: one run of the scheduled backup, gently (nice, idle I/O).
pub fn service_unit(exe: &Path, spec: &Spec) -> String {
    format!(
        "# Written by kast (kast schedule enable); kast schedule disable removes it.\n\
         [Unit]\n\
         Description=kast: back up to Proton Drive\n\
         Documentation=https://github.com/kud3n013/kast\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} backup {} --upload proton --keep {} --scheduled\n\
         Nice=10\n\
         IOSchedulingClass=idle\n",
        systemd_quote(&exe.display().to_string()),
        spec.targets.join(" "),
        spec.keep
    )
}

/// The timer: daily or weekly, caught up after the machine was off, spread by 30 minutes.
pub fn timer_unit(spec: &Spec) -> String {
    format!(
        "# Written by kast (kast schedule enable); kast schedule disable removes it.\n\
         [Unit]\n\
         Description=kast: scheduled backup to Proton Drive\n\
         \n\
         [Timer]\n\
         OnCalendar={}\n\
         Persistent=true\n\
         RandomizedDelaySec=30min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        spec.every
    )
}

/// A path as systemd reads it on an ExecStart line (quoted when it has spaces).
fn systemd_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_string() }
}

/// The program an existing service runs (to notice a kast that moved).
pub fn exec_path(service: &str) -> Option<String> {
    let line = service.lines().find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let line = line.trim_start_matches(['-', '@', '+', '!', ':']);
    if let Some(rest) = line.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.extend(chars.next()),
                '"' => return Some(out),
                c => out.push(c),
            }
        }
        None
    } else {
        line.split_whitespace().next().map(String::from)
    }
}

/// The targets, keep and frequency of existing units (shown on the schedule screen).
pub fn spec_of(service: &str, timer: &str) -> Option<Spec> {
    let exec = service.lines().find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let words: Vec<&str> = exec.split_whitespace().collect();
    let start = words.iter().position(|w| *w == "backup")? + 1;
    let targets: Vec<String> = words[start..].iter().take_while(|w| !w.starts_with("--")).map(|w| w.to_string()).collect();
    let keep = words.iter().position(|w| *w == "--keep").and_then(|i| words.get(i + 1)).and_then(|k| k.parse().ok()).unwrap_or(settings::DEFAULT_KEEP);
    let every = timer.lines().find_map(|l| l.trim().strip_prefix("OnCalendar=")).unwrap_or("daily").to_string();
    Some(Spec { targets, every, keep })
}

// --- systemctl --user -----------------------------------------------------------------------

/// `systemctl --user …` as the target user (sudo -u … env with their runtime dir and bus
/// when kast is root).
fn systemctl(args: &[&str]) -> std::process::Command {
    let mut argv = vec!["systemctl", "--user"];
    argv.extend_from_slice(args);
    if ctx().is_root {
        ctx::run_as_user(&argv)
    } else {
        let mut c = std::process::Command::new("systemctl");
        c.arg("--user").args(args);
        c
    }
}

fn systemctl_ok(args: &[&str]) -> Result<()> {
    let out = systemctl(args).stdin(Stdio::null()).output()?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("systemctl --user {} failed: {err}", args.join(" "))
    }
}

fn systemctl_out(args: &[&str]) -> String {
    systemctl(args).stdin(Stdio::null()).stderr(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn current_exe() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_else(|| PathBuf::from("/usr/bin/kast"))
}

fn paths() -> (PathBuf, PathBuf) {
    let d = unit_dir(&ctx().config_home);
    (d.join(SERVICE), d.join(TIMER))
}

/// Write the units (as the user) and enable the timer. Recorded for undo when kast is root.
pub fn enable(spec: &Spec) -> Result<()> {
    spec.validate().map_err(|e| anyhow::anyhow!(e))?;
    let (svc, tmr) = paths();
    let before = (settings::user_read(&svc), settings::user_read(&tmr));
    let exe = current_exe();
    settings::user_write(&svc, service_unit(&exe, spec).as_bytes(), 0o644)?;
    settings::user_write(&tmr, timer_unit(spec).as_bytes(), 0o644)?;
    systemctl_ok(&["daemon-reload"])?;
    systemctl_ok(&["enable", "--now", TIMER])?;
    settings::update(&["schedule", "targets"], Some(toml::Value::Array(spec.targets.iter().map(|t| t.clone().into()).collect())))?;
    settings::update(&["schedule", "every"], Some(spec.every.clone().into()))?;
    crate::journal::user_timer("enable", &svc, &tmr, before.0.as_deref(), before.1.as_deref());
    Ok(())
}

/// Stop and disable the timer and remove the units (as the user).
pub fn disable() -> Result<()> {
    let (svc, tmr) = paths();
    let before = (settings::user_read(&svc), settings::user_read(&tmr));
    let _ = systemctl_ok(&["disable", "--now", TIMER]);
    for p in [&tmr, &svc] {
        remove_user_file(p)?;
    }
    let _ = systemctl_ok(&["daemon-reload"]);
    if before.0.is_some() || before.1.is_some() {
        crate::journal::user_timer("disable", &svc, &tmr, before.0.as_deref(), before.1.as_deref());
    }
    Ok(())
}

fn remove_user_file(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p).is_err() {
        return Ok(());
    }
    if ctx().is_root {
        let s = p.display().to_string();
        let st = ctx::run_as_user(&["rm", "-f", "--", &s]).stdin(Stdio::null()).status()?;
        anyhow::ensure!(st.success(), "could not remove {s}");
    } else {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// Undo of an enable: put the units back as they were (or remove them), as the user.
pub fn undo_enable(before_service: Option<&str>, before_timer: Option<&str>) -> Result<()> {
    let (svc, tmr) = paths();
    let _ = systemctl_ok(&["disable", "--now", TIMER]);
    for (p, before) in [(&svc, before_service), (&tmr, before_timer)] {
        match before {
            Some(text) => settings::user_write(p, text.as_bytes(), 0o644)?,
            None => remove_user_file(p)?,
        }
    }
    let _ = systemctl_ok(&["daemon-reload"]);
    if before_timer.is_some() {
        let _ = systemctl_ok(&["enable", "--now", TIMER]);
    }
    Ok(())
}

/// Start a run now (doesn't wait for it).
pub fn run_now() -> Result<()> {
    systemctl_ok(&["start", "--no-block", SERVICE])
}

// --- Status ----------------------------------------------------------------------------------

/// What the last scheduled run did (`~/.local/state/kast/schedule.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastRun {
    pub at: String,
    /// uploaded, unchanged, nothing, failed
    pub result: String,
    pub uploaded: Vec<String>,
    pub skipped_apps: Vec<String>,
    pub error: Option<String>,
    pub next_run: Option<String>,
}

impl LastRun {
    pub fn path() -> PathBuf {
        settings::state_dir().join("schedule.json")
    }

    pub fn load() -> Option<LastRun> {
        settings::user_read(&Self::path()).and_then(|t| serde_json::from_str(&t).ok())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub installed: bool,
    pub enabled: bool,
    pub active: bool,
    pub next: Option<String>,
    pub spec: Option<Spec>,
    /// The service runs another kast than this one (it moved)
    pub stale: Option<String>,
    pub last: Option<LastRun>,
}

pub fn status() -> Status {
    let (svc, tmr) = paths();
    let service = settings::user_read(&svc);
    let timer = settings::user_read(&tmr);
    let installed = service.is_some() && timer.is_some();
    let next =
        if installed { Some(systemctl_out(&["show", TIMER, "-p", "NextElapseUSecRealtime", "--value"])).filter(|s| !s.is_empty() && s != "n/a") } else { None };
    let exe = current_exe().display().to_string();
    let stale =
        service.as_deref().and_then(exec_path).filter(|p| *p != exe && Path::new(p).canonicalize().ok().map(|c| c.display().to_string()) != Some(exe.clone()));
    Status {
        installed,
        enabled: installed && systemctl_out(&["is-enabled", TIMER]) == "enabled",
        active: installed && systemctl_out(&["is-active", TIMER]) == "active",
        next,
        spec: match (&service, &timer) {
            (Some(s), Some(t)) => spec_of(s, t),
            _ => None,
        },
        stale,
        last: LastRun::load(),
    }
}

pub fn status_json(s: &Status) -> serde_json::Value {
    json!({
        "installed": s.installed,
        "enabled": s.enabled,
        "active": s.active,
        "next_run": s.next,
        "targets": s.spec.as_ref().map(|p| p.targets.clone()),
        "every": s.spec.as_ref().map(|p| p.every.clone()),
        "keep": s.spec.as_ref().map(|p| p.keep),
        "stale_path": s.stale,
        "last_run": s.last,
    })
}

// --- The scheduled run -------------------------------------------------------------------------

/// The scheduled run's reporter: plain output (the user journal gets it), every line in the
/// user's log, and no questions: any that comes up is answered No and logged.
pub struct Scheduled;

impl Reporter for Scheduled {
    fn log(&self, level: Level, msg: &str) {
        crate::report::CliReporter.log(level, msg);
        if !matches!(level, Level::Blank) {
            ctx::log_file_only(&format!("{level:?}").to_uppercase(), &crate::ui::sanitize_log(msg));
        }
    }
    fn progress(&self, _: &Progress) {}
    fn ask(&self, p: &Prompt) -> bool {
        ctx::log_file_only("WARN", &format!("Scheduled backup: not asking \"{}\"; answered no", p.text));
        false
    }
}

/// The options a scheduled backup of `target` runs with.
pub fn scheduled_opts(target: &str, keep: u32, also_local: Option<PathBuf>) -> (crate::vault::backup::BackupOpts, drive::UploadOpts) {
    let o = crate::vault::backup::BackupOpts {
        target: target.to_string(),
        include_media: true,
        running: crate::vault::backup::RunningPolicy::Skip,
        no_howdy: true,
        no_relaunch: true,
        ..Default::default()
    };
    let up = drive::UploadOpts {
        keep,
        also_local,
        if_unchanged: drive::IfUnchanged::Skip,
        retention_ok: true,
        // Without the face model, "all" is its own series: retention never mixes the two
        name_target: (target == "all").then(|| "apps".to_string()),
    };
    (o, up)
}

/// `kast backup <targets…> --upload proton --scheduled`.
pub fn run_scheduled(targets: &[String], keep: u32) -> Result<()> {
    crate::report::install(std::sync::Arc::new(Scheduled));
    let s = settings::load();
    let mut last = LastRun { at: ctx::now_str("%Y-%m-%d %H:%M"), ..Default::default() };
    let outcome = (|| -> Result<Vec<Uploaded>> {
        let cli_ok = drive::ready_cli();
        if let Err(e) = cli_ok {
            return Err(e.into());
        }
        let mut out = Vec::new();
        for t in targets.iter().filter(|t| t.as_str() != "howdy") {
            let (o, up) = scheduled_opts(t, keep, s.also_local());
            out.push(drive::backup_upload(o, &up)?);
            last.skipped_apps.extend(crate::vault::backup::take_skipped());
        }
        Ok(out)
    })();
    let result = match &outcome {
        Ok(list) => {
            for u in list {
                if let Uploaded::Done { remote, .. } = u {
                    last.uploaded.push(remote.clone());
                }
            }
            last.result = if !last.uploaded.is_empty() {
                "uploaded"
            } else if list.iter().any(|u| matches!(u, Uploaded::Unchanged { .. })) {
                "unchanged"
            } else {
                "nothing"
            }
            .into();
            Ok(())
        }
        Err(e) => {
            last.result = "failed".into();
            last.error = Some(e.to_string());
            Err(e)
        }
    };
    last.next_run = Some(systemctl_out(&["show", TIMER, "-p", "NextElapseUSecRealtime", "--value"])).filter(|s| !s.is_empty() && s != "n/a");
    let summary = match last.result.as_str() {
        "uploaded" => format!("Uploaded {}", last.uploaded.iter().map(|u| u.rsplit('/').next().unwrap_or(u)).collect::<Vec<_>>().join(", ")),
        "unchanged" => "Nothing changed since the last upload".into(),
        "nothing" => "Nothing was backed up".into(),
        _ => last.error.clone().unwrap_or_default(),
    };
    let skipped = if last.skipped_apps.is_empty() { String::new() } else { format!(" (left out, running: {})", last.skipped_apps.join(", ")) };
    ctx::log("INFO", &format!("Scheduled backup: {}{skipped}", summary));
    if let Err(e) = settings::user_write(&LastRun::path(), (serde_json::to_string_pretty(&last)? + "\n").as_bytes(), 0o600) {
        ctx::log("WARN", &format!("Could not write {}: {e}", LastRun::path().display()));
    }
    match &result {
        Err(e) => notify(true, "kast: scheduled backup failed", &failure_text(e)),
        Ok(()) if s.schedule.notify_success => notify(false, "kast: scheduled backup", &format!("{summary}{skipped}")),
        Ok(()) => {}
    }
    match outcome {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

/// What a failure notification says, with what to do.
pub fn failure_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<drive::NotReady>().map(|n| &n.0) {
        Some(crate::proton::Problem::SignedOut) => "Not signed in to Proton Drive. Run: kast drive login".into(),
        Some(crate::proton::Problem::Keyring(_)) => "The keyring holding the Proton Drive session is locked. Unlock it, or run: kast drive store pass".into(),
        Some(crate::proton::Problem::Offline(_)) => "Proton Drive couldn't be reached (offline?). It tries again next time.".into(),
        Some(crate::proton::Problem::NotInstalled) => "proton-drive isn't installed. See: kast drive status".into(),
        _ => format!("{e}. Details: kast log"),
    }
}

/// A desktop notification through notify-send, when it's there.
fn notify(critical: bool, title: &str, body: &str) {
    if !ctx::have("notify-send") {
        return;
    }
    let urgency = if critical { "critical" } else { "normal" };
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "kast", "-u", urgency, "-i", "drive-harddisk", title, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// --- kast schedule ------------------------------------------------------------------------------

pub fn cmd_enable(targets: Vec<String>, every: Option<String>, keep: Option<u32>) -> Result<()> {
    let s = settings::load();
    let spec = Spec {
        targets: if targets.is_empty() { s.schedule.targets.clone().unwrap_or_else(|| vec!["all".into()]) } else { targets },
        every: every.or(s.schedule.every.clone()).unwrap_or_else(|| "daily".into()),
        keep: keep.unwrap_or_else(|| s.keep()),
    };
    enable(&spec)?;
    println!("Scheduled: {} backup of {} to Proton Drive (keeps {}).", spec.every, spec.targets.join(", "), spec.keep);
    println!("It runs while you're logged in; a run missed while the computer was off happens at your next login.");
    println!("Running apps are left out (never closed); the face model isn't included.");
    let st = status();
    if let Some(n) = st.next {
        println!("Next run: {n}");
    }
    Ok(())
}

pub fn cmd_disable() -> Result<()> {
    disable()?;
    println!("The scheduled backup is off (kast-backup.timer and .service removed).");
    Ok(())
}

pub fn cmd_status(json_out: bool) -> Result<()> {
    let s = status();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&status_json(&s))?);
        return Ok(());
    }
    if !s.installed {
        println!("No scheduled backup. Set one up: kast schedule enable --targets all --every daily");
        return Ok(());
    }
    println!(
        "Scheduled backup: {}",
        if s.enabled && s.active {
            "on"
        } else if s.enabled {
            "enabled, not active"
        } else {
            "off"
        }
    );
    if let Some(p) = &s.spec {
        println!("What:      {} ({}, keeps {})", p.targets.join(", "), p.every, p.keep);
    }
    if let Some(n) = &s.next {
        println!("Next run:  {n}");
    }
    match &s.last {
        Some(l) => {
            println!("Last run:  {} · {}", l.at, l.result);
            for u in &l.uploaded {
                println!("           {u}");
            }
            if !l.skipped_apps.is_empty() {
                println!("Left out:  {} (running)", l.skipped_apps.join(", "));
            }
            if let Some(e) = &l.error {
                println!("Error:     {e}");
            }
        }
        None => println!("Last run:  not yet"),
    }
    if let Some(p) = &s.stale {
        println!();
        println!("The timer runs {p}, which isn't this kast ({}).", current_exe().display());
        println!("Point it here: kast schedule enable");
    }
    Ok(())
}

pub fn cmd_run() -> Result<()> {
    run_now()?;
    println!("Started; see the result with: kast schedule status");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec { targets: vec!["all".into()], every: "daily".into(), keep: 5 }
    }

    #[test]
    fn units_match_the_golden_files() {
        assert_eq!(service_unit(Path::new("/usr/bin/kast"), &spec()), include_str!("testdata/kast-backup.service"));
        assert_eq!(timer_unit(&Spec { every: "weekly".into(), ..spec() }), include_str!("testdata/kast-backup.timer"));
        let multi = Spec { targets: vec!["zen".into(), "beeper".into()], every: "weekly".into(), keep: 3 };
        assert!(
            service_unit(Path::new("/opt/my apps/kast"), &multi)
                .contains("ExecStart=\"/opt/my apps/kast\" backup zen beeper --upload proton --keep 3 --scheduled\n")
        );
    }

    #[test]
    fn units_read_back() {
        let svc = service_unit(Path::new("/opt/my apps/kast"), &Spec { targets: vec!["zen".into(), "kde".into()], every: "weekly".into(), keep: 3 });
        let tmr = timer_unit(&Spec { every: "weekly".into(), ..spec() });
        assert_eq!(exec_path(&svc).as_deref(), Some("/opt/my apps/kast"));
        assert_eq!(exec_path("ExecStart=/usr/local/bin/kast backup all").as_deref(), Some("/usr/local/bin/kast"));
        assert_eq!(exec_path("[Service]\n"), None);
        assert_eq!(spec_of(&svc, &tmr), Some(Spec { targets: vec!["zen".into(), "kde".into()], every: "weekly".into(), keep: 3 }));
    }

    #[test]
    fn specs_are_checked() {
        assert!(spec().validate().is_ok());
        assert!(Spec { targets: vec![], ..spec() }.validate().is_err());
        assert!(Spec { targets: vec!["howdy".into()], ..spec() }.validate().unwrap_err().contains("face model"));
        assert!(Spec { every: "hourly".into(), ..spec() }.validate().is_err());
    }

    #[test]
    fn scheduled_policy() {
        let (o, up) = scheduled_opts("all", 4, None);
        assert_eq!((o.running, o.no_howdy, o.kill, o.no_relaunch), (crate::vault::backup::RunningPolicy::Skip, true, false, true));
        assert_eq!((up.if_unchanged, up.retention_ok, up.keep, up.name_target.as_deref()), (drive::IfUnchanged::Skip, true, 4, Some("apps")));
        assert_eq!(scheduled_opts("zen", 4, None).1.name_target, None);
        // No questions: the reporter answers No without asking
        assert!(!Scheduled.ask(&Prompt::yes_no("Close?", true)));
    }

    /// Fails the test on any question.
    struct NoPrompts;

    impl Reporter for NoPrompts {
        fn log(&self, _: Level, _: &str) {}
        fn progress(&self, _: &Progress) {}
        fn ask(&self, p: &Prompt) -> bool {
            panic!("a scheduled backup asked: {}", p.text)
        }
    }

    #[test]
    fn scheduled_backup_skips_running_apps_and_the_face_model_without_asking() {
        if !ctx::have("rsync") || !ctx::have("pgrep") {
            eprintln!("skipped: needs rsync and pgrep");
            return;
        }
        let c = ctx::init_for_tests();
        let _staging = ctx::staging_test_lock();
        crate::report::install(std::sync::Arc::new(NoPrompts));
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(c.config_home.join("fcitx5")).unwrap();
        std::fs::write(c.config_home.join("fcitx5/config"), "x").unwrap();
        // An "obsidian" that's running
        let fake = t.path().join("obsidian");
        std::fs::copy("/bin/sleep", &fake).unwrap();
        let mut running = std::process::Command::new(&fake).arg("30").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (mut o, _) = scheduled_opts("all", 5, None);
        let out = t.path().join("apps.zip");
        o.output = Some(out.clone());
        let r = crate::vault::backup::cmd_backup(o);
        let _ = running.kill();
        let _ = running.wait();
        r.unwrap();
        assert_eq!(crate::vault::backup::take_skipped(), ["obsidian"]);
        let m: serde_json::Value = serde_json::from_str(&crate::vault::archive::read_zip_manifest(&out).unwrap()).unwrap();
        let apps = m["apps"].as_object().unwrap();
        assert!(apps.contains_key("fcitx5") && !apps.contains_key("obsidian") && !apps.contains_key("howdy"), "{apps:?}");
    }

    #[test]
    fn failure_texts_say_what_to_do() {
        let e: anyhow::Error = drive::NotReady(crate::proton::Problem::SignedOut).into();
        assert_eq!(failure_text(&e), "Not signed in to Proton Drive. Run: kast drive login");
        let e: anyhow::Error = drive::NotReady(crate::proton::Problem::Keyring("x".into())).into();
        assert!(failure_text(&e).contains("kast drive store pass"));
        assert!(failure_text(&anyhow::anyhow!("Proton Drive is full")).starts_with("Proton Drive is full"));
    }
}
