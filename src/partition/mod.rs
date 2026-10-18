//! The data partition step: detect partitions, write a correct `/etc/fstab` entry, mount it
//! and undo all of it. See `fstab.rs` for the design note (the rules and the man pages they
//! come from).
//!
//! - `detect`: `lsblk`/`blkid` parsing, which partitions can be picked and why not
//! - `fstab`: the entry per filesystem, escaping, option validation, fstab editing
//! - `apply`: test mount, install, mount, verify, roll back; the home folder moves
//! - `cli`: `kast partition`

pub mod apply;
pub mod detect;
#[cfg(test)]
pub mod fake;
pub mod fstab;
pub mod undo;

use std::process::{Command, Stdio};

/// What a program printed and how it ended.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn success(stdout: &str) -> Out {
        Out { ok: true, code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    pub fn failure(stderr: &str) -> Out {
        Out { ok: false, code: Some(1), stdout: String::new(), stderr: stderr.into() }
    }

    /// stderr, or stdout when stderr is empty: the message to show for a failed command.
    pub fn message(&self) -> String {
        let m = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        m.trim().to_string()
    }
}

/// Runs the system tools (`mount`, `umount`, `findmnt`, `systemctl`, `blkid`, ...). Tests
/// swap in a fake so nothing real is ever mounted.
pub trait Runner {
    fn run(&self, prog: &str, args: &[&str]) -> Out;

    /// Run `args` as the target user (the real write test of the mounted partition).
    fn run_as_user(&self, args: &[&str]) -> Out;
}

/// The real thing.
pub struct SysRunner;

fn capture(mut cmd: Command) -> Out {
    match cmd.stdin(Stdio::null()).output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Out { ok: false, code: None, stdout: String::new(), stderr: e.to_string() },
    }
}

impl Runner for SysRunner {
    fn run(&self, prog: &str, args: &[&str]) -> Out {
        let mut c = Command::new(prog);
        c.args(args);
        capture(c)
    }

    fn run_as_user(&self, args: &[&str]) -> Out {
        capture(crate::ctx::run_as_user(args))
    }
}

/// The filesystems kast can set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fs {
    Ntfs,
    Exfat,
    Vfat,
    Ext4,
    Btrfs,
    Xfs,
}

impl Fs {
    /// From what `lsblk`/`blkid` call it (`TYPE`).
    pub fn from_type(t: &str) -> Option<Fs> {
        match t {
            "ntfs" | "ntfs3" => Some(Fs::Ntfs),
            "exfat" => Some(Fs::Exfat),
            "vfat" => Some(Fs::Vfat),
            "ext4" => Some(Fs::Ext4),
            "btrfs" => Some(Fs::Btrfs),
            "xfs" => Some(Fs::Xfs),
            _ => None,
        }
    }

    /// No Unix owners or modes on disk: ownership comes from mount options.
    pub fn foreign(self) -> bool {
        matches!(self, Fs::Ntfs | Fs::Exfat | Fs::Vfat)
    }

    pub fn name(self) -> &'static str {
        match self {
            Fs::Ntfs => "NTFS",
            Fs::Exfat => "exFAT",
            Fs::Vfat => "FAT",
            Fs::Ext4 => "ext4",
            Fs::Btrfs => "btrfs",
            Fs::Xfs => "XFS",
        }
    }
}

/// The kernel (or FUSE) driver an entry uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Ntfs3,
    Ntfs3g,
    Exfat,
    Vfat,
    Ext4,
    Btrfs,
    Xfs,
}

impl Driver {
    pub fn fs(self) -> Fs {
        match self {
            Driver::Ntfs3 | Driver::Ntfs3g => Fs::Ntfs,
            Driver::Exfat => Fs::Exfat,
            Driver::Vfat => Fs::Vfat,
            Driver::Ext4 => Fs::Ext4,
            Driver::Btrfs => Fs::Btrfs,
            Driver::Xfs => Fs::Xfs,
        }
    }

    /// The `fs_vfstype` field.
    pub fn fstab_type(self) -> &'static str {
        match self {
            Driver::Ntfs3 => "ntfs3",
            Driver::Ntfs3g => "ntfs-3g",
            Driver::Exfat => "exfat",
            Driver::Vfat => "vfat",
            Driver::Ext4 => "ext4",
            Driver::Btrfs => "btrfs",
            Driver::Xfs => "xfs",
        }
    }
}

/// A partition (or a whole disk with a filesystem on it), as `lsblk` reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Partition {
    pub path: String,
    pub name: String,
    /// The disk it is on (its own path for a whole-disk filesystem)
    pub disk: String,
    pub whole_disk: bool,
    pub fstype: Option<String>,
    pub fsver: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub partuuid: Option<String>,
    pub partlabel: Option<String>,
    pub parttype: Option<String>,
    pub size: u64,
    pub fsavail: Option<u64>,
    pub fsused: Option<u64>,
    pub mountpoints: Vec<String>,
    pub ro: bool,
    /// On a USB / removable / hot-pluggable disk
    pub external: bool,
    pub tran: Option<String>,
    pub model: Option<String>,
}

impl Partition {
    pub fn fs(&self) -> Option<Fs> {
        self.fstype.as_deref().and_then(Fs::from_type)
    }

    /// "Data (/dev/sdb1)" for messages and the fstab comment.
    pub fn title(&self) -> String {
        match self.label.as_deref().filter(|l| !l.is_empty()) {
            Some(l) => l.to_string(),
            None => self.path.clone(),
        }
    }
}

/// "12.3 GiB"
pub fn human(bytes: u64) -> String {
    crate::undo::human(bytes)
}
