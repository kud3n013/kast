//! The `/etc/fstab` entry for a data partition: building it, escaping it, validating options,
//! editing the file, and checking the result.
//!
//! # Design note: where the rules come from
//!
//! A wrong fstab can stop the machine from booting, so every rule below is taken from the
//! documentation, and an option is only emitted when a source confirms the driver takes it.
//!
//! - **Six fields, explicit** (`fstab(5)`): `fs_spec fs_file fs_vfstype fs_mntops fs_freq
//!   fs_passno`. A space in a field is `\040`, a tab `\011`; a backslash is written `\134` as
//!   well, because getmntent decodes octal escapes. All six are always written.
//! - **Tags, never `/dev/sdX`** (`mount(8)`, "UUID="): `UUID=` is compared as a string, so the
//!   value is kept exactly as `blkid`/`lsblk` report it (FAT/NTFS IDs are uppercase, ext4,
//!   btrfs and xfs lowercase). `PARTUUID=` only when the filesystem has no UUID, and then the
//!   entry carries a note.
//! - **`fs_passno`** (`fstab(5)`, `fsck.xfs(8)`, `fsck.btrfs(8)`): 1 is the root filesystem
//!   only. NTFS, exFAT and FAT are never checked at boot: 0. ext4 and xfs: 2, the documented
//!   convention for other filesystems (`fsck.xfs` is a successful no-op, so 2 costs nothing).
//!   btrfs: 0, `fsck.btrfs(8)` says "You should set fs_passno to 0". `fs_freq` is 0.
//! - **`nofail`** (`mount(8)`, `systemd.mount(5)`): without it the mount is *required* by
//!   `local-fs.target` and a missing device drops boot into emergency mode; with it the mount
//!   is only *wanted*. Every entry kast writes has it.
//! - **`x-systemd.device-timeout=`** (`systemd.mount(5)`, `systemd-system.conf(5)`): without
//!   it systemd waits `DefaultDeviceTimeoutSec` (90 s) for an absent device at boot. 5 s for
//!   external drives, which are often not plugged in; 10 s for internal ones, which are
//!   normally there but may take a moment to appear.
//! - **External drives** get `x-systemd.automount` and `x-systemd.idle-timeout=5min`
//!   (`systemd.mount(5)`): mounted on first access, unmounted when idle, so unplugging an
//!   idle drive is safe. Internal drives are mounted at boot. The user can flip either.
//! - **Permissions on filesystems without Unix owners** (`ntfs3.rst`, `vfat.rst`,
//!   `fs/exfat/super.c`): `uid=`, `gid=`, `dmask=` and `fmask=` exist on all of them.
//!   `dmask`/`fmask` instead of `umask` so files don't get the execute bit.
//! - **ntfs3** (`ntfs3.rst`): `iocharset=`, `windows_names`, `prealloc` are documented;
//!   `noatime` is not an ntfs3 option but a generic VFS flag (`mount(8)`). `force`, which
//!   mounts a dirty volume, is never emitted and never accepted.
//! - **vfat** (`vfat.rst`): `utf8` is the documented way to get UTF-8 names (the file advises
//!   against `iocharset=utf8`). `shortname=mixed` and `errors=remount-ro` are the defaults, so
//!   they are left out. **exFAT**: the default `iocharset` is a kernel build option, so none
//!   is written; its `utf8` option is deprecated and ignored.
//! - **ext4, xfs**: no `uid`/`gid`/`umask` (ext4 rejects them); ownership is a chown of the
//!   mount root. **btrfs** (`btrfs(5)`): `subvol=<path>` is relative to the top-level
//!   subvolume, `compress=zstd:1` only affects new writes.
//! - **`defaults`** is not needed once other options are present, so it is not written.
//! - **Verification** (`mount(8)`, `findmnt(8)`): `findmnt --verify --verbose --tab-file` runs
//!   on the candidate before it replaces `/etc/fstab`. It exits 1 on errors, but it also
//!   reports problems in lines kast didn't write, so the result is compared with the same check
//!   on the current file. It does *not* check that the driver accepts the options: the test
//!   mount does. Then `systemctl daemon-reload` so `systemd-fstab-generator` regenerates the
//!   mount units. Never `mount -a`: only the new entry is mounted.

use std::collections::HashMap;
use std::fmt;

use super::{Driver, Out, Partition, Runner};

// --- Choices and the entry ---------------------------------------------------------------

/// Permission presets for filesystems without Unix permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Owned by the user, nobody else gets in, files not executable
    #[default]
    OnlyMe,
    /// Others can read
    OthersRead,
    /// Owned by the user, files executable (scripts, AppImages)
    FilesExecutable,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::OnlyMe, Preset::OthersRead, Preset::FilesExecutable];

    pub fn masks(self) -> (&'static str, &'static str) {
        match self {
            Preset::OnlyMe => ("0077", "0177"),
            Preset::OthersRead => ("0022", "0133"),
            Preset::FilesExecutable => ("0077", "0077"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::OnlyMe => "Only me",
            Preset::OthersRead => "Me, others can read",
            Preset::FilesExecutable => "Me, files executable",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Preset::OnlyMe => "owned by you; nobody else can open it; files are not executable",
            Preset::OthersRead => "owned by you; other users can read and list",
            Preset::FilesExecutable => "owned by you; files are executable (scripts, AppImages)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Mounted at boot
    Boot,
    /// Mounted on first access, unmounted when idle (`x-systemd.automount`)
    Automount,
}

/// What the user decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Choices {
    pub driver: Driver,
    pub mountpoint: String,
    pub mode: Mode,
    pub read_only: bool,
    /// Picks the device timeout (5 s external, 10 s internal)
    pub external: bool,
    pub preset: Preset,
    pub uid: u32,
    pub gid: u32,
    pub noatime: bool,
    pub prealloc: bool,
    pub compress: bool,
    /// btrfs subvolume, as listed (`/@data`); None = the default subvolume
    pub subvol: Option<String>,
    /// chown the top folder to the user (ext4/btrfs/xfs)
    pub own_top_folder: bool,
    /// The filesystem options as edited by hand (replaces the generated ones)
    pub manual: Option<Vec<String>>,
}

impl Choices {
    /// The defaults for `p`: boot or first access by internal/external, "Only me".
    pub fn new(p: &Partition, driver: Driver, mountpoint: &str, uid: u32, gid: u32) -> Choices {
        Choices {
            driver,
            mountpoint: mountpoint.into(),
            mode: if p.external { Mode::Automount } else { Mode::Boot },
            read_only: false,
            external: p.external,
            preset: Preset::default(),
            uid,
            gid,
            noatime: false,
            prealloc: false,
            compress: false,
            subvol: None,
            own_top_folder: false,
            manual: None,
        }
    }
}

pub const IDLE_TIMEOUT: &str = "5min";

/// One fstab line, as fields. `Display` writes it escaped.
#[derive(Debug, Clone, PartialEq)]
pub struct FstabEntry {
    pub spec: String,
    pub file: String,
    pub vfstype: String,
    pub options: Vec<String>,
    pub freq: u8,
    pub passno: u8,
    /// Things to tell the user (e.g. "no filesystem UUID: using PARTUUID")
    pub notes: Vec<String>,
}

impl fmt::Display for FstabEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let opts: Vec<String> = self.options.iter().map(|o| escape_field(o)).collect();
        write!(f, "{} {} {} {} {} {}", escape_field(&self.spec), escape_field(&self.file), escape_field(&self.vfstype), opts.join(","), self.freq, self.passno)
    }
}

impl FstabEntry {
    /// The options `mount` takes on the command line: the systemd-only ones and `nofail`
    /// belong to fstab.
    pub fn mount_options(&self) -> Vec<String> {
        self.options.iter().filter(|o| *o != "nofail" && !o.starts_with("x-systemd.")).cloned().collect()
    }

    /// The comment line written above it.
    pub fn comment(p: &Partition, date: &str) -> String {
        format!("# kast: data partition ({}), added {date}", escape_comment(&p.title()))
    }
}

fn escape_comment(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// An ordered list without duplicates (the same name twice keeps the first).
#[derive(Default)]
struct Opts(Vec<String>);

impl Opts {
    fn push(&mut self, o: impl Into<String>) {
        let o = o.into();
        let key = |s: &str| s.split('=').next().unwrap_or_default().to_string();
        if !self.0.iter().any(|e| key(e) == key(&o)) {
            self.0.push(o);
        }
    }
}

/// The filesystem-specific options, before `nofail` and the systemd ones. Also what the
/// "edit options manually" row starts from.
pub fn driver_options(c: &Choices) -> Vec<String> {
    let mut o = Opts::default();
    let masks = |o: &mut Opts| {
        let (d, f) = c.preset.masks();
        o.push(format!("uid={}", c.uid));
        o.push(format!("gid={}", c.gid));
        o.push(format!("dmask={d}"));
        o.push(format!("fmask={f}"));
    };
    match c.driver {
        Driver::Ntfs3 => {
            masks(&mut o);
            o.push("iocharset=utf8");
            o.push("windows_names");
            if c.noatime {
                o.push("noatime");
            }
            if c.prealloc {
                o.push("prealloc");
            }
        }
        Driver::Ntfs3g => {
            masks(&mut o);
            o.push("windows_names");
            if c.noatime {
                o.push("noatime");
            }
        }
        Driver::Exfat => {
            masks(&mut o);
            if c.noatime {
                o.push("noatime");
            }
        }
        Driver::Vfat => {
            masks(&mut o);
            o.push("utf8");
            if c.noatime {
                o.push("noatime");
            }
        }
        Driver::Ext4 | Driver::Xfs => {
            if c.noatime {
                o.push("noatime");
            }
        }
        Driver::Btrfs => {
            if let Some(s) = &c.subvol {
                o.push(format!("subvol={s}"));
            }
            if c.compress {
                o.push("compress=zstd:1");
            }
            if c.noatime {
                o.push("noatime");
            }
        }
    }
    o.0
}

fn passno(d: Driver) -> u8 {
    match d {
        Driver::Ext4 | Driver::Xfs => 2,
        _ => 0,
    }
}

/// The fstab entry for `p` with `c`. Pure: no I/O.
pub fn fstab_entry(p: &Partition, c: &Choices) -> FstabEntry {
    let mut notes = Vec::new();
    let spec = match (p.uuid.as_deref().filter(|u| !u.is_empty()), p.partuuid.as_deref().filter(|u| !u.is_empty())) {
        (Some(u), _) => format!("UUID={u}"),
        (None, Some(u)) => {
            notes.push("the filesystem has no UUID: the entry uses PARTUUID, which stays the same as long as the partition table does".to_string());
            format!("PARTUUID={u}")
        }
        (None, None) => {
            notes.push("no UUID or PARTUUID: the device name can change between boots".to_string());
            p.path.clone()
        }
    };
    let mut o = Opts::default();
    if c.read_only {
        o.push("ro");
    }
    for x in c.manual.clone().unwrap_or_else(|| driver_options(c)) {
        o.push(x);
    }
    o.push("nofail");
    o.push(format!("x-systemd.device-timeout={}", if c.external { "5s" } else { "10s" }));
    if c.mode == Mode::Automount {
        o.push("x-systemd.automount");
        o.push(format!("x-systemd.idle-timeout={IDLE_TIMEOUT}"));
    }
    FstabEntry { spec, file: c.mountpoint.clone(), vfstype: c.driver.fstab_type().into(), options: o.0, freq: 0, passno: passno(c.driver), notes }
}

// --- Escaping -----------------------------------------------------------------------------

/// A field as fstab(5) wants it: space `\040`, tab `\011`, newline `\012`, backslash `\134`.
pub fn escape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            c => out.push(c),
        }
    }
    out
}

/// What getmntent does with a field: `\ooo` (three octal digits) becomes that byte.
pub fn unescape_field(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = |k: usize| b.get(i + k).is_some_and(|c| (b'0'..=b'7').contains(c));
        if b[i] == b'\\' && oct(1) && oct(2) && oct(3) {
            out.push(((b[i + 1] - b'0') << 6) | ((b[i + 2] - b'0') << 3) | (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- Parsing the file ---------------------------------------------------------------------

/// An entry of `/etc/fstab` (decoded).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// 1-based
    pub no: usize,
    pub raw: String,
    pub spec: String,
    pub file: String,
    pub vfstype: String,
    pub options: String,
}

/// The entries of `text`: blank lines and comments are skipped.
pub fn entries(text: &str) -> Vec<Line> {
    text.split('\n')
        .enumerate()
        .filter_map(|(i, raw)| {
            let t = raw.trim();
            if t.is_empty() || t.starts_with('#') {
                return None;
            }
            let mut f = t.split_whitespace();
            let (spec, file) = (f.next()?, f.next()?);
            Some(Line {
                no: i + 1,
                raw: raw.trim_end_matches('\r').to_string(),
                spec: unescape_field(spec),
                file: unescape_field(file),
                vfstype: f.next().map(unescape_field).unwrap_or_default(),
                options: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Paths the system runs from; never offered as a mount point, never a partition to take.
pub fn is_system_path(p: &str) -> bool {
    const EXACT: [&str; 11] = ["/", "/home", "/root", "/opt", "/srv", "/etc", "/usr", "/var", "/boot", "/efi", "/boot/efi"];
    EXACT.contains(&p) || ["/usr/", "/var/", "/boot/", "/etc/", "/efi/"].iter().any(|d| p.starts_with(d))
}

fn tag_eq(a: &str, b: &Option<String>) -> bool {
    b.as_deref().is_some_and(|b| !b.is_empty() && a.trim_matches('"').eq_ignore_ascii_case(b))
}

/// Does this entry's source name `p` (by UUID, PARTUUID, LABEL, PARTLABEL or device path)?
pub fn names_partition(spec: &str, p: &Partition) -> bool {
    let spec = spec.trim();
    if let Some(v) = spec.strip_prefix("UUID=") {
        return tag_eq(v, &p.uuid);
    }
    if let Some(v) = spec.strip_prefix("PARTUUID=") {
        return tag_eq(v, &p.partuuid);
    }
    if let Some(v) = spec.strip_prefix("LABEL=") {
        return p.label.as_deref().is_some_and(|l| l == v.trim_matches('"'));
    }
    if let Some(v) = spec.strip_prefix("PARTLABEL=") {
        return p.partlabel.as_deref().is_some_and(|l| l == v.trim_matches('"'));
    }
    if let Some(v) = spec.strip_prefix("/dev/disk/by-uuid/") {
        return tag_eq(v, &p.uuid);
    }
    if let Some(v) = spec.strip_prefix("/dev/disk/by-partuuid/") {
        return tag_eq(v, &p.partuuid);
    }
    if let Some(v) = spec.strip_prefix("/dev/disk/by-label/") {
        return p.label.as_deref().is_some_and(|l| l == unescape_field(&v.replace("\\x20", " ")));
    }
    spec == p.path
}

/// A mount point compared the way the kernel would: no trailing slashes.
fn same_dir(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        let t = s.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    };
    n(a) == n(b)
}

/// Why an existing entry is in the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub line: Line,
    pub same_device: bool,
    pub same_mountpoint: bool,
}

impl Conflict {
    pub fn why(&self) -> &'static str {
        match (self.same_device, self.same_mountpoint) {
            (true, true) => "the same partition at the same folder",
            (true, false) => "the same partition, mounted at another folder",
            _ => "another partition at this folder",
        }
    }
}

/// The entries that name this partition or this mount point.
pub fn conflicts(text: &str, p: &Partition, mountpoint: &str) -> Vec<Conflict> {
    entries(text)
        .into_iter()
        .filter_map(|line| {
            let same_device = names_partition(&line.spec, p);
            let same_mountpoint = same_dir(&line.file, mountpoint);
            (same_device || same_mountpoint).then_some(Conflict { line, same_device, same_mountpoint })
        })
        .collect()
}

/// Entries for `p` at a system path: the partition is part of the system.
pub fn system_entry(text: &str, p: &Partition) -> Option<Line> {
    entries(text).into_iter().find(|l| names_partition(&l.spec, p) && is_system_path(&l.file))
}

// --- Editing the file ---------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct Edit {
    pub text: String,
    /// The lines taken out (without line ending)
    pub replaced: Vec<String>,
}

fn eol_of(text: &str) -> &'static str {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// `text` with the entry appended (after its comment line). `replace` lists the 1-based
/// numbers of the conflicting lines the user agreed to replace; a conflict that isn't listed
/// is an error, so there are never two entries for one device or one mount point. Every
/// other byte stays as it was.
pub fn add_entry(text: &str, p: &Partition, entry: &FstabEntry, comment: &str, replace: &[usize]) -> Result<Edit, String> {
    let found = conflicts(text, p, &entry.file);
    if let Some(c) = found.iter().find(|c| !replace.contains(&c.line.no)) {
        return Err(format!("/etc/fstab already has an entry for {} (line {}): {}", c.why(), c.line.no, c.line.raw));
    }
    let eol = eol_of(text);
    let mut out = String::with_capacity(text.len() + 256);
    let mut replaced = Vec::new();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    for (i, l) in lines.iter().enumerate() {
        let body = l.trim_end_matches(['\n', '\r']);
        if replace.contains(&(i + 1)) && found.iter().any(|c| c.line.no == i + 1) {
            replaced.push(body.to_string());
            // Our own comment above a replaced line goes with it
            if out.ends_with('\n') || out.is_empty() {
                let last_start = out.trim_end_matches(['\n', '\r']).rfind('\n').map_or(0, |k| k + 1);
                if out[last_start..].trim_end_matches(['\n', '\r']).starts_with("# kast: data partition") {
                    out.truncate(last_start);
                }
            }
            continue;
        }
        out.push_str(l);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(comment);
    out.push_str(eol);
    out.push_str(&entry.to_string());
    out.push_str(eol);
    Ok(Edit { text: out, replaced })
}

/// Undo of `add_entry` on a file that was edited since: take out exactly kast's line and its
/// comment, and put the replaced lines back (at the end, where kast's line was).
pub fn remove_entry(text: &str, line: &str, comment: &str, replaced: &[String]) -> String {
    let eol = eol_of(text);
    let mut kept: Vec<&str> = Vec::new();
    let mut removed = false;
    for l in text.split_inclusive('\n') {
        let body = l.trim_end_matches(['\n', '\r']);
        if body == line && !removed {
            removed = true;
            if kept.last().is_some_and(|k| k.trim_end_matches(['\n', '\r']) == comment) {
                kept.pop();
            }
            continue;
        }
        kept.push(l);
    }
    let mut out: String = kept.concat();
    if removed {
        for r in replaced {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(r);
            out.push_str(eol);
        }
    }
    out
}

// --- Manual options -----------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Val {
    Num,
    Octal,
    Word,
    /// Any text without commas (a subvolume path)
    Text,
    Compress,
    OneOf(&'static [&'static str]),
}

struct Allowed {
    flags: &'static [&'static str],
    values: &'static [(&'static str, Val)],
}

/// Generic mount(8) options that are fine on every filesystem here.
const GENERIC: Allowed = Allowed { flags: &["noatime", "relatime", "nodiratime", "lazytime", "nosuid", "nodev", "noexec"], values: &[] };

const ERRORS3: Val = Val::OneOf(&["continue", "panic", "remount-ro"]);

fn allowed(d: Driver) -> Allowed {
    match d {
        Driver::Ntfs3 => Allowed {
            flags: &["windows_names", "nohidden", "sys_immutable", "hide_dot_files", "discard", "sparse", "showmeta", "prealloc", "acl"],
            values: &[("uid", Val::Num), ("gid", Val::Num), ("umask", Val::Octal), ("dmask", Val::Octal), ("fmask", Val::Octal), ("iocharset", Val::Word)],
        },
        Driver::Ntfs3g => Allowed {
            flags: &["windows_names"],
            values: &[("uid", Val::Num), ("gid", Val::Num), ("umask", Val::Octal), ("dmask", Val::Octal), ("fmask", Val::Octal)],
        },
        Driver::Exfat => Allowed {
            flags: &["discard", "keep_last_dots", "sys_tz", "zero_size_dir"],
            values: &[
                ("uid", Val::Num),
                ("gid", Val::Num),
                ("umask", Val::Octal),
                ("dmask", Val::Octal),
                ("fmask", Val::Octal),
                ("allow_utime", Val::Octal),
                ("iocharset", Val::Word),
                ("errors", ERRORS3),
                ("time_offset", Val::Num),
            ],
        },
        Driver::Vfat => Allowed {
            flags: &["utf8", "uni_xlate", "nonumtail", "usefree", "quiet", "flush", "discard", "showexec", "sys_immutable", "rodir"],
            values: &[
                ("uid", Val::Num),
                ("gid", Val::Num),
                ("umask", Val::Octal),
                ("dmask", Val::Octal),
                ("fmask", Val::Octal),
                ("allow_utime", Val::Octal),
                ("codepage", Val::Num),
                ("iocharset", Val::Word),
                ("shortname", Val::OneOf(&["lower", "win95", "winnt", "mixed"])),
                ("check", Val::OneOf(&["s", "r", "n"])),
                ("tz", Val::OneOf(&["UTC"])),
                ("time_offset", Val::Num),
                ("errors", Val::OneOf(&["panic", "continue", "remount-ro"])),
            ],
        },
        Driver::Ext4 => Allowed {
            flags: &["discard", "nodiscard", "user_xattr", "nouser_xattr", "acl", "noacl"],
            values: &[("commit", Val::Num), ("data", Val::OneOf(&["ordered", "writeback", "journal"])), ("errors", ERRORS3)],
        },
        Driver::Btrfs => Allowed {
            flags: &["autodefrag", "noautodefrag", "discard", "ssd", "nossd"],
            values: &[
                ("subvol", Val::Text),
                ("subvolid", Val::Num),
                ("compress", Val::Compress),
                ("commit", Val::Num),
                ("discard", Val::OneOf(&["async", "sync"])),
            ],
        },
        Driver::Xfs => Allowed {
            flags: &["discard", "nodiscard", "largeio", "nolargeio", "swalloc", "inode64"],
            values: &[("allocsize", Val::Word), ("logbufs", Val::Num), ("logbsize", Val::Word)],
        },
    }
}

fn check_value(v: Val, value: &str) -> bool {
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':'));
    match v {
        Val::Num => !value.is_empty() && value.len() <= 10 && value.chars().all(|c| c.is_ascii_digit()),
        Val::Octal => (3..=4).contains(&value.len()) && value.chars().all(|c| ('0'..='7').contains(&c)),
        Val::Word => word(value),
        Val::Text => !value.is_empty() && !value.contains([',', '\n', '\0']),
        Val::Compress => {
            let (alg, level) = value.split_once(':').map_or((value, None), |(a, l)| (a, Some(l)));
            ["zstd", "zlib", "lzo", "no", "none"].contains(&alg) && level.is_none_or(|l| l.parse::<u8>().is_ok_and(|n| (1..=15).contains(&n)))
        }
        Val::OneOf(set) => set.contains(&value),
    }
}

/// Why an option is refused, when it is one kast must never write.
fn forbidden(name: &str) -> Option<&'static str> {
    match name {
        "force" => Some("`force` mounts a volume that Windows left dirty, which can damage it"),
        "exec" => Some("`exec` makes every file executable; use the \"files executable\" preset instead"),
        "defaults" => Some("`defaults` is not needed"),
        "ro" | "rw" => Some("use the read-only switch"),
        "nofail" => Some("`nofail` is always added"),
        "auto" | "noauto" | "user" | "users" | "owner" | "group" | "_netdev" | "bind" | "rbind" | "remount" => Some("not for a data partition"),
        n if n.starts_with("x-systemd.") => Some("the systemd options follow the \"mount at boot / on first access\" switch"),
        _ => None,
    }
}

/// The options typed by hand (comma-separated, `\040` for a space) checked against what
/// `driver` accepts. Returns them decoded.
pub fn validate_manual(driver: Driver, input: &str) -> Result<Vec<String>, String> {
    if input.chars().any(char::is_whitespace) {
        return Err("no spaces or tabs: write a space inside a value as \\040".into());
    }
    let ok = allowed(driver);
    let mut out = Vec::new();
    for tok in input.split(',').filter(|t| !t.is_empty()) {
        let raw = unescape_field(tok);
        let (name, value) = match raw.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (raw.as_str(), None),
        };
        if let Some(why) = forbidden(name) {
            return Err(format!("`{name}` is not allowed: {why}"));
        }
        let valid = match value {
            None => ok.flags.contains(&name) || GENERIC.flags.contains(&name),
            Some(v) => ok.values.iter().find(|(n, _)| *n == name).is_some_and(|(_, k)| check_value(*k, v)),
        };
        if !valid {
            let known = ok
                .values
                .iter()
                .map(|(n, _)| format!("{n}="))
                .chain(ok.flags.iter().chain(GENERIC.flags).map(|s| s.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(match value {
                Some(_) if ok.values.iter().any(|(n, _)| *n == name) => format!("`{raw}` has a value {} doesn't accept", driver.fstab_type()),
                _ => format!("`{raw}` is not an option {} accepts here. Known: {known}", driver.fstab_type()),
            });
        }
        if raw.contains('\n') {
            return Err("no line breaks".into());
        }
        out.push(raw);
    }
    if out.is_empty() {
        return Err("no options".into());
    }
    Ok(out)
}

// --- Mount point --------------------------------------------------------------------------

/// The mount point as typed, made absolute-and-tidy: no trailing or repeated slashes. `.` and
/// `..` are refused, as are the system's own paths.
pub fn normalise_mountpoint(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("enter a folder".into());
    }
    if !raw.starts_with('/') {
        return Err("the mount point must be an absolute path (start with /)".into());
    }
    if raw.contains(['\0', '\n', '\r']) {
        return Err("no line breaks in the path".into());
    }
    let parts: Vec<&str> = raw.split('/').filter(|c| !c.is_empty()).collect();
    if parts.iter().any(|c| *c == "." || *c == "..") {
        return Err("`.` and `..` are not allowed in the path".into());
    }
    let path = format!("/{}", parts.join("/"));
    const REFUSED: [&str; 16] =
        ["/", "/boot", "/efi", "/home", "/root", "/usr", "/var", "/etc", "/opt", "/srv", "/tmp", "/run", "/proc", "/sys", "/dev", "/mnt"];
    if REFUSED.contains(&path.as_str()) {
        return Err(format!("{path} is a system folder; pick a folder of its own, e.g. /media/data"));
    }
    if ["/proc/", "/sys/", "/dev/", "/run/"].iter().any(|d| path.starts_with(d)) || is_system_path(&path) {
        return Err(format!("{path} is inside a system folder"));
    }
    Ok(path)
}

/// What exists at the mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spot {
    /// Will be created (and removed again by undo)
    Missing,
    /// An empty folder
    Empty,
}

/// `normalise_mountpoint` plus the checks on the machine: not an existing mount point, a
/// folder, empty. `dir` is where the path is on disk (the same, outside tests).
pub fn check_mountpoint(raw: &str, dir: &std::path::Path, rn: &dyn Runner) -> Result<(String, Spot), String> {
    let path = normalise_mountpoint(raw)?;
    let r = rn.run("findmnt", &["--mountpoint", &path, "-n", "-o", "SOURCE"]);
    if r.ok && !r.stdout.trim().is_empty() {
        return Err(format!("{path} is already a mount point (of {})", r.stdout.trim()));
    }
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((path, Spot::Missing)),
        Err(e) => Err(format!("can't look at {path}: {e}")),
        Ok(m) if !m.is_dir() => Err(format!("{path} exists and is not a folder")),
        Ok(_) => match std::fs::read_dir(dir) {
            Ok(mut it) => {
                if it.next().is_some() {
                    Err(format!("{path} is not empty; files in it would be hidden by the mount"))
                } else {
                    Ok((path, Spot::Empty))
                }
            }
            Err(e) => Err(format!("can't read {path}: {e}")),
        },
    }
}

// --- findmnt --verify ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Issue {
    pub target: String,
    pub error: bool,
    pub msg: String,
}

/// What `findmnt --verify --verbose` printed: the `[E]`/`[W]` lines per target, and how many
/// lines it could not parse.
pub fn parse_verify(out: &str) -> (Vec<Issue>, usize) {
    let mut issues = Vec::new();
    let mut parse_errors = 0;
    let mut target = String::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            let t = line.trim();
            if let Some(m) = t.strip_prefix("[E]") {
                issues.push(Issue { target: target.clone(), error: true, msg: m.trim().into() });
            } else if let Some(m) = t.strip_prefix("[W]") {
                issues.push(Issue { target: target.clone(), error: false, msg: m.trim().into() });
            }
        } else if line.starts_with("findmnt:") {
            if line.contains("parse error") {
                parse_errors += 1;
            }
        } else if !line.starts_with(|c: char| c.is_ascii_digit()) && !line.starts_with("Success") {
            target = line.trim().to_string();
        }
    }
    (issues, parse_errors)
}

#[derive(Debug, Default, PartialEq)]
pub struct VerifyResult {
    /// New problems: kast stops
    pub errors: Vec<String>,
    /// Shown; fine to go on
    pub warnings: Vec<String>,
}

/// Compare the check of the candidate with the check of the current file: only problems that
/// are new, or on our own mount point, count.
pub fn verify_diff(before: &Out, after: &Out, mountpoint: &str) -> VerifyResult {
    let (old, old_parse) = parse_verify(&before.stdout);
    let (new, new_parse) = parse_verify(&after.stdout);
    let mut count: HashMap<&Issue, usize> = HashMap::new();
    for i in &old {
        *count.entry(i).or_default() += 1;
    }
    let mut r = VerifyResult::default();
    if new_parse > old_parse {
        r.errors.push("the new /etc/fstab has a line findmnt can't parse".into());
    }
    for i in &new {
        let existed = count.get_mut(i).is_some_and(|n| {
            if *n > 0 {
                *n -= 1;
                true
            } else {
                false
            }
        });
        let ours = same_dir(&i.target, mountpoint);
        let line = format!("{}: {}", i.target, i.msg);
        if ((!existed || ours) && i.error) || (!existed && i.msg.contains("more than once")) {
            r.errors.push(line);
        } else if !existed || ours {
            r.warnings.push(line);
        }
    }
    // Nothing parsed but it failed: don't wave it through
    if r.errors.is_empty() && !after.ok && new.iter().all(|i| !i.error) && new_parse <= old_parse && before.ok {
        r.errors.push(format!("findmnt --verify failed: {}", after.message()));
    }
    r
}

/// `findmnt --verify` on `file`.
pub fn verify_file(rn: &dyn Runner, file: &std::path::Path) -> Out {
    rn.run("findmnt", &["--verify", "--verbose", "--tab-file", &file.to_string_lossy()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(fstype: &str, uuid: &str) -> Partition {
        Partition {
            path: "/dev/sdb1".into(),
            name: "sdb1".into(),
            disk: "/dev/sdb".into(),
            fstype: Some(fstype.into()),
            uuid: Some(uuid.into()),
            partuuid: Some("1234abcd-01".into()),
            label: Some("Data".into()),
            size: 1 << 30,
            ..Default::default()
        }
    }

    fn choices(p: &Partition, d: Driver) -> Choices {
        Choices::new(p, d, "/media/data", 1000, 1001)
    }

    #[test]
    fn golden_lines_per_filesystem_internal() {
        let cases: [(Driver, &str, &str); 7] = [
            (
                Driver::Ntfs3,
                "0123456789ABCDEF",
                "UUID=0123456789ABCDEF /media/data ntfs3 uid=1000,gid=1001,dmask=0077,fmask=0177,iocharset=utf8,windows_names,nofail,x-systemd.device-timeout=10s 0 0",
            ),
            (
                Driver::Ntfs3g,
                "0123456789ABCDEF",
                "UUID=0123456789ABCDEF /media/data ntfs-3g uid=1000,gid=1001,dmask=0077,fmask=0177,windows_names,nofail,x-systemd.device-timeout=10s 0 0",
            ),
            (Driver::Exfat, "A40D-85E7", "UUID=A40D-85E7 /media/data exfat uid=1000,gid=1001,dmask=0077,fmask=0177,nofail,x-systemd.device-timeout=10s 0 0"),
            (Driver::Vfat, "A40D-85E7", "UUID=A40D-85E7 /media/data vfat uid=1000,gid=1001,dmask=0077,fmask=0177,utf8,nofail,x-systemd.device-timeout=10s 0 0"),
            (
                Driver::Ext4,
                "8b1e2f3a-aaaa-bbbb-cccc-0123456789ab",
                "UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/data ext4 nofail,x-systemd.device-timeout=10s 0 2",
            ),
            (
                Driver::Btrfs,
                "8b1e2f3a-aaaa-bbbb-cccc-0123456789ab",
                "UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/data btrfs nofail,x-systemd.device-timeout=10s 0 0",
            ),
            (
                Driver::Xfs,
                "8b1e2f3a-aaaa-bbbb-cccc-0123456789ab",
                "UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/data xfs nofail,x-systemd.device-timeout=10s 0 2",
            ),
        ];
        for (d, uuid, want) in cases {
            let p = part(d.fs().name(), uuid);
            assert_eq!(fstab_entry(&p, &choices(&p, d)).to_string(), want, "{d:?}");
        }
    }

    #[test]
    fn external_automount_read_only_and_presets() {
        let mut p = part("exfat", "A40D-85E7");
        p.external = true;
        let mut c = choices(&p, Driver::Exfat);
        assert_eq!(c.mode, Mode::Automount, "external drives default to first access");
        assert_eq!(
            fstab_entry(&p, &c).to_string(),
            "UUID=A40D-85E7 /media/data exfat uid=1000,gid=1001,dmask=0077,fmask=0177,nofail,x-systemd.device-timeout=5s,x-systemd.automount,x-systemd.idle-timeout=5min 0 0"
        );
        c.mode = Mode::Boot;
        c.read_only = true;
        c.preset = Preset::OthersRead;
        assert_eq!(
            fstab_entry(&p, &c).to_string(),
            "UUID=A40D-85E7 /media/data exfat ro,uid=1000,gid=1001,dmask=0022,fmask=0133,nofail,x-systemd.device-timeout=5s 0 0"
        );
        c.preset = Preset::FilesExecutable;
        assert!(fstab_entry(&p, &c).to_string().contains("dmask=0077,fmask=0077"));
    }

    #[test]
    fn btrfs_subvolume_compress_and_noatime() {
        let p = part("btrfs", "8b1e2f3a-aaaa-bbbb-cccc-0123456789ab");
        let mut c = choices(&p, Driver::Btrfs);
        c.subvol = Some("/@data".into());
        c.compress = true;
        c.noatime = true;
        assert_eq!(
            fstab_entry(&p, &c).to_string(),
            "UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/data btrfs subvol=/@data,compress=zstd:1,noatime,nofail,x-systemd.device-timeout=10s 0 0"
        );
        c.subvol = Some("/my vol".into());
        assert!(fstab_entry(&p, &c).to_string().contains("subvol=/my\\040vol,"));
    }

    #[test]
    fn ids_keep_the_case_they_were_reported_in() {
        let p = part("vfat", "a40d-85e7");
        assert!(fstab_entry(&p, &choices(&p, Driver::Vfat)).to_string().starts_with("UUID=a40d-85e7 "));
        let p = part("ntfs", "0123456789abcdef");
        assert!(fstab_entry(&p, &choices(&p, Driver::Ntfs3)).to_string().starts_with("UUID=0123456789abcdef "));
    }

    #[test]
    fn partuuid_only_without_a_filesystem_uuid() {
        let mut p = part("ext4", "x");
        p.uuid = None;
        let e = fstab_entry(&p, &choices(&p, Driver::Ext4));
        assert_eq!(e.spec, "PARTUUID=1234abcd-01");
        assert_eq!(e.notes.len(), 1);
        p.partuuid = None;
        let e = fstab_entry(&p, &choices(&p, Driver::Ext4));
        assert_eq!(e.spec, "/dev/sdb1");
    }

    #[test]
    fn every_entry_has_six_fields_nofail_and_a_timeout() {
        for d in [Driver::Ntfs3, Driver::Ntfs3g, Driver::Exfat, Driver::Vfat, Driver::Ext4, Driver::Btrfs, Driver::Xfs] {
            for ext in [false, true] {
                let mut p = part("x", "ABCD-1234");
                p.external = ext;
                let line = fstab_entry(&p, &choices(&p, d)).to_string();
                let f: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(f.len(), 6, "{line}");
                assert!(f[3].split(',').any(|o| o == "nofail"), "{line}");
                assert!(f[3].contains("x-systemd.device-timeout="), "{line}");
                assert_eq!((f[4], matches!(f[5], "0" | "2")), ("0", true), "{line}");
                assert!(!f[3].split(',').any(|o| matches!(o, "defaults" | "force" | "exec" | "umask=000")), "{line}");
                assert_ne!(f[5], "1");
            }
        }
    }

    #[test]
    fn escaping_round_trips_through_a_getmntent_style_decoder() {
        for mp in ["/media/my data", "/media/a\tb", "/media/back\\slash", "/media/all \t\\ three", "/media/plain"] {
            let p = part("ext4", "8b1e2f3a-aaaa-bbbb-cccc-0123456789ab");
            let mut c = choices(&p, Driver::Ext4);
            c.mountpoint = mp.into();
            let line = fstab_entry(&p, &c).to_string();
            assert!(!line.contains('\t') && line.split(' ').count() == 6, "{line:?}");
            let l = &entries(&line)[0];
            assert_eq!(l.file, mp, "{line:?}");
            assert_eq!(l.vfstype, "ext4");
        }
        assert_eq!(escape_field("a b\tc\\d"), "a\\040b\\011c\\134d");
        assert_eq!(unescape_field("a\\040b\\011c\\134d"), "a b\tc\\d");
        assert_eq!(unescape_field("a\\0b\\99"), "a\\0b\\99");
    }

    #[test]
    fn manual_options_are_checked_against_the_driver() {
        assert_eq!(validate_manual(Driver::Ntfs3, "uid=1000,windows_names,noatime").unwrap(), ["uid=1000", "windows_names", "noatime"]);
        assert_eq!(validate_manual(Driver::Btrfs, "subvol=/my\\040vol,compress=zstd:3").unwrap(), ["subvol=/my vol", "compress=zstd:3"]);
        for bad in ["force", "exec", "uid=1000,force", "defaults", "umask=00x", "bogus", "uid=", "uid=1,x-systemd.automount", "ro", "nofail"] {
            assert!(validate_manual(Driver::Ntfs3, bad).is_err(), "{bad}");
        }
        // injection through whitespace, commas, line breaks
        for bad in ["uid=1000 0 0", "uid=1000\t/etc", "uid=1000\n/dev/sda1 / ext4 defaults 0 0", "uid=1000\\012x"] {
            assert!(validate_manual(Driver::Ntfs3, bad).is_err(), "{bad:?}");
        }
        // comma-separated pieces are separate options and each must be valid
        assert!(validate_manual(Driver::Ntfs3, "uid=1000,/etc").is_err());
        // an option of another driver
        assert!(validate_manual(Driver::Ext4, "uid=1000").is_err());
        assert!(validate_manual(Driver::Vfat, "shortname=bogus").is_err());
        assert!(validate_manual(Driver::Btrfs, "compress=zstd:99").is_err());
        assert!(validate_manual(Driver::Ntfs3, "").is_err());
    }

    #[test]
    fn manual_options_replace_the_generated_ones_but_nofail_stays() {
        let p = part("ntfs", "0123456789ABCDEF");
        let mut c = choices(&p, Driver::Ntfs3);
        c.manual = Some(validate_manual(Driver::Ntfs3, "uid=1000,gid=1001,umask=027").unwrap());
        assert_eq!(
            fstab_entry(&p, &c).to_string(),
            "UUID=0123456789ABCDEF /media/data ntfs3 uid=1000,gid=1001,umask=027,nofail,x-systemd.device-timeout=10s 0 0"
        );
    }

    #[test]
    fn mount_options_for_the_test_mount_drop_the_fstab_only_ones() {
        let mut p = part("exfat", "A40D-85E7");
        p.external = true;
        let e = fstab_entry(&p, &choices(&p, Driver::Exfat));
        assert_eq!(e.mount_options(), ["uid=1000", "gid=1001", "dmask=0077", "fmask=0177"]);
    }

    const FSTAB: &str = "# /etc/fstab\nUUID=D0A3-F918 /boot vfat defaults,noatime 0 2\nUUID=2720fb5d-cbdd /  btrfs subvol=/@,defaults 0 0\n\n# swap\n";

    fn entry_for(p: &Partition) -> (FstabEntry, String) {
        let e = fstab_entry(p, &choices(p, Driver::Ntfs3));
        (e, "# kast: data partition (Data), added 2026-10-18".into())
    }

    #[test]
    fn appending_keeps_every_other_byte() {
        let p = part("ntfs", "0123456789ABCDEF");
        let (e, c) = entry_for(&p);
        let out = add_entry(FSTAB, &p, &e, &c, &[]).unwrap();
        assert!(out.text.starts_with(FSTAB));
        assert_eq!(&out.text[FSTAB.len()..], format!("{c}\n{e}\n"));
        assert!(out.replaced.is_empty());
        // no trailing newline
        let no_nl = FSTAB.trim_end();
        let out = add_entry(no_nl, &p, &e, &c, &[]).unwrap();
        assert_eq!(out.text, format!("{no_nl}\n{c}\n{e}\n"));
        // CRLF files get CRLF lines
        let crlf = "UUID=D0A3-F918 /boot vfat defaults 0 2\r\n";
        let out = add_entry(crlf, &p, &e, &c, &[]).unwrap();
        assert_eq!(out.text, format!("{crlf}{c}\r\n{e}\r\n"));
        // empty file
        assert_eq!(add_entry("", &p, &e, &c, &[]).unwrap().text, format!("{c}\n{e}\n"));
    }

    #[test]
    fn a_conflicting_entry_must_be_replaced_explicitly() {
        let p = part("ntfs", "0123456789ABCDEF");
        let (e, c) = entry_for(&p);
        let old = "UUID=0123456789ABCDEF /media/old ntfs3 defaults 0 0";
        let other = "UUID=ffff-0000 /media/data ext4 defaults 0 2";
        let text = format!("{FSTAB}{old}\n{other}\n");
        // same device at another folder, and another device at our folder
        let found = conflicts(&text, &p, "/media/data");
        assert_eq!(found.iter().map(|c| (c.same_device, c.same_mountpoint)).collect::<Vec<_>>(), [(true, false), (false, true)]);
        assert!(add_entry(&text, &p, &e, &c, &[]).is_err());
        let first = found[0].line.no;
        assert!(add_entry(&text, &p, &e, &c, &[first]).is_err(), "the second conflict is still there");
        let out = add_entry(&text, &p, &e, &c, &[found[0].line.no, found[1].line.no]).unwrap();
        assert_eq!(out.replaced, [old, other]);
        assert!(!out.text.contains(old) && !out.text.contains(other));
        assert!(out.text.starts_with(FSTAB));
        assert_eq!(out.text.matches("/media/data").count(), 1);
        // trailing slashes and the other ways to name the device are conflicts too
        assert_eq!(conflicts("UUID=0123456789abcdef /x/ ntfs3 defaults 0 0\n/dev/sdb1 /media/data/ ext4 defaults 0 2\n", &p, "/media/data").len(), 2);
        assert_eq!(
            conflicts(
                "/dev/disk/by-uuid/0123456789ABCDEF /x ntfs3 defaults 0 0\nLABEL=Data /y ntfs3 defaults 0 0\nPARTUUID=1234ABCD-01 /z ntfs3 defaults 0 0\n",
                &p,
                "/media/data"
            )
            .len(),
            3
        );
    }

    #[test]
    fn an_old_kast_comment_goes_with_its_replaced_line() {
        let p = part("ntfs", "0123456789ABCDEF");
        let (e, c) = entry_for(&p);
        let text =
            "UUID=D0A3-F918 /boot vfat defaults 0 2\n# kast: data partition (Data), added 2026-01-01\nUUID=0123456789ABCDEF /media/data ntfs3 defaults 0 0\n";
        let out = add_entry(text, &p, &e, &c, &[3]).unwrap();
        assert_eq!(out.text, format!("UUID=D0A3-F918 /boot vfat defaults 0 2\n{c}\n{e}\n"));
    }

    #[test]
    fn removing_only_kasts_lines_from_an_edited_file() {
        let p = part("ntfs", "0123456789ABCDEF");
        let (e, c) = entry_for(&p);
        let text = format!("# mine\n{FSTAB}{c}\n{e}\n# added later\nUUID=zzzz /mnt/x ext4 defaults 0 2\n");
        let out = remove_entry(&text, &e.to_string(), &c, &[]);
        assert_eq!(out, format!("# mine\n{FSTAB}# added later\nUUID=zzzz /mnt/x ext4 defaults 0 2\n"));
        let back = remove_entry(&text, &e.to_string(), &c, &["UUID=0123456789ABCDEF /media/old ntfs3 defaults 0 0".into()]);
        assert!(back.ends_with("UUID=zzzz /mnt/x ext4 defaults 0 2\nUUID=0123456789ABCDEF /media/old ntfs3 defaults 0 0\n"));
        // the user already deleted it
        assert_eq!(remove_entry(FSTAB, &e.to_string(), &c, &[]), FSTAB);
    }

    #[test]
    fn system_entries_are_found() {
        let mut p = part("btrfs", "2720fb5d-cbdd");
        p.partuuid = None;
        assert_eq!(system_entry(FSTAB, &p).unwrap().file, "/");
        assert!(system_entry(FSTAB, &part("ntfs", "0123456789ABCDEF")).is_none());
    }

    #[test]
    fn mount_point_rules() {
        assert_eq!(normalise_mountpoint("/media/data/").unwrap(), "/media/data");
        assert_eq!(normalise_mountpoint(" //media///data ").unwrap(), "/media/data");
        assert_eq!(normalise_mountpoint("/media/my data").unwrap(), "/media/my data");
        for bad in [
            "",
            "media/data",
            "relative",
            "/",
            "/boot",
            "/efi",
            "/home",
            "/root",
            "/usr",
            "/var",
            "/etc",
            "/opt",
            "/srv",
            "/tmp",
            "/run",
            "/proc",
            "/sys",
            "/dev",
            "/mnt",
            "/mnt/",
            "/proc/x",
            "/sys/x",
            "/dev/shm",
            "/run/media/u/x",
            "/media/../etc",
            "/media/./data",
            "/boot/data",
            "/usr/local/data",
            "/var/lib/x",
            "/etc/x",
            "/media/a\nb",
        ] {
            assert!(normalise_mountpoint(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(normalise_mountpoint("/mnt/data").is_ok());
        assert!(normalise_mountpoint("/home/k/data").is_ok());
    }

    struct Fake(Out);
    impl Runner for Fake {
        fn run(&self, _: &str, _: &[&str]) -> Out {
            self.0.clone()
        }
        fn run_as_user(&self, _: &[&str]) -> Out {
            self.0.clone()
        }
    }

    #[test]
    fn mount_point_state_checks() {
        let t = tempfile::tempdir().unwrap();
        let none = Fake(Out { ok: false, code: Some(1), ..Default::default() });
        let missing = t.path().join("missing");
        assert_eq!(check_mountpoint("/media/data", &missing, &none).unwrap().1, Spot::Missing);
        let empty = t.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(check_mountpoint("/media/data", &empty, &none).unwrap().1, Spot::Empty);
        std::fs::write(empty.join("f"), "x").unwrap();
        assert!(check_mountpoint("/media/data", &empty, &none).unwrap_err().contains("not empty"));
        let file = t.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(check_mountpoint("/media/data", &file, &none).unwrap_err().contains("not a folder"));
        let mounted = Fake(Out::success("/dev/sdb1\n"));
        assert!(check_mountpoint("/media/data", &missing, &mounted).unwrap_err().contains("already a mount point"));
        assert!(check_mountpoint("/etc", &missing, &none).is_err());
    }

    const VERIFY_OK: &str = "/boot\n   [ ] target exists\n   [ ] FS type is vfat\n/\n   [ ] target exists\nSuccess, no errors or warnings detected\n";
    const VERIFY_OLD: &str = "/boot\n   [ ] target exists\n/old\n   [W] unreachable source: /dev/gone: No such file or directory\n   [E] unreachable on boot required source: UUID=dead\n\n0 parse errors, 1 error, 1 warning\n";

    fn out(ok: bool, s: &str) -> Out {
        Out { ok, code: Some(i32::from(!ok)), stdout: s.into(), stderr: String::new() }
    }

    #[test]
    fn verify_output_is_parsed_per_target() {
        let (i, p) = parse_verify(VERIFY_OLD);
        assert_eq!(p, 0);
        assert_eq!(i.len(), 2);
        assert_eq!(i[0], Issue { target: "/old".into(), error: false, msg: "unreachable source: /dev/gone: No such file or directory".into() });
        assert!(i[1].error);
        assert!(parse_verify(VERIFY_OK).0.is_empty());
        assert_eq!(parse_verify("findmnt: x: parse error at line 4 -- ignored\n\n1 parse error, 0 errors, 0 warnings\n").1, 1);
    }

    #[test]
    fn old_problems_in_other_lines_do_not_block() {
        let before = out(false, VERIFY_OLD);
        let after = out(false, &format!("{VERIFY_OLD}/media/data\n   [ ] target exists\n   [ ] FS type is ntfs3\n"));
        assert_eq!(verify_diff(&before, &after, "/media/data"), VerifyResult::default());
    }

    #[test]
    fn new_errors_and_errors_on_our_line_block() {
        let before = out(false, VERIFY_OLD);
        let after = out(false, &format!("{VERIFY_OLD}/media/data\n   [E] unreachable on boot required source: UUID=0123\n"));
        let r = verify_diff(&before, &after, "/media/data");
        assert_eq!(r.errors, ["/media/data: unreachable on boot required source: UUID=0123"]);
        // a new parse error
        let after = out(false, &format!("{VERIFY_OLD}findmnt: f: parse error at line 9 -- ignored\n"));
        assert_eq!(verify_diff(&before, &after, "/media/data").errors.len(), 1);
        // a duplicate target is new and blocks even as a warning
        let after = out(true, "/media/data\n   [W] target specified more than once\n");
        assert_eq!(verify_diff(&out(true, VERIFY_OK), &after, "/media/data").errors.len(), 1);
        // a new warning on another line is shown, not blocking
        let after = out(true, "/other\n   [W] something odd\n");
        let r = verify_diff(&out(true, VERIFY_OK), &after, "/media/data");
        assert_eq!((r.errors.len(), r.warnings.len()), (0, 1));
        // a clean candidate, but the tool itself failed
        let r = verify_diff(&out(true, VERIFY_OK), &Out { ok: false, code: Some(1), stderr: "boom".into(), ..Default::default() }, "/media/data");
        assert_eq!(r.errors.len(), 1);
    }
}
