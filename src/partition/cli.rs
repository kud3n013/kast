//! `kast partition`: list the partitions, or mount one without the full-screen app.

use anyhow::{Result, bail};
use clap::Args;
use serde_json::json;

use super::apply::{self, Plan, Who};
use super::detect::{self, Row};
use super::fstab::{self, Choices, Mode};
use super::{SysRunner, human};
use crate::ctx;
use crate::journal::sys_path;
use crate::setup::{self, HomeState};

#[derive(Args, Debug, Default)]
pub struct PartitionArgs {
    /// List the partitions and whether each can be used, then exit
    #[arg(long)]
    pub list: bool,
    /// With --list: machine-readable output
    #[arg(long, requires = "list")]
    pub json: bool,
    /// The partition: /dev/sdb1, UUID=…, PARTUUID=… or LABEL=…
    #[arg(long)]
    pub device: Option<String>,
    /// Where to mount it (default /media/<label>)
    #[arg(long)]
    pub mountpoint: Option<String>,
    /// Treat it as an external drive: 5 s device timeout, mounted on first access
    #[arg(long, conflicts_with = "internal")]
    pub external: bool,
    /// Treat it as an internal drive: 10 s device timeout, mounted at boot
    #[arg(long)]
    pub internal: bool,
    /// Mount at boot even for an external drive (no x-systemd.automount)
    #[arg(long)]
    pub boot: bool,
    /// Mount read-only
    #[arg(long)]
    pub read_only: bool,
    /// The filesystem options, comma separated (replaces the generated ones; checked per driver)
    #[arg(long)]
    pub options: Option<String>,
    /// Home folders to move onto the partition (comma separated names, or `all`)
    #[arg(long, value_delimiter = ',')]
    pub move_folders: Vec<String>,
    /// Replace the /etc/fstab line(s) that name the partition or the mount point
    #[arg(long)]
    pub replace: bool,
    /// Unmount the partition first if it is mounted somewhere else
    #[arg(long)]
    pub unmount_first: bool,
    /// Show what would be done and check what can be checked; change nothing
    #[arg(long)]
    pub dry_run: bool,
}

impl PartitionArgs {
    /// Nothing given: the full-screen app.
    pub fn is_empty(&self) -> bool {
        !self.list && self.device.is_none() && !self.dry_run
    }

    pub fn needs_root(&self) -> bool {
        !(self.list || self.dry_run)
    }
}

fn list_json(rows: &[Row]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            let p = &r.part;
            json!({
                "device": p.path,
                "label": p.label,
                "fstype": p.fstype,
                "uuid": p.uuid,
                "partuuid": p.partuuid,
                "size": p.size,
                "mountpoints": p.mountpoints,
                "external": p.external,
                "in_fstab": r.existing.iter().map(|l| l.raw.trim()).collect::<Vec<_>>(),
                "selectable": r.driver().is_some(),
                "driver": r.driver().map(|d| d.fstab_type()),
                "reason": r.reason(),
            })
        })
        .collect()
}

fn list_text(rows: &[Row]) -> String {
    let mut s = String::new();
    for r in rows {
        let p = &r.part;
        let fs = p.fstype.as_deref().unwrap_or("-");
        let label = p.label.as_deref().filter(|l| !l.is_empty()).unwrap_or("-");
        let kind = if p.external { "external" } else { "internal" };
        let verdict = match r.reason() {
            Some(why) => format!("not usable: {why}"),
            None => format!("usable ({})", r.driver().map_or("", |d| d.fstab_type())),
        };
        s += &format!("{:<14} {:<12} {:<8} {:>9}  {:<8} {verdict}\n", p.path, label, fs, human(p.size), kind);
    }
    if s.is_empty() {
        s = "No partitions found.\n".into();
    }
    s
}

pub fn run(a: PartitionArgs) -> Result<()> {
    let rn = SysRunner;
    let fstab_text = std::fs::read_to_string(sys_path("/etc/fstab")).unwrap_or_default();
    let (_, rows) = detect::scan(&rn, &fstab_text)?;
    if a.list {
        if a.json {
            println!("{}", serde_json::to_string_pretty(&list_json(&rows))?);
        } else {
            print!("{}", list_text(&rows));
        }
        return Ok(());
    }
    let Some(device) = a.device.as_deref() else { bail!("--device is required (see `kast partition --list`)") };
    let Some(row) = detect::find(&rows, device) else { bail!("no partition matches {device} (see `kast partition --list`)") };
    if let Some(why) = row.reason() {
        bail!("{} can't be used: {why}", row.part.path);
    }
    let Some(driver) = row.driver() else { bail!("{} has no usable driver", row.part.path) };
    let c = ctx::ctx();
    let mountpoint = fstab::normalise_mountpoint(&a.mountpoint.clone().unwrap_or_else(|| super::default_mountpoint(&row.part))).map_err(anyhow::Error::msg)?;
    let mut choices = Choices::new(&row.part, driver, &mountpoint, c.target_uid, c.target_gid);
    if a.external || a.internal {
        choices.external = a.external;
        choices.mode = if a.external { Mode::Automount } else { Mode::Boot };
    }
    if a.boot {
        choices.mode = Mode::Boot;
    }
    choices.read_only = a.read_only;
    choices.own_top_folder = !driver.fs().foreign();
    if let Some(o) = &a.options {
        choices.manual = Some(fstab::validate_manual(driver, o).map_err(anyhow::Error::msg)?);
    }

    let conflicts = fstab::conflicts(&fstab_text, &row.part, &mountpoint);
    if !conflicts.is_empty() && !a.replace {
        let lines: Vec<String> = conflicts.iter().map(|c| format!("  line {} ({}): {}", c.line.no, c.why(), c.line.raw.trim())).collect();
        bail!("these /etc/fstab lines are in the way (pass --replace to replace them):\n{}", lines.join("\n"));
    }
    let mounted: Vec<&String> = row.part.mountpoints.iter().filter(|m| !m.starts_with('[')).collect();
    if !mounted.is_empty() && !a.unmount_first {
        bail!(
            "{} is mounted at {} (pass --unmount-first to let kast unmount it)",
            row.part.path,
            mounted.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    let mut folders = Vec::new();
    let all = a.move_folders.iter().any(|f| f == "all");
    for name in setup::USER_DIRS {
        let wanted = all || a.move_folders.iter().any(|f| f == name);
        if wanted {
            match setup::home_state(&c.target_home.join(name)) {
                HomeState::Dir | HomeState::Absent => folders.push(name.to_string()),
                HomeState::Link(t) if all => eprintln!("~/{name} is already a link to {}: left alone", t.display()),
                HomeState::Link(t) => bail!("~/{name} is already a link to {}", t.display()),
                HomeState::File if all => eprintln!("~/{name} is a file: left alone"),
                HomeState::File => bail!("~/{name} is a file, not a folder"),
            }
        }
    }
    if let Some(bad) = a.move_folders.iter().find(|f| *f != "all" && !setup::USER_DIRS.contains(&f.as_str())) {
        bail!("{bad} is not one of: {}", setup::USER_DIRS.join(", "));
    }

    let plan = Plan {
        part: row.part.clone(),
        choices,
        replace: conflicts.iter().map(|c| c.line.no).collect(),
        unmount_first: !mounted.is_empty(),
        folders,
        who: Who::current(),
        date: ctx::now_str("%Y-%m-%d"),
    };
    for l in apply::describe(&plan) {
        println!("{l}");
    }
    // What can be checked without changing anything
    if let Err(e) = fstab::check_mountpoint(&plan.choices.mountpoint, &sys_path(&plan.choices.mountpoint), &rn) {
        // Mounted there now and about to be unmounted first: that is the way it should be
        if mounted.is_empty() || !e.contains("already a mount point") {
            bail!(e);
        }
    }
    if a.dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if !c.is_root {
        bail!("run with sudo to change /etc/fstab and mount (--dry-run and --list work without it)");
    }
    let r = crate::journal::recorded(&["partition"], false, || setup::setup_data_partition(&plan, false, 0, 1, 1))?;
    println!("\n{} is mounted at {}.", plan.part.path, r.mountpoint);
    for (dir, why) in &r.kept {
        println!("~/{dir} was kept where it is: {why}");
    }
    for (dir, why) in &r.failed {
        println!("~/{dir} was not moved: {why}");
    }
    println!("To revert: {}", apply::UNDO_HINT);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::Partition;
    use crate::partition::detect::Verdict;

    fn rows() -> Vec<Row> {
        let ok = Partition {
            path: "/dev/sdb1".into(),
            fstype: Some("ntfs".into()),
            label: Some("Data".into()),
            uuid: Some("6C62ADEA62ADB96A".into()),
            size: 1 << 40,
            external: true,
            ..Default::default()
        };
        let swap = Partition { path: "/dev/sda3".into(), fstype: Some("swap".into()), size: 1 << 33, ..Default::default() };
        vec![
            Row { part: ok, verdict: Verdict::Pick(crate::partition::Driver::Ntfs3), existing: Vec::new() },
            Row { part: swap, verdict: Verdict::Blocked("swap space".into()), existing: Vec::new() },
        ]
    }

    #[test]
    fn list_text_and_json_say_why_a_row_is_not_usable() {
        let t = list_text(&rows());
        assert!(t.contains("usable (ntfs3)") && t.contains("not usable: swap space"), "{t}");
        let j = list_json(&rows());
        assert_eq!(j[0]["selectable"], true);
        assert_eq!(j[0]["driver"], "ntfs3");
        assert_eq!(j[1]["selectable"], false);
        assert_eq!(j[1]["reason"], "swap space");
        assert_eq!(list_text(&[]), "No partitions found.\n");
    }

    #[test]
    fn root_is_needed_only_to_change_things() {
        let a = PartitionArgs::default();
        assert!(a.is_empty() && a.needs_root());
        assert!(!PartitionArgs { list: true, ..Default::default() }.needs_root());
        assert!(!PartitionArgs { device: Some("/dev/sdb1".into()), dry_run: true, ..Default::default() }.needs_root());
        assert!(PartitionArgs { device: Some("/dev/sdb1".into()), ..Default::default() }.needs_root());
    }
}
