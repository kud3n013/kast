//! System upgrades can't be undone by kast; a filesystem snapshot can. This finds out what
//! the machine has (btrfs, snapper with a root config, snap-pac, Timeshift in btrfs mode,
//! a boot menu that lists snapshots) and takes the snapshots around the upgrade.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Result, bail};
use serde_json::{Value, json};

/// The snapshot tool kast can use for `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// snapper with a `root` config; `snap_pac`: it already snapshots every pacman run
    Snapper { snap_pac: bool },
    /// Timeshift in btrfs mode
    Timeshift,
    /// btrfs, but nothing set up to take snapshots
    BtrfsOnly,
    /// Not btrfs
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub fs: String,
    pub tool: Tool,
    /// A boot menu that lists snapshots (limine-snapper-sync, grub-btrfs)
    pub boot_menu: Option<String>,
}

/// What the commands said, for `decide` (tests pass their own).
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// `findmnt -no FSTYPE /`
    pub fstype: String,
    pub have_snapper: bool,
    /// `snapper list-configs`
    pub snapper_configs: String,
    pub have_timeshift: bool,
    /// /etc/timeshift/timeshift.json
    pub timeshift_json: String,
    /// Installed among snap-pac, limine-snapper-sync, grub-btrfs
    pub packages: Vec<String>,
}

/// Whether snapper has a config named `root` (`snapper list-configs` output).
fn has_root_config(list: &str) -> bool {
    list.lines().any(|l| l.split(['|', '│']).next().is_some_and(|c| c.trim() == "root"))
}

pub fn decide(p: &Probe) -> Detected {
    let fs = p.fstype.trim().to_string();
    let btrfs = fs == "btrfs";
    let has = |n: &str| p.packages.iter().any(|x| x == n);
    let tool = if !btrfs {
        Tool::None
    } else if p.have_snapper && has_root_config(&p.snapper_configs) {
        Tool::Snapper { snap_pac: has("snap-pac") }
    } else if p.have_timeshift && p.timeshift_json.replace(' ', "").contains("\"btrfs_mode\":\"true\"") {
        Tool::Timeshift
    } else {
        Tool::BtrfsOnly
    };
    let boot_menu = ["limine-snapper-sync", "grub-btrfs"].into_iter().find(|n| has(n)).map(String::from);
    Detected { fs, tool, boot_menu: if matches!(tool, Tool::Snapper { .. }) { boot_menu } else { None } }
}

fn out(prog: &str, args: &[&str]) -> String {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// What this machine has (looked at once per session).
pub fn detect() -> Detected {
    static CACHE: OnceLock<Detected> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let have = crate::ctx::have;
            let probe = Probe {
                fstype: out("findmnt", &["-no", "FSTYPE", "/"]),
                have_snapper: have("snapper"),
                snapper_configs: if have("snapper") { out("snapper", &["--no-dbus", "list-configs"]) } else { String::new() },
                have_timeshift: have("timeshift"),
                timeshift_json: std::fs::read_to_string("/etc/timeshift/timeshift.json").unwrap_or_default(),
                packages: ["snap-pac", "limine-snapper-sync", "grub-btrfs"]
                    .into_iter()
                    .filter(|p| crate::ctx::ok("pacman", &["-Q", p]))
                    .map(String::from)
                    .collect(),
            };
            decide(&probe)
        })
        .clone()
}

impl Detected {
    /// kast can take a snapshot here.
    pub fn offered(&self) -> bool {
        matches!(self.tool, Tool::Snapper { .. } | Tool::Timeshift)
    }

    /// On unless snap-pac already snapshots every pacman run.
    pub fn default_on(&self) -> bool {
        matches!(self.tool, Tool::Snapper { snap_pac: false } | Tool::Timeshift)
    }

    pub fn tool_name(&self) -> &'static str {
        match self.tool {
            Tool::Snapper { .. } => "snapper",
            Tool::Timeshift => "timeshift",
            _ => "",
        }
    }

    /// What to say wherever the upgrade is chosen.
    pub fn warning(&self) -> Vec<String> {
        let mut v = vec!["System upgrades can't be undone by kast.".to_string()];
        match self.tool {
            Tool::Snapper { snap_pac } => {
                v.push("/ is btrfs with snapper: kast can take a snapshot before the upgrade (and one after), to roll back to.".into());
                if snap_pac {
                    v.push("snap-pac is installed and already takes snapshots around every pacman run, so kast's own is off by default.".into());
                }
                if let Some(b) = &self.boot_menu {
                    v.push(format!("{b} lists the snapshots in the boot menu: boot one to go back."));
                }
            }
            Tool::Timeshift => v.push("/ is btrfs with Timeshift (btrfs mode): kast can take a snapshot before the upgrade, to restore from.".into()),
            Tool::BtrfsOnly => v.push(
                "/ is btrfs, so it could have snapshots, but no snapshot tool is set up. Install snapper to get them (on CachyOS likely \
                 cachyos-snapper-support; check with pacman -Si). kast doesn't install it."
                    .into(),
            ),
            Tool::None => v.push(format!(
                "/ is {}, with no snapshots to roll back to: make a backup first if you want one.",
                if self.fs.is_empty() { "not btrfs" } else { &self.fs }
            )),
        }
        v
    }
}

/// The command that takes the "before" snapshot.
pub fn pre_cmd(tool: Tool, version: &str) -> Vec<String> {
    match tool {
        Tool::Snapper { .. } => [
            "snapper",
            "-c",
            "root",
            "create",
            "--type",
            "pre",
            "--cleanup-algorithm",
            "number",
            "--print-number",
            "--description",
            &format!("kast {version} before upgrade"),
        ]
        .map(String::from)
        .to_vec(),
        Tool::Timeshift => ["timeshift", "--create", "--comments", "kast before upgrade", "--tags", "O"].map(String::from).to_vec(),
        _ => Vec::new(),
    }
}

/// The matching "after" snapshot (snapper only).
pub fn post_cmd(tool: Tool, pre: u64, version: &str) -> Vec<String> {
    match tool {
        Tool::Snapper { .. } => [
            "snapper",
            "-c",
            "root",
            "create",
            "--type",
            "post",
            "--pre-number",
            &pre.to_string(),
            "--cleanup-algorithm",
            "number",
            "--print-number",
            "--description",
            &format!("kast {version} after upgrade"),
        ]
        .map(String::from)
        .to_vec(),
        _ => Vec::new(),
    }
}

fn run(argv: &[String]) -> Result<String> {
    let o = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output()?;
    if !o.status.success() {
        bail!("{} failed: {}", argv[0], String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("").trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Take the "before" snapshot: its number (snapper) or 0 (timeshift).
pub fn take_pre(d: &Detected) -> Result<u64> {
    let text = run(&pre_cmd(d.tool, env!("CARGO_PKG_VERSION")))?;
    match d.tool {
        Tool::Snapper { .. } => text.trim().parse().map_err(|_| anyhow::anyhow!("snapper printed no snapshot number")),
        _ => Ok(0),
    }
}

pub fn take_post(d: &Detected, pre: u64) -> Option<u64> {
    let argv = post_cmd(d.tool, pre, env!("CARGO_PKG_VERSION"));
    if argv.is_empty() {
        return None;
    }
    run(&argv).ok().and_then(|t| t.trim().parse().ok())
}

/// The journal's record of the snapshots.
pub fn record(d: &Detected, pre: u64, post: Option<u64>) -> Value {
    json!({ "tool": d.tool_name(), "pre": if d.tool_name() == "snapper" { json!(pre) } else { Value::Null }, "post": post, "boot": d.boot_menu })
}

/// How to roll back, with the real snapshot numbers.
pub fn rollback_hint(snap: &Value) -> Option<String> {
    let boot = snap["boot"].as_str().map(|b| format!("boot snapshot {} from the boot menu ({b}), or ", snap["pre"].as_u64().unwrap_or_default()));
    match (snap["tool"].as_str(), snap["pre"].as_u64(), snap["post"].as_u64()) {
        (Some("snapper"), Some(pre), post) => {
            let files = post.map_or(String::new(), |p| format!("; file by file: sudo snapper -c root undochange {pre}..{p}"));
            Some(format!("To roll back: {}sudo snapper -c root rollback {pre} and reboot{files}", boot.unwrap_or_default()))
        }
        (Some("timeshift"), ..) => Some("To roll back: sudo timeshift --restore and pick \"kast before upgrade\"".into()),
        _ => None,
    }
}

#[cfg(test)]
pub use tests::cases as test_cases;

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(fs: &str, snapper: Option<&str>, timeshift: Option<&str>, pkgs: &[&str]) -> Probe {
        Probe {
            fstype: format!("{fs}\n"),
            have_snapper: snapper.is_some(),
            snapper_configs: snapper.unwrap_or_default().into(),
            have_timeshift: timeshift.is_some(),
            timeshift_json: timeshift.unwrap_or_default().into(),
            packages: pkgs.iter().map(|s| s.to_string()).collect(),
        }
    }

    const CONFIGS: &str = "Config │ Subvolume\n───────┼──────────\nhome   │ /home\nroot   │ /\n";

    pub fn cases() -> Vec<(&'static str, Detected)> {
        vec![
            ("snapper", decide(&probe("btrfs", Some(CONFIGS), None, &["limine-snapper-sync"]))),
            ("snap_pac", decide(&probe("btrfs", Some(CONFIGS), None, &["snap-pac"]))),
            ("timeshift", decide(&probe("btrfs", None, Some(r#"{ "btrfs_mode" : "true" }"#), &[]))),
            ("btrfs_only", decide(&probe("btrfs", Some("Config | Subvolume\nhome | /home\n"), None, &[]))),
            ("ext4", decide(&probe("ext4", Some(CONFIGS), None, &[]))),
        ]
    }

    #[test]
    fn detection_matrix() {
        let c = cases();
        assert_eq!(c[0].1, Detected { fs: "btrfs".into(), tool: Tool::Snapper { snap_pac: false }, boot_menu: Some("limine-snapper-sync".into()) });
        assert!(c[0].1.offered() && c[0].1.default_on());
        assert_eq!(c[1].1.tool, Tool::Snapper { snap_pac: true });
        assert!(c[1].1.offered() && !c[1].1.default_on(), "snap-pac: still offered, off by default");
        assert!(c[1].1.warning().iter().any(|w| w.contains("snap-pac")));
        assert_eq!(c[2].1.tool, Tool::Timeshift);
        assert!(c[2].1.default_on());
        assert_eq!(c[3].1.tool, Tool::BtrfsOnly, "snapper without a root config");
        assert!(!c[3].1.offered() && c[3].1.warning()[1].contains("Install snapper"));
        assert_eq!(c[4].1.tool, Tool::None, "snapper configs don't matter off btrfs");
        assert!(c[4].1.warning()[1].starts_with("/ is ext4"));
        for (_, d) in &c {
            assert_eq!(d.warning()[0], "System upgrades can't be undone by kast.");
        }
        assert!(has_root_config("root | /\n"));
        assert!(!has_root_config("rootfs | /\n"));
    }

    #[test]
    fn commands_and_rollback_hints() {
        let snapper = Tool::Snapper { snap_pac: false };
        assert_eq!(
            pre_cmd(snapper, "0.4.0").join(" "),
            "snapper -c root create --type pre --cleanup-algorithm number --print-number --description kast 0.4.0 before upgrade"
        );
        assert_eq!(
            post_cmd(snapper, 41, "0.4.0").join(" "),
            "snapper -c root create --type post --pre-number 41 --cleanup-algorithm number --print-number --description kast 0.4.0 after upgrade"
        );
        assert_eq!(pre_cmd(Tool::Timeshift, "x").join(" "), "timeshift --create --comments kast before upgrade --tags O");
        assert!(pre_cmd(Tool::BtrfsOnly, "x").is_empty() && post_cmd(Tool::Timeshift, 1, "x").is_empty());
        let d = cases().remove(0).1;
        let rec = record(&d, 41, Some(42));
        assert_eq!(
            rollback_hint(&rec).unwrap(),
            "To roll back: boot snapshot 41 from the boot menu (limine-snapper-sync), or sudo snapper -c root rollback 41 and reboot; file by file: sudo snapper -c root undochange 41..42"
        );
        let plain = Detected { boot_menu: None, ..d };
        assert_eq!(rollback_hint(&record(&plain, 7, None)).unwrap(), "To roll back: sudo snapper -c root rollback 7 and reboot");
        assert!(rollback_hint(&Value::Null).is_none());
    }
}
