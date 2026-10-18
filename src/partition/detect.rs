//! Finding the partitions on the machine and deciding which can be picked.
//!
//! `lsblk -J` is the source; the JSON differs between util-linux versions (booleans are
//! strings in old ones, sizes too, `mountpoint` became `mountpoints`), so every field is read
//! defensively. `blkid -p -o export` fills in what lsblk (udev) doesn't know.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::fstab::{self, Line};
use super::{Driver, Fs, Partition, Runner};

const COLUMNS: &[&str] = &[
    "NAME",
    "PATH",
    "TYPE",
    "PKNAME",
    "FSTYPE",
    "FSVER",
    "LABEL",
    "UUID",
    "PARTUUID",
    "PARTLABEL",
    "PARTTYPE",
    "SIZE",
    "FSAVAIL",
    "FSUSED",
    "MOUNTPOINTS",
    "RM",
    "HOTPLUG",
    "TRAN",
    "MODEL",
    "VENDOR",
    "RO",
];

pub const ESP_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

// --- Parsing lsblk ------------------------------------------------------------------------

fn flex_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn flex_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn flex_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Bool(b) => b,
        Value::Number(n) => n.as_u64().is_some_and(|n| n != 0),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        _ => false,
    })
}

fn flex_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Array(a) => a.into_iter().filter_map(|x| x.as_str().map(str::to_string)).filter(|s| !s.is_empty()).collect(),
        Value::String(s) if !s.is_empty() => vec![s],
        _ => Vec::new(),
    })
}

#[derive(Deserialize, Default)]
struct Dev {
    #[serde(default, deserialize_with = "flex_str")]
    name: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    path: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "flex_str")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    fstype: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    fsver: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    label: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    uuid: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    partuuid: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    partlabel: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    parttype: Option<String>,
    #[serde(default, deserialize_with = "flex_u64")]
    size: Option<u64>,
    #[serde(default, deserialize_with = "flex_u64")]
    fsavail: Option<u64>,
    #[serde(default, deserialize_with = "flex_u64")]
    fsused: Option<u64>,
    #[serde(default, deserialize_with = "flex_list")]
    mountpoints: Vec<String>,
    /// util-linux before 2.37
    #[serde(default, deserialize_with = "flex_list")]
    mountpoint: Vec<String>,
    #[serde(default, deserialize_with = "flex_bool")]
    rm: bool,
    #[serde(default, deserialize_with = "flex_bool")]
    hotplug: bool,
    #[serde(default, deserialize_with = "flex_bool")]
    ro: bool,
    #[serde(default, deserialize_with = "flex_str")]
    tran: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    model: Option<String>,
    #[serde(default, deserialize_with = "flex_str")]
    vendor: Option<String>,
    #[serde(default)]
    children: Vec<Dev>,
}

#[derive(Deserialize, Default)]
struct Tree {
    #[serde(default)]
    blockdevices: Vec<Dev>,
}

/// A disk and what is on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Disk {
    pub path: String,
    pub model: String,
    pub size: u64,
    pub tran: Option<String>,
    pub external: bool,
    pub parts: Vec<Partition>,
}

fn partition(d: &Dev, disk: &Dev, whole: bool) -> Partition {
    let mut mounts = d.mountpoints.clone();
    for m in &d.mountpoint {
        if !mounts.contains(m) {
            mounts.push(m.clone());
        }
    }
    Partition {
        path: d.path.clone().or_else(|| d.name.as_ref().map(|n| format!("/dev/{n}"))).unwrap_or_default(),
        name: d.name.clone().unwrap_or_default(),
        disk: disk.path.clone().or_else(|| disk.name.as_ref().map(|n| format!("/dev/{n}"))).unwrap_or_default(),
        whole_disk: whole,
        fstype: d.fstype.clone(),
        fsver: d.fsver.clone(),
        label: d.label.clone(),
        uuid: d.uuid.clone(),
        partuuid: d.partuuid.clone(),
        partlabel: d.partlabel.clone(),
        parttype: d.parttype.clone().map(|t| t.to_ascii_lowercase()),
        size: d.size.unwrap_or(0),
        fsavail: d.fsavail,
        fsused: d.fsused,
        mountpoints: mounts,
        ro: d.ro || disk.ro,
        external: d.rm || d.hotplug || disk.rm || disk.hotplug || disk.tran.as_deref() == Some("usb"),
        tran: disk.tran.clone(),
        model: join_model(disk),
    }
}

fn join_model(d: &Dev) -> Option<String> {
    let m = format!("{} {}", d.vendor.as_deref().unwrap_or_default().trim(), d.model.as_deref().unwrap_or_default().trim());
    let m = m.trim().to_string();
    (!m.is_empty()).then_some(m)
}

/// The disks in `lsblk -J` output, each with its partitions (a disk with a filesystem and no
/// partitions counts as one). Loop devices and the like are left out.
pub fn parse_lsblk(json: &str) -> Result<Vec<Disk>> {
    let tree: Tree = serde_json::from_str(json).context("reading lsblk's output")?;
    let mut disks = Vec::new();
    for d in &tree.blockdevices {
        if !matches!(d.kind.as_deref(), Some("disk") | Some("rom")) {
            continue;
        }
        let parts: Vec<Partition> = d.children.iter().filter(|c| c.kind.as_deref() == Some("part")).map(|c| partition(c, d, false)).collect();
        let whole = d.fstype.is_some() && parts.is_empty();
        let mut parts = parts;
        if whole {
            parts.push(partition(d, d, true));
        }
        let p = partition(d, d, true);
        disks.push(Disk {
            path: p.path.clone(),
            model: join_model(d).unwrap_or_default(),
            size: d.size.unwrap_or(0),
            tran: d.tran.clone(),
            external: p.external,
            parts,
        });
    }
    Ok(disks)
}

/// Add what `blkid -p -o export` says where lsblk had nothing (it reads the device itself,
/// root only; without root there is nothing to add).
pub fn merge_blkid(p: &mut Partition, export: &str) {
    for line in export.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        let slot = match k {
            "TYPE" => &mut p.fstype,
            "UUID" => &mut p.uuid,
            "LABEL" => &mut p.label,
            "VERSION" => &mut p.fsver,
            "PART_ENTRY_UUID" => &mut p.partuuid,
            "PART_ENTRY_TYPE" => &mut p.parttype,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(if k == "PART_ENTRY_TYPE" { v.to_ascii_lowercase() } else { v.to_string() });
        }
    }
}

/// Run `lsblk` and `blkid` and return the disks. Drops a column the installed lsblk doesn't
/// know and tries again.
pub fn detect(rn: &dyn Runner) -> Result<Vec<Disk>> {
    let mut cols: Vec<&str> = COLUMNS.to_vec();
    let json = loop {
        let list = cols.join(",");
        let out = rn.run("lsblk", &["-J", "-b", "-o", &list]);
        if out.ok {
            break out.stdout;
        }
        let msg = out.message();
        match msg.split("unknown column:").nth(1).map(|c| c.trim().to_uppercase()).and_then(|c| cols.iter().position(|x| *x == c)) {
            Some(i) if cols.len() > 4 => {
                cols.remove(i);
            }
            _ => bail!("lsblk failed: {msg}"),
        }
    };
    let mut disks = parse_lsblk(&json)?;
    for p in disks.iter_mut().flat_map(|d| d.parts.iter_mut()) {
        let missing = p.fstype.is_none() || p.uuid.is_none() || p.parttype.is_none();
        if missing && !p.path.is_empty() {
            let b = rn.run("blkid", &["-p", "-o", "export", &p.path]);
            if b.ok {
                merge_blkid(p, &b.stdout);
            }
        }
    }
    Ok(disks)
}

// --- Which drivers the machine has ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drivers {
    pub ntfs3: bool,
    pub ntfs3g: bool,
    pub exfat: bool,
    pub vfat: bool,
    pub ext4: bool,
    pub btrfs: bool,
    pub xfs: bool,
}

impl Drivers {
    /// `/proc/filesystems` lists what is loaded or built in; `modinfo` finds modules that are
    /// there but not loaded yet (mount loads them).
    pub fn from(proc_filesystems: &str, module: impl Fn(&str) -> bool, ntfs3g: bool) -> Drivers {
        let has = |n: &str| proc_filesystems.lines().any(|l| l.split_whitespace().last() == Some(n)) || module(n);
        Drivers { ntfs3: has("ntfs3"), ntfs3g, exfat: has("exfat"), vfat: has("vfat"), ext4: has("ext4"), btrfs: has("btrfs"), xfs: has("xfs") }
    }

    pub fn probe(rn: &dyn Runner) -> Drivers {
        let proc = std::fs::read_to_string("/proc/filesystems").unwrap_or_default();
        let ntfs3g = crate::ctx::have("ntfs-3g") || crate::ctx::have("mount.ntfs-3g");
        Drivers::from(&proc, |n| rn.run("modinfo", &["-n", n]).ok, ntfs3g)
    }

    /// The driver to use for `fs`: `ntfs3` first, `ntfs-3g` if that is all there is.
    pub fn driver(&self, fs: Fs) -> Option<Driver> {
        match fs {
            Fs::Ntfs if self.ntfs3 => Some(Driver::Ntfs3),
            Fs::Ntfs if self.ntfs3g => Some(Driver::Ntfs3g),
            Fs::Ntfs => None,
            Fs::Exfat => self.exfat.then_some(Driver::Exfat),
            Fs::Vfat => self.vfat.then_some(Driver::Vfat),
            Fs::Ext4 => self.ext4.then_some(Driver::Ext4),
            Fs::Btrfs => self.btrfs.then_some(Driver::Btrfs),
            Fs::Xfs => self.xfs.then_some(Driver::Xfs),
        }
    }
}

// --- Selectability -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pick(Driver),
    /// Shown greyed out, with the reason
    Blocked(String),
}

/// A row of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub part: Partition,
    pub verdict: Verdict,
    /// Entries of /etc/fstab that already name it
    pub existing: Vec<Line>,
}

impl Row {
    pub fn driver(&self) -> Option<Driver> {
        match self.verdict {
            Verdict::Pick(d) => Some(d),
            Verdict::Blocked(_) => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match &self.verdict {
            Verdict::Blocked(r) => Some(r),
            Verdict::Pick(_) => None,
        }
    }
}

/// Can `p` be picked, given the drivers and the current fstab?
pub fn classify(p: &Partition, drivers: &Drivers, fstab_text: &str) -> Verdict {
    let blocked = |s: String| Verdict::Blocked(s);
    if p.ro {
        return blocked("read-only device".into());
    }
    if p.mountpoints.iter().any(|m| m == "[SWAP]") {
        return blocked("in use as swap".into());
    }
    if let Some(m) = p.mountpoints.iter().find(|m| fstab::is_system_path(m)) {
        return blocked(format!("in use by the system (mounted at {m})"));
    }
    if p.parttype.as_deref() == Some(ESP_GUID) {
        return blocked("EFI system partition".into());
    }
    if let Some(l) = fstab::system_entry(fstab_text, p) {
        return blocked(format!("listed in /etc/fstab for {}", l.file));
    }
    let Some(t) = p.fstype.as_deref() else {
        return blocked("no filesystem (format it first)".into());
    };
    let Some(fs) = Fs::from_type(t) else {
        return blocked(match t {
            "crypto_LUKS" => "encrypted (LUKS): needs /etc/crypttab, not supported yet".into(),
            "BitLocker" => "encrypted (BitLocker): not supported".into(),
            "LVM2_member" => "part of an LVM volume group".into(),
            "linux_raid_member" => "part of a RAID array".into(),
            "zfs_member" => "part of a ZFS pool".into(),
            "swap" => "swap space".into(),
            "iso9660" | "udf" => "read-only media".into(),
            other => format!("{other} is not supported (NTFS, exFAT, FAT, ext4, btrfs and XFS are)"),
        });
    };
    if p.uuid.is_none() && p.partuuid.is_none() {
        return blocked("it has no UUID or PARTUUID to identify it by".into());
    }
    match drivers.driver(fs) {
        Some(d) => Verdict::Pick(d),
        None if fs == Fs::Ntfs => blocked("NTFS needs the ntfs3 kernel driver or ntfs-3g, and neither is available".into()),
        None => blocked(format!("the kernel has no {} driver", fs.name())),
    }
}

pub fn rows(disks: &[Disk], drivers: &Drivers, fstab_text: &str) -> Vec<Row> {
    disks
        .iter()
        .flat_map(|d| &d.parts)
        .map(|p| Row {
            verdict: classify(p, drivers, fstab_text),
            existing: fstab::entries(fstab_text).into_iter().filter(|l| fstab::names_partition(&l.spec, p)).collect(),
            part: p.clone(),
        })
        .collect()
}

/// `lsblk` and the rest, in one go: the disks and their rows.
pub fn scan(rn: &dyn Runner, fstab_text: &str) -> Result<(Vec<Disk>, Vec<Row>)> {
    let disks = detect(rn)?;
    let drivers = Drivers::probe(rn);
    let r = rows(&disks, &drivers, fstab_text);
    Ok((disks, r))
}

/// Look up `device` (a path, `UUID=…`, `PARTUUID=…` or `LABEL=…`) among the rows.
pub fn find<'a>(rows: &'a [Row], device: &str) -> Option<&'a Row> {
    rows.iter().find(|r| fstab::names_partition(device, &r.part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::Out;

    const FULL: &str = include_str!("../testdata/lsblk/full_2_41.json");
    const OLD: &str = include_str!("../testdata/lsblk/old_2_34.json");
    const MID: &str = include_str!("../testdata/lsblk/mid_2_38.json");
    const VM: &str = include_str!("../testdata/lsblk/vm_2_42.json");

    const ALL: Drivers = Drivers { ntfs3: true, ntfs3g: false, exfat: true, vfat: true, ext4: true, btrfs: true, xfs: true };

    fn verdicts(json: &str, d: &Drivers, fstab: &str) -> Vec<(String, Verdict)> {
        let disks = parse_lsblk(json).unwrap();
        rows(&disks, d, fstab).into_iter().map(|r| (r.part.name.clone(), r.verdict)).collect()
    }

    fn blocked(v: &[(String, Verdict)], name: &str) -> String {
        match &v.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no row {name}")).1 {
            Verdict::Blocked(r) => r.clone(),
            Verdict::Pick(d) => panic!("{name} is selectable ({d:?})"),
        }
    }

    fn picked(v: &[(String, Verdict)], name: &str) -> Driver {
        match v.iter().find(|(n, _)| n == name).unwrap().1 {
            Verdict::Pick(d) => d,
            _ => panic!("{name} is blocked"),
        }
    }

    #[test]
    fn a_modern_machine_every_row_with_its_reason() {
        let v = verdicts(FULL, &ALL, "UUID=2720fb5d-cbdd-4919-a2e1-8143929ab295 / btrfs defaults 0 0\n");
        assert!(blocked(&v, "nvme0n1p1").contains("in use by the system (mounted at /boot)"));
        assert!(blocked(&v, "nvme0n1p2").contains("in use by the system"));
        assert_eq!(blocked(&v, "nvme0n1p3"), "in use as swap");
        assert_eq!(picked(&v, "nvme0n1p4"), Driver::Ntfs3);
        assert!(blocked(&v, "nvme0n1p5").contains("LUKS"));
        assert_eq!(picked(&v, "nvme0n1p6"), Driver::Btrfs);
        assert!(blocked(&v, "nvme0n1p7").contains("no filesystem"));
        assert_eq!(picked(&v, "sdb1"), Driver::Exfat, "mounted by udisks under /run/media is not the system");
        assert!(blocked(&v, "sr0").contains("read-only media") || blocked(&v, "sr0").contains("iso9660"));
        assert!(blocked(&v, "sdc").contains("read-only device"));
        assert_eq!(v.len(), 10, "{v:?}");
    }

    #[test]
    fn the_esp_is_recognised_by_its_type_guid_even_when_unmounted() {
        let json = FULL.replace("\"/boot\"", "null");
        let v = verdicts(&json, &ALL, "");
        assert_eq!(blocked(&v, "nvme0n1p1"), "EFI system partition");
    }

    #[test]
    fn disks_are_grouped_with_their_kind() {
        let disks = parse_lsblk(FULL).unwrap();
        let nvme = &disks[0];
        assert_eq!((nvme.path.as_str(), nvme.external, nvme.parts.len()), ("/dev/nvme0n1", false, 7));
        let usb = &disks[1];
        assert!(usb.external);
        let p = &usb.parts[0];
        assert_eq!(
            (p.fstype.as_deref(), p.fsver.as_deref(), p.label.as_deref(), p.uuid.as_deref(), p.external),
            (Some("exfat"), Some("1.0"), Some("STICK"), Some("A40D-85E7"), true)
        );
        assert_eq!(p.mountpoints, ["/run/media/k/STICK"]);
        assert_eq!((p.fsavail, p.fsused), (Some(60000000000), Some(3000000000)));
        assert!(!nvme.parts[3].external);
        assert_eq!(nvme.parts[3].uuid.as_deref(), Some("6C62ADEA62ADB96A"), "case kept as reported");
    }

    #[test]
    fn old_util_linux_with_string_booleans_and_a_single_mountpoint() {
        let disks = parse_lsblk(OLD).unwrap();
        let usb = disks.iter().find(|d| d.path == "/dev/sdb").unwrap();
        assert!(usb.external, "rm and hotplug as \"1\"");
        assert_eq!(usb.size, 64000000000, "size as a string");
        assert_eq!(usb.parts[0].mountpoints, ["/run/media/k/STICK"]);
        let v = verdicts(OLD, &ALL, "");
        assert_eq!(picked(&v, "sdb1"), Driver::Exfat);
        assert_eq!(picked(&v, "nvme0n1p4"), Driver::Ntfs3);
        assert!(blocked(&v, "nvme0n1p5").contains("LUKS"));
        assert_eq!(blocked(&v, "nvme0n1p3"), "swap space", "no mount point on this one: the swap type is the reason");
    }

    #[test]
    fn util_linux_2_38_without_a_parttype_column() {
        let v = verdicts(MID, &ALL, "");
        assert_eq!(picked(&v, "nvme0n1p4"), Driver::Ntfs3);
        assert_eq!(picked(&v, "nvme0n1p6"), Driver::Btrfs);
        // no PARTTYPE: the ESP is still the system's through its mount point
        assert!(blocked(&v, "nvme0n1p1").contains("/boot"));
    }

    #[test]
    fn the_live_capture_of_a_vm() {
        let disks = parse_lsblk(VM).unwrap();
        assert_eq!(disks.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(), ["/dev/sda", "/dev/sr0", "/dev/zram0"]);
        let v = verdicts(VM, &ALL, "");
        assert_eq!(v.len(), disks.iter().map(|d| d.parts.len()).sum::<usize>());
        assert!(blocked(&v, "sr0").contains("read-only media"));
        assert!(blocked(&v, "zram0").contains("swap"));
        assert!(blocked(&v, "sda2").contains("/boot") || blocked(&v, "sda2").contains("EFI"));
        assert!(blocked(&v, "sda3").contains("in use by the system"));
        assert!(blocked(&v, "sda1").contains("no filesystem"));
    }

    #[test]
    fn ntfs_driver_choice_and_missing_drivers() {
        let only_3g = Drivers { ntfs3: false, ntfs3g: true, ..ALL };
        assert_eq!(picked(&verdicts(FULL, &only_3g, ""), "nvme0n1p4"), Driver::Ntfs3g);
        let none = Drivers { ntfs3: false, ntfs3g: false, exfat: false, ..ALL };
        let v = verdicts(FULL, &none, "");
        assert!(blocked(&v, "nvme0n1p4").contains("neither is available"));
        assert!(blocked(&v, "sdb1").contains("no exFAT driver"));
        assert_eq!(picked(&v, "nvme0n1p6"), Driver::Btrfs);
    }

    #[test]
    fn a_system_fstab_entry_blocks_the_partition_but_a_data_entry_does_not() {
        let fstab = "UUID=6C62ADEA62ADB96A /home ntfs3 defaults 0 0\nUUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/vault btrfs defaults 0 0\n";
        let disks = parse_lsblk(FULL).unwrap();
        let rows = rows(&disks, &ALL, fstab);
        let ntfs = rows.iter().find(|r| r.part.name == "nvme0n1p4").unwrap();
        assert_eq!(ntfs.reason(), Some("listed in /etc/fstab for /home"));
        let btrfs = rows.iter().find(|r| r.part.name == "nvme0n1p6").unwrap();
        assert_eq!(btrfs.driver(), Some(Driver::Btrfs));
        assert_eq!(btrfs.existing.len(), 1, "offered as an entry to replace");
        assert_eq!(btrfs.existing[0].file, "/media/vault");
    }

    #[test]
    fn partitions_without_any_id_and_unsupported_filesystems() {
        let mut p = Partition { path: "/dev/sdx1".into(), fstype: Some("ext4".into()), ..Default::default() };
        assert!(matches!(classify(&p, &ALL, ""), Verdict::Blocked(r) if r.contains("no UUID")));
        p.partuuid = Some("abcd-01".into());
        assert_eq!(classify(&p, &ALL, ""), Verdict::Pick(Driver::Ext4));
        for (t, word) in [
            ("BitLocker", "BitLocker"),
            ("LVM2_member", "LVM"),
            ("linux_raid_member", "RAID"),
            ("zfs_member", "ZFS"),
            ("swap", "swap"),
            ("udf", "read-only"),
            ("f2fs", "f2fs"),
        ] {
            p.fstype = Some(t.into());
            assert!(matches!(classify(&p, &ALL, ""), Verdict::Blocked(r) if r.contains(word)), "{t}");
        }
    }

    #[test]
    fn blkid_fills_only_the_gaps() {
        let mut p = Partition { fstype: Some("ntfs".into()), uuid: Some("KEEP".into()), ..Default::default() };
        merge_blkid(
            &mut p,
            "DEVNAME=/dev/x\nTYPE=exfat\nUUID=OTHER\nLABEL=Stick\nVERSION=1.0\nPART_ENTRY_UUID=abc\nPART_ENTRY_TYPE=C12A7328-F81F-11D2-BA4B-00A0C93EC93B\nEMPTY=\n",
        );
        assert_eq!((p.fstype.as_deref(), p.uuid.as_deref()), (Some("ntfs"), Some("KEEP")));
        assert_eq!((p.label.as_deref(), p.fsver.as_deref(), p.partuuid.as_deref()), (Some("Stick"), Some("1.0"), Some("abc")));
        assert_eq!(p.parttype.as_deref(), Some(ESP_GUID));
    }

    struct Scripted {
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Runner for Scripted {
        fn run(&self, prog: &str, args: &[&str]) -> Out {
            self.calls.borrow_mut().push(format!("{prog} {}", args.join(" ")));
            match prog {
                "lsblk" if args[3].contains("FSVER") => Out::failure("lsblk: unknown column: FSVER"),
                "lsblk" => Out::success(MID),
                "blkid" => Out::success("TYPE=ntfs\nUUID=FILLED\n"),
                _ => Out::failure("no"),
            }
        }

        fn run_as_user(&self, _: &[&str]) -> Out {
            Out::failure("no")
        }
    }

    #[test]
    fn an_lsblk_without_some_columns_is_retried_and_blkid_fills_gaps() {
        let rn = Scripted { calls: Default::default() };
        let disks = detect(&rn).unwrap();
        let calls = rn.calls.borrow();
        assert!(calls[0].contains("FSVER") && !calls[1].contains("FSVER"), "{calls:?}");
        let unformatted = disks[0].parts.iter().find(|p| p.name == "nvme0n1p7").unwrap();
        assert_eq!(unformatted.fstype.as_deref(), Some("ntfs"), "blkid found a filesystem lsblk didn't");
    }

    #[test]
    fn drivers_from_proc_and_modinfo() {
        let proc = "nodev\tsysfs\n\text4\n\tbtrfs\nnodev\ttmpfs\n";
        let d = Drivers::from(proc, |n| n == "ntfs3" || n == "vfat", false);
        assert_eq!((d.ext4, d.btrfs, d.ntfs3, d.vfat, d.exfat, d.xfs), (true, true, true, true, false, false));
        assert_eq!(d.driver(Fs::Ntfs), Some(Driver::Ntfs3));
        assert_eq!(d.driver(Fs::Xfs), None);
    }

    #[test]
    fn find_by_path_uuid_or_label() {
        let disks = parse_lsblk(FULL).unwrap();
        let r = rows(&disks, &ALL, "");
        assert_eq!(find(&r, "/dev/sdb1").unwrap().part.name, "sdb1");
        assert_eq!(find(&r, "UUID=A40D-85E7").unwrap().part.name, "sdb1");
        assert_eq!(find(&r, "LABEL=Data").unwrap().part.name, "nvme0n1p4");
        assert!(find(&r, "/dev/nope").is_none());
    }
}
