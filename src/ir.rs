//! IR emitters of Windows Hello cameras: the face-authentication control of Microsoft's UVC
//! extension unit (MSXU). Windows sets it on the IR stream before streaming and the firmware
//! then strobes the emitter; nothing on Linux sets it, so Howdy gets dark frames. `kast
//! ir-emitter enable` sets it right before each authentication (from PAM).
//! <https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/uvc-extensions-1-5>
//!
//! PAM runs `enable` in every sudo, login and unlock, as root or as the caller: nothing here
//! uses `crate::ctx` (no log file, no journal, no terminal), and nothing re-runs through sudo.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use serde_json::{Value, json};

/// 0f3f95dc-2632-4c4e-92c9-a04782f43bc8, as descriptors store it (first three fields little-endian).
pub const MSXU_GUID: [u8; 16] = [0xdc, 0x95, 0x3f, 0x0f, 0x32, 0x26, 0x4e, 0x4c, 0x92, 0xc9, 0xa0, 0x47, 0x82, 0xf4, 0x3b, 0xc8];
/// MSXU_CONTROL_FACE_AUTHENTICATION
pub const SEL_FACE_AUTH: u8 = 6;
const SEL_IR_TORCH: u8 = 10;

// UVC requests (linux/usb/video.h)
const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_RES: u8 = 0x84;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const GET_DEF: u8 = 0x87;

/// bmControlFlags of a face-authentication entry
const DISABLED: u64 = 1 << 0;
const ALTERNATE: u64 = 1 << 1;
const SUBTRACT: u64 = 1 << 2;
/// bNumEntries + one entry (bInterfaceNumber, 7 bytes of flags)
const ENTRY_LEN: usize = 9;

/// Exit statuses of `enable`, `restore` and `probe` (PAM only looks at zero or not).
pub const EXIT_NO_CAMERA: u8 = 3;
pub const EXIT_REFUSED: u8 = 4;
pub const EXIT_FAILED: u8 = 5;

fn req_name(req: u8) -> &'static str {
    match req {
        SET_CUR => "SET_CUR",
        GET_CUR => "GET_CUR",
        GET_MIN => "GET_MIN",
        GET_MAX => "GET_MAX",
        GET_RES => "GET_RES",
        GET_LEN => "GET_LEN",
        GET_INFO => "GET_INFO",
        GET_DEF => "GET_DEF",
        _ => "request",
    }
}

pub fn selector_name(s: u8) -> &'static str {
    match s {
        1 => "focus",
        2 => "exposure",
        3 => "ev-compensation",
        4 => "white-balance",
        6 => "face-authentication",
        7 => "camera-extrinsics",
        8 => "camera-intrinsics",
        9 => "metadata",
        10 => "ir-torch",
        11 => "digital-window",
        12 => "digital-window-config",
        13 => "video-hdr",
        14 => "framerate-throttle",
        15 => "field-of-view-2-config",
        16 => "field-of-view-2",
        _ => "?",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum IrError {
    /// No camera with the control (or not the node asked for)
    NoCamera(String),
    /// The camera has it, but the safety rules say no
    Refused(String),
    /// Opening the node or a request failed
    Failed(String),
}

impl IrError {
    pub fn exit(&self) -> u8 {
        match self {
            IrError::NoCamera(_) => EXIT_NO_CAMERA,
            IrError::Refused(_) => EXIT_REFUSED,
            IrError::Failed(_) => EXIT_FAILED,
        }
    }
}

impl fmt::Display for IrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (IrError::NoCamera(m) | IrError::Refused(m) | IrError::Failed(m)) = self;
        f.write_str(m)
    }
}

fn errno_text(e: Errno) -> String {
    let hint = match e {
        Errno::ENOENT => "uvcvideo has no such unit or selector on this node",
        Errno::ENOBUFS => "the size doesn't match the control's length",
        Errno::EBADRQC => "the control doesn't support this request",
        Errno::EPIPE => "the camera rejected the request (USB stall)",
        Errno::EACCES => "permission denied: log in locally or join the video group",
        Errno::EPERM => "operation not permitted: try again with sudo",
        Errno::EBUSY => "the device is busy",
        Errno::EIO => "I/O error talking to the camera",
        Errno::ETIMEDOUT => "the camera didn't answer in time",
        _ => return e.desc().to_string(),
    };
    format!("{} ({hint})", e.desc())
}

// --- Descriptors -------------------------------------------------------------------------

/// One VC_EXTENSION_UNIT.
#[derive(Debug, Clone, PartialEq)]
pub struct Xu {
    pub unit: u8,
    pub guid: [u8; 16],
    pub selectors: Vec<u8>,
}

impl Xu {
    pub fn is_msxu(&self) -> bool {
        self.guid == MSXU_GUID
    }
}

/// What a UVC function's VideoControl interface describes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Function {
    pub xus: Vec<Xu>,
    /// The VideoStreaming interfaces it owns (VC header's baInterfaceNr)
    pub streaming: Vec<u8>,
}

impl Function {
    pub fn msxu(&self) -> Option<&Xu> {
        self.xus.iter().find(|x| x.is_msxu())
    }

    /// The MSXU's unit when it has the face-authentication control.
    pub fn face_auth_unit(&self) -> Option<u8> {
        self.msxu().filter(|x| x.selectors.contains(&SEL_FACE_AUTH)).map(|x| x.unit)
    }
}

/// The extension units and streaming interfaces of VideoControl interface `vc` in raw USB
/// descriptors (sysfs `descriptors`).
pub fn parse_function(desc: &[u8], vc: u8) -> Function {
    let mut f = Function::default();
    let mut cur: Option<(u8, u8, u8)> = None;
    let mut i = 0;
    while i + 2 <= desc.len() {
        let (len, ty) = (desc[i] as usize, desc[i + 1]);
        if len < 2 || i + len > desc.len() {
            break;
        }
        let d = &desc[i..i + len];
        if ty == 0x04 && len >= 9 {
            // INTERFACE: number, class, subclass
            cur = Some((d[2], d[5], d[6]));
        } else if ty == 0x24 && len >= 3 && cur == Some((vc, 0x0e, 0x01)) {
            match d[2] {
                // VC_HEADER
                0x01 => {
                    if let Some(&n) = d.get(11) {
                        f.streaming = d.get(12..12 + n as usize).map(<[u8]>::to_vec).unwrap_or_default();
                    }
                }
                // VC_EXTENSION_UNIT
                0x06 => f.xus.extend(parse_xu(d)),
                _ => {}
            }
        }
        i += len;
    }
    f
}

fn parse_xu(d: &[u8]) -> Option<Xu> {
    let unit = *d.get(3)?;
    let guid: [u8; 16] = d.get(4..20)?.try_into().ok()?;
    let nr_in = *d.get(21)? as usize;
    let size = *d.get(22 + nr_in)? as usize;
    let bm = d.get(23 + nr_in..23 + nr_in + size)?;
    let selectors = (0..(size * 8).min(255)).filter(|b| bm[b / 8] >> (b % 8) & 1 == 1).map(|b| b as u8 + 1).collect();
    Some(Xu { unit, guid, selectors })
}

pub fn guid_string(g: &[u8; 16]) -> String {
    let h = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(&[g[3], g[2], g[1], g[0]]), h(&[g[5], g[4]]), h(&[g[7], g[6]]), h(&g[8..10]), h(&g[10..]))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

// --- Discovery (sysfs, unprivileged) -----------------------------------------------------

/// A video4linux capture node of a USB camera.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// /dev/videoN
    pub node: PathBuf,
    /// Its /dev/v4l/by-path name, when udev made one
    pub by_path: Option<PathBuf>,
    pub name: String,
    pub index: u32,
    pub vc_interface: u8,
    pub vid: String,
    pub pid: String,
    /// The USB device's sysfs name (3-1)
    pub usb_path: String,
    pub function: Function,
}

impl Node {
    /// The name to use: by-path when there is one.
    pub fn device(&self) -> &Path {
        self.by_path.as_deref().unwrap_or(&self.node)
    }

    pub fn qualifies(&self) -> bool {
        self.function.face_auth_unit().is_some()
    }
}

fn read_trim(p: &Path) -> Result<String, String> {
    fs::read_to_string(p).map(|s| s.trim().to_string()).map_err(|e| format!("{}: {e}", p.display()))
}

/// The first `/dev/v4l/by-path` link (sorted, so it's the same every run) to `/dev/<name>`.
fn by_path_at(root: &Path, name: &str) -> Option<PathBuf> {
    let target = fs::canonicalize(root.join("dev").join(name)).ok()?;
    let mut links: Vec<_> = fs::read_dir(root.join("dev/v4l/by-path")).ok()?.flatten().map(|e| e.file_name()).collect();
    links.sort();
    links.into_iter().find(|l| fs::canonicalize(root.join("dev/v4l/by-path").join(l)).is_ok_and(|t| t == target)).map(|l| Path::new("/dev/v4l/by-path").join(l))
}

/// The stable name of a video node: its by-path link, else the node itself.
pub fn stable_name(dev: &Path) -> PathBuf {
    fs::canonicalize(dev)
        .ok()
        .and_then(|real| real.file_name().map(|n| n.to_string_lossy().into_owned()))
        .and_then(|name| by_path_at(Path::new("/"), &name))
        .unwrap_or_else(|| dev.to_path_buf())
}

/// `/sys/class/video4linux/<name>` under `root`.
fn node_at(root: &Path, name: &str) -> Result<Node, String> {
    let base = root.join("sys/class/video4linux").join(name);
    if !base.is_dir() {
        return Err(format!("/dev/{name} is not a video4linux device"));
    }
    let not_usb = |e: String| format!("/dev/{name} is not a USB video device ({e})");
    let intf = fs::canonicalize(base.join("device")).map_err(|e| not_usb(e.to_string()))?;
    let usb = intf.parent().ok_or_else(|| not_usb("no parent device".into()))?;
    let vc = u8::from_str_radix(&read_trim(&intf.join("bInterfaceNumber")).map_err(not_usb)?, 16).map_err(|e| not_usb(e.to_string()))?;
    let desc = fs::read(usb.join("descriptors")).map_err(|e| not_usb(e.to_string()))?;
    Ok(Node {
        node: Path::new("/dev").join(name),
        by_path: by_path_at(root, name),
        name: read_trim(&base.join("name")).unwrap_or_default(),
        index: read_trim(&base.join("index")).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        vc_interface: vc,
        vid: read_trim(&usb.join("idVendor")).map_err(not_usb)?,
        pid: read_trim(&usb.join("idProduct")).map_err(not_usb)?,
        usb_path: usb.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        function: parse_function(&desc, vc),
    })
}

/// The node `dev` names (/dev/videoN or a symlink to it, such as a by-path name).
pub fn node_info(dev: &Path) -> Result<Node, String> {
    let real = fs::canonicalize(dev).map_err(|e| format!("{}: {e}", dev.display()))?;
    let name = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.starts_with("video") {
        return Err(format!("{} is not a /dev/video* node", dev.display()));
    }
    node_at(Path::new("/"), &name)
}

/// Every USB capture node (index 0), sorted.
fn scan_at(root: &Path) -> Vec<Node> {
    let mut names: Vec<String> = fs::read_dir(root.join("sys/class/video4linux"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("video")).collect())
        .unwrap_or_default();
    names.sort_by_key(|n| (n.len(), n.clone()));
    names.iter().filter_map(|n| node_at(root, n).ok()).filter(|n| n.index == 0).collect()
}

pub fn scan() -> Vec<Node> {
    scan_at(Path::new("/"))
}

/// Whether any camera has the control (sysfs only, no device I/O).
pub fn any_candidate() -> bool {
    scan().iter().any(Node::qualifies)
}

/// The node to use: `device`, or the one camera with the control.
pub fn detect(device: Option<&Path>) -> Result<Node, IrError> {
    let node = match device {
        Some(dev) => node_info(dev).map_err(IrError::NoCamera)?,
        None => {
            let mut found: Vec<Node> = scan().into_iter().filter(Node::qualifies).collect();
            match found.len() {
                0 => return Err(IrError::NoCamera("no camera with an MSXU face-authentication control".into())),
                1 => found.remove(0),
                _ => {
                    let names: Vec<String> = found.iter().map(|n| n.device().display().to_string()).collect();
                    return Err(IrError::Refused(format!("several cameras qualify ({}); pick one with --device", names.join(", "))));
                }
            }
        }
    };
    if !node.qualifies() {
        return Err(IrError::NoCamera(format!("{} (USB {}:{}) has no MSXU face-authentication control", node.device().display(), node.vid, node.pid)));
    }
    Ok(node)
}

// --- UVC queries -------------------------------------------------------------------------

/// struct uvc_xu_control_query (linux/uvcvideo.h)
#[repr(C)]
pub struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

nix::ioctl_readwrite!(uvc_ctrl_query, b'u', 0x21, UvcXuControlQuery);

/// A camera's extension unit controls (a fake one in tests).
pub trait Camera {
    fn query(&mut self, unit: u8, selector: u8, req: u8, data: &mut [u8]) -> Result<(), Errno>;
}

/// A /dev/video node, through uvcvideo's UVCIOC_CTRL_QUERY.
pub struct Uvc(fs::File);

impl Uvc {
    pub fn open(p: &Path) -> Result<Uvc, IrError> {
        fs::OpenOptions::new().read(true).write(true).open(p).map(Uvc).map_err(|e| {
            let text = e.raw_os_error().map_or_else(|| e.to_string(), |n| errno_text(Errno::from_raw(n)));
            IrError::Failed(format!("can't open {}: {text}", p.display()))
        })
    }
}

impl Camera for Uvc {
    fn query(&mut self, unit: u8, selector: u8, req: u8, data: &mut [u8]) -> Result<(), Errno> {
        let size = u16::try_from(data.len()).map_err(|_| Errno::EINVAL)?;
        let mut q = UvcXuControlQuery { unit, selector, query: req, size, data: data.as_mut_ptr() };
        // SAFETY: `q` points at `data`, which is `size` bytes long and outlives the call
        unsafe { uvc_ctrl_query(self.0.as_raw_fd(), &mut q) }.map(|_| ())
    }
}

fn get(cam: &mut impl Camera, unit: u8, selector: u8, req: u8, len: usize) -> Result<Vec<u8>, IrError> {
    let mut buf = vec![0u8; len];
    cam.query(unit, selector, req, &mut buf).map_err(|e| IrError::Failed(format!("{}: {}", req_name(req), errno_text(e))))?;
    Ok(buf)
}

fn info_len(cam: &mut impl Camera, unit: u8) -> Result<(u8, usize), IrError> {
    let info = get(cam, unit, SEL_FACE_AUTH, GET_INFO, 1)?[0];
    let len = get(cam, unit, SEL_FACE_AUTH, GET_LEN, 2)?;
    Ok((info, u16::from_le_bytes([len[0], len[1]]) as usize))
}

fn set(cam: &mut impl Camera, unit: u8, payload: &[u8]) -> Result<(), IrError> {
    let mut buf = payload.to_vec();
    cam.query(unit, SEL_FACE_AUTH, SET_CUR, &mut buf).map_err(|e| IrError::Failed(format!("SET_CUR: {}", errno_text(e))))
}

// --- Face-authentication values ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Alternate frame illumination
    Alternate,
    /// Background subtraction
    Subtract,
}

impl Mode {
    pub fn bit(self) -> u64 {
        match self {
            Mode::Alternate => ALTERNATE,
            Mode::Subtract => SUBTRACT,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Alternate => "alternate",
            Mode::Subtract => "subtract",
        }
    }
}

/// (bInterfaceNumber, bmControlFlags) entries.
pub fn entries(data: &[u8]) -> Vec<(u8, u64)> {
    let n = data.first().copied().unwrap_or(0) as usize;
    (0..n)
        .map_while(|k| {
            let e = data.get(1 + 8 * k..9 + 8 * k)?;
            let mut flags = [0u8; 8];
            flags[..7].copy_from_slice(&e[1..8]);
            Some((e[0], u64::from_le_bytes(flags)))
        })
        .collect()
}

pub fn flag_names(flags: u64) -> String {
    let mut names: Vec<String> =
        [(DISABLED, "disabled"), (ALTERNATE, "alternate"), (SUBTRACT, "subtract")].iter().filter(|(b, _)| flags & b != 0).map(|(_, n)| n.to_string()).collect();
    if flags & !0x7 != 0 {
        names.push(format!("reserved:0x{:x}", flags & !0x7));
    }
    if names.is_empty() { "none".into() } else { names.join("+") }
}

pub fn describe(data: &[u8]) -> String {
    let e = entries(data);
    if e.is_empty() {
        return "no interfaces listed".into();
    }
    e.iter().map(|(i, f)| format!("interface {i}: {}", flag_names(*f))).collect::<Vec<_>>().join("; ")
}

/// One entry, `len` bytes long (zero-padded). None when `len` can't hold it.
pub fn payload(interface: u8, flag: u64, len: usize) -> Option<Vec<u8>> {
    if len < ENTRY_LEN {
        return None;
    }
    let mut p = vec![0u8; len];
    p[0] = 1;
    p[1] = interface;
    p[2..9].copy_from_slice(&flag.to_le_bytes()[..7]);
    Some(p)
}

pub fn info_bits(v: u8) -> Vec<&'static str> {
    let mut bits = vec![if v & 1 != 0 { "GET" } else { "no-GET" }, if v & 2 != 0 { "SET" } else { "no-SET" }];
    for (bit, name) in [(4, "disabled-by-auto"), (8, "autoupdate"), (16, "async"), (32, "disabled-incompatible")] {
        if v & bit != 0 {
            bits.push(name);
        }
    }
    bits
}

/// What `enable` does with a camera.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Set { interface: u8, mode: Mode, payload: Vec<u8> },
    Already { interface: u8, mode: Mode },
    Refuse(String),
}

/// Decide from GET_INFO, GET_LEN, GET_MAX and GET_CUR. `streaming`: the node's streaming
/// interfaces (empty when the descriptors don't say).
pub fn plan(info: u8, len: usize, max: &[u8], cur: &[u8], streaming: &[u8]) -> Plan {
    if info & 2 == 0 {
        return Plan::Refuse("the camera reports the face-authentication control as read-only".into());
    }
    if len < ENTRY_LEN {
        return Plan::Refuse(format!("the control is {len} bytes, too short for one entry; not guessing its layout"));
    }
    let listed = entries(max);
    if listed.is_empty() {
        return Plan::Refuse("GET_MAX lists no interface that supports face authentication".into());
    }
    let ours: Vec<(u8, u64)> = listed.iter().copied().filter(|(i, _)| streaming.is_empty() || streaming.contains(i)).collect();
    let ifaces = |v: &[(u8, u64)]| v.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>().join(", ");
    let (interface, supported) = match ours.as_slice() {
        [one] => *one,
        [] => {
            return Plan::Refuse(format!(
                "GET_MAX lists interface(s) {}, none of them this node's stream ({})",
                ifaces(&listed),
                streaming.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")
            ));
        }
        many => return Plan::Refuse(format!("GET_MAX lists several of this node's interfaces ({}); not guessing which one", ifaces(many))),
    };
    let mode = if supported & ALTERNATE != 0 {
        Mode::Alternate
    } else if supported & SUBTRACT != 0 {
        Mode::Subtract
    } else {
        return Plan::Refuse(format!("interface {interface} advertises no illumination mode ({})", flag_names(supported)));
    };
    if entries(cur).iter().any(|&(i, f)| i == interface && f & 0x7 == mode.bit()) {
        return Plan::Already { interface, mode };
    }
    match payload(interface, mode.bit(), len) {
        Some(payload) => Plan::Set { interface, mode, payload },
        None => Plan::Refuse("the control is too short for one entry".into()),
    }
}

/// GET requests only.
pub fn read_plan(cam: &mut impl Camera, unit: u8, streaming: &[u8]) -> Result<Plan, IrError> {
    let (info, len) = info_len(cam, unit)?;
    if info & 2 == 0 {
        return Ok(plan(info, len, &[], &[], streaming));
    }
    let max = get(cam, unit, SEL_FACE_AUTH, GET_MAX, len)?;
    let cur = get(cam, unit, SEL_FACE_AUTH, GET_CUR, len)?;
    Ok(plan(info, len, &max, &cur, streaming))
}

/// Set the mode unless it's set already. Ok: the plan that was carried out (Set or Already).
pub fn enable_on(cam: &mut impl Camera, unit: u8, streaming: &[u8]) -> Result<Plan, IrError> {
    let p = read_plan(cam, unit, streaming)?;
    match &p {
        Plan::Refuse(why) => return Err(IrError::Refused(why.clone())),
        Plan::Set { payload, .. } => set(cam, unit, payload)?,
        Plan::Already { .. } => {}
    }
    Ok(p)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Restored {
    Written(Vec<u8>),
    AlreadyDefault(Vec<u8>),
}

/// Write the camera's own default (GET_DEF) back.
pub fn restore_on(cam: &mut impl Camera, unit: u8) -> Result<Restored, IrError> {
    let (info, len) = info_len(cam, unit)?;
    if info & 2 == 0 {
        return Err(IrError::Refused("the camera reports the face-authentication control as read-only".into()));
    }
    let def = get(cam, unit, SEL_FACE_AUTH, GET_DEF, len)?;
    if get(cam, unit, SEL_FACE_AUTH, GET_CUR, len)? == def {
        return Ok(Restored::AlreadyDefault(def));
    }
    set(cam, unit, &def)?;
    Ok(Restored::Written(def))
}

fn unit_of(node: &Node) -> Result<u8, IrError> {
    node.function.face_auth_unit().ok_or_else(|| IrError::NoCamera(format!("{} has no MSXU face-authentication control", node.device().display())))
}

/// Detect, open and enable.
pub fn enable(device: Option<&Path>) -> Result<(Node, Plan), IrError> {
    let node = detect(device)?;
    let mut cam = Uvc::open(&node.node)?;
    let p = enable_on(&mut cam, unit_of(&node)?, &node.function.streaming)?;
    Ok((node, p))
}

fn done_line(node: &Node, p: &Plan) -> String {
    match p {
        Plan::Set { interface, mode, .. } => format!("IR emitter on: {} on interface {interface} of {}", mode.name(), node.device().display()),
        Plan::Already { interface, mode } => format!("IR emitter already on: {} on interface {interface} of {}", mode.name(), node.device().display()),
        Plan::Refuse(why) => format!("IR emitter not switched on: {why}"),
    }
}

// --- Commands ----------------------------------------------------------------------------

/// `kast ir-emitter enable`: what PAM runs. One line of output, no prompts.
pub fn cmd_enable(device: Option<&Path>) -> u8 {
    match enable(device) {
        Ok((node, p)) => {
            println!("{}", done_line(&node, &p));
            0
        }
        Err(e) => {
            eprintln!("kast ir-emitter: {e}");
            e.exit()
        }
    }
}

pub fn cmd_restore(device: Option<&Path>) -> u8 {
    let r = detect(device).and_then(|node| {
        let mut cam = Uvc::open(&node.node)?;
        Ok((restore_on(&mut cam, unit_of(&node)?)?, node))
    });
    match r {
        Ok((Restored::Written(def), node)) => {
            println!("Wrote the default back to {}: {}", node.device().display(), describe(&def));
            0
        }
        Ok((Restored::AlreadyDefault(def), node)) => {
            println!("{} is already at its default: {}", node.device().display(), describe(&def));
            0
        }
        Err(e) => {
            eprintln!("kast ir-emitter: {e}");
            e.exit()
        }
    }
}

/// `kast ir-emitter run -- CMD`: switch the emitter on, then run CMD (also when it can't be
/// switched on) and exit with its status. Nothing is restored: the camera goes back to
/// disabled on its own once the stream stops.
pub fn cmd_run(device: Option<&Path>, command: &[OsString]) -> u8 {
    run_with(|| enable(device).map(|(node, p)| done_line(&node, &p)), command)
}

pub fn run_with(enable: impl FnOnce() -> Result<String, IrError>, command: &[OsString]) -> u8 {
    use nix::sys::signal::{SigHandler, Signal, signal};
    use std::os::unix::process::ExitStatusExt;
    let Some((prog, args)) = command.split_first() else {
        eprintln!("kast ir-emitter run: no command given");
        return 2;
    };
    match enable() {
        Ok(line) => eprintln!("{line}"),
        Err(e) => eprintln!("IR emitter not switched on ({e}); running {} anyway", prog.to_string_lossy()),
    }
    let mut child = match std::process::Command::new(prog).args(args).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kast ir-emitter run: {}: {e}", prog.to_string_lossy());
            return if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
        }
    };
    // Ctrl+C is for the command (it gets it too); wait for it like a shell does
    // SAFETY: plain dispositions, put back right after
    let old = [Signal::SIGINT, Signal::SIGQUIT].map(|s| (s, unsafe { signal(s, SigHandler::SigIgn) }));
    let status = child.wait();
    for (s, h) in old {
        if let Ok(h) = h {
            // SAFETY: as above
            unsafe {
                let _ = signal(s, h);
            }
        }
    }
    match status {
        Ok(st) => st.code().map_or_else(|| st.signal().map_or(1, |s| 128 + s as u8), |c| c as u8),
        Err(e) => {
            eprintln!("kast ir-emitter run: {e}");
            1
        }
    }
}

/// One request's answer in `probe`.
struct Answer {
    req: u8,
    data: Result<Vec<u8>, String>,
}

/// (request, data)
type Answered = (u8, Vec<u8>);

struct ControlProbe {
    info: u8,
    len: usize,
    answers: Vec<Answer>,
    plan: Plan,
    torch: Option<Result<Vec<Answered>, String>>,
}

fn probe_control(node: &Node) -> Result<ControlProbe, String> {
    let unit = unit_of(node).map_err(|e| e.to_string())?;
    let mut cam = Uvc::open(&node.node).map_err(|e| e.to_string())?;
    let (info, len) = info_len(&mut cam, unit).map_err(|e| e.to_string())?;
    let answers: Vec<Answer> = [GET_MIN, GET_MAX, GET_RES, GET_DEF, GET_CUR]
        .into_iter()
        .map(|req| Answer { req, data: get(&mut cam, unit, SEL_FACE_AUTH, req, len).map_err(|e| e.to_string()) })
        .collect();
    let value = |r: u8| answers.iter().find(|a| a.req == r).and_then(|a| a.data.as_ref().ok()).cloned().unwrap_or_default();
    let plan = plan(info, len, &value(GET_MAX), &value(GET_CUR), &node.function.streaming);
    let torch = node.function.msxu().filter(|x| x.selectors.contains(&SEL_IR_TORCH)).map(|_| {
        let tlen = get(&mut cam, unit, SEL_IR_TORCH, GET_LEN, 2).map_err(|e| e.to_string())?;
        let tlen = u16::from_le_bytes([tlen[0], tlen[1]]) as usize;
        [GET_MAX, GET_DEF, GET_CUR].into_iter().map(|r| get(&mut cam, unit, SEL_IR_TORCH, r, tlen).map(|d| (r, d)).map_err(|e| e.to_string())).collect()
    });
    Ok(ControlProbe { info, len, answers, plan, torch })
}

fn plan_text(p: &Plan) -> String {
    match p {
        Plan::Set { interface, mode, payload } => format!("set {} on interface {interface} (SET_CUR {})", mode.name(), hex(payload)),
        Plan::Already { interface, mode } => format!("nothing: {} is already set on interface {interface}", mode.name()),
        Plan::Refuse(why) => format!("nothing: {why}"),
    }
}

fn entries_json(data: &[u8]) -> Value {
    Value::Array(entries(data).iter().map(|(i, f)| json!({ "interface": i, "flags": format!("0x{f:x}"), "modes": flag_names(*f) })).collect())
}

fn node_json(n: &Node, c: Option<&Result<ControlProbe, String>>) -> Value {
    let mut v = json!({
        "node": n.node,
        "by_path": n.by_path,
        "name": n.name,
        "usb": format!("{}:{}", n.vid, n.pid),
        "usb_path": n.usb_path,
        "vc_interface": n.vc_interface,
        "streaming_interfaces": n.function.streaming,
        "extension_units": n.function.xus.iter().map(|x| json!({
            "unit": x.unit, "guid": guid_string(&x.guid), "selectors": x.selectors, "msxu": x.is_msxu(),
        })).collect::<Vec<_>>(),
        "qualifies": n.qualifies(),
    });
    if let Some(x) = n.function.msxu() {
        v["msxu"] = json!({ "unit": x.unit, "selectors": x.selectors.iter().map(|s| json!({ "selector": s, "name": selector_name(*s) })).collect::<Vec<_>>() });
    }
    match c {
        Some(Ok(c)) => {
            let mut fa = json!({ "info": format!("0x{:02x}", c.info), "info_bits": info_bits(c.info), "len": c.len });
            for a in &c.answers {
                let key = req_name(a.req).trim_start_matches("GET_").to_lowercase();
                fa[key] = match &a.data {
                    Ok(d) => json!({ "hex": hex(d), "entries": entries_json(d) }),
                    Err(e) => json!({ "error": e }),
                };
            }
            v["face_authentication"] = fa;
            v["enable"] = match &c.plan {
                Plan::Set { interface, mode, payload } => json!({ "action": "set", "interface": interface, "mode": mode.name(), "payload": hex(payload) }),
                Plan::Already { interface, mode } => json!({ "action": "none", "interface": interface, "mode": mode.name(), "reason": "already set" }),
                Plan::Refuse(why) => json!({ "action": "none", "reason": why }),
            };
        }
        Some(Err(e)) => v["error"] = json!(e),
        None => {}
    }
    v
}

fn print_node(n: &Node, c: Option<&Result<ControlProbe, String>>) {
    println!("Device:    {}", n.node.display());
    println!("By path:   {}", n.by_path.as_ref().map_or_else(|| "none".into(), |p| p.display().to_string()));
    println!("Camera:    {} (USB {}:{} at {}, VideoControl interface {})", n.name, n.vid, n.pid, n.usb_path, n.vc_interface);
    if !n.function.streaming.is_empty() {
        println!("Streams:   interface(s) {}", n.function.streaming.iter().map(u8::to_string).collect::<Vec<_>>().join(", "));
    }
    for x in &n.function.xus {
        println!("XU {:<6} {}  selectors {:?}{}", x.unit, guid_string(&x.guid), x.selectors, if x.is_msxu() { "  <- Microsoft MSXU" } else { "" });
    }
    match n.function.msxu() {
        Some(x) => println!("MSXU:      unit {}: {}", x.unit, x.selectors.iter().map(|s| format!("{s}={}", selector_name(*s))).collect::<Vec<_>>().join(", ")),
        None => println!("MSXU:      none"),
    }
    let Some(c) = c else {
        println!("\nNo face-authentication control: kast can't switch an IR emitter on here.");
        return;
    };
    let c = match c {
        Ok(c) => c,
        Err(e) => {
            println!("\nFace authentication: {e}");
            return;
        }
    };
    println!("\nFace authentication (selector {SEL_FACE_AUTH}), GET requests only");
    println!("  GET_INFO = 0x{:02x} ({})", c.info, info_bits(c.info).join(","));
    println!("  GET_LEN  = {}", c.len);
    for a in &c.answers {
        match &a.data {
            Ok(d) => println!("  {:<8} = {}  ({})", req_name(a.req), hex(d), describe(d)),
            Err(e) => println!("  {:<8} : {e}", req_name(a.req)),
        }
    }
    match &c.torch {
        None => println!("IR torch:  not implemented by this camera"),
        Some(Err(e)) => println!("IR torch:  {e}"),
        Some(Ok(v)) => {
            for (r, d) in v {
                println!("IR torch {:<8} = {}", req_name(*r), hex(d));
            }
        }
    }
    println!("\nenable would: {}", plan_text(&c.plan));
}

/// `kast ir-emitter probe`: GET requests only.
pub fn cmd_probe(device: Option<&Path>, as_json: bool) -> u8 {
    let nodes: Vec<Node> = match device {
        Some(d) => match node_info(d) {
            Ok(n) => vec![n],
            Err(e) => {
                eprintln!("kast ir-emitter: {e}");
                return EXIT_NO_CAMERA;
            }
        },
        None => scan(),
    };
    let shown: Vec<&Node> = if device.is_some() { nodes.iter().collect() } else { nodes.iter().filter(|n| n.qualifies()).collect() };
    let probes: Vec<Option<Result<ControlProbe, String>>> = shown.iter().map(|n| n.qualifies().then(|| probe_control(n))).collect();
    let rc = if shown.iter().any(|n| n.qualifies()) {
        if probes.iter().flatten().all(|p| p.as_ref().is_ok_and(|c| !matches!(c.plan, Plan::Refuse(_)))) { 0 } else { EXIT_REFUSED }
    } else {
        EXIT_NO_CAMERA
    };
    if as_json {
        let cams: Vec<Value> = shown.iter().zip(&probes).map(|(n, p)| node_json(n, p.as_ref())).collect();
        let checked: Vec<Value> = nodes.iter().map(|n| json!({ "node": n.node, "usb": format!("{}:{}", n.vid, n.pid), "qualifies": n.qualifies() })).collect();
        println!("{}", serde_json::to_string_pretty(&json!({ "cameras": cams, "checked": checked })).unwrap_or_default());
        return rc;
    }
    if shown.is_empty() {
        println!("No camera with an MSXU face-authentication control found.");
        for n in &nodes {
            println!(
                "  {} (USB {}:{}): {}",
                n.node.display(),
                n.vid,
                n.pid,
                if n.function.msxu().is_some() { "MSXU without face authentication" } else { "no MSXU" }
            );
        }
        return rc;
    }
    for (k, (n, p)) in shown.iter().zip(&probes).enumerate() {
        if k > 0 {
            println!();
        }
        print_node(n, p.as_ref());
    }
    rc
}

// --- Status and Howdy hints --------------------------------------------------------------

/// The IR emitter for `kast status` (GET requests only).
#[derive(Debug, Clone, PartialEq)]
pub enum IrState {
    /// kast can switch it on; `pam`: the kast line is in /etc/pam.d/sudo
    Supported {
        device: String,
        mode: Mode,
        pam: bool,
    },
    /// The camera has the control but kast won't set it, or reading it failed
    Unusable {
        device: String,
        why: String,
    },
    NotSupported {
        device: String,
    },
    NoCamera,
}

impl IrState {
    pub fn supported(&self) -> bool {
        matches!(self, IrState::Supported { .. })
    }
}

/// `howdy_device`: Howdy's device_path; `pam_sudo`: /etc/pam.d/sudo's text.
pub fn status(howdy_device: Option<&str>, pam_sudo: Option<&str>) -> IrState {
    let configured = howdy_device.and_then(|d| node_info(Path::new(d)).ok());
    let found: Vec<Node> = scan().into_iter().filter(Node::qualifies).collect();
    let node = match configured {
        Some(c) if c.qualifies() => c,
        c => match found.into_iter().next() {
            Some(n) => n,
            None => return c.map_or(IrState::NoCamera, |c| IrState::NotSupported { device: c.device().display().to_string() }),
        },
    };
    let device = node.device().display().to_string();
    let p = unit_of(&node).and_then(|unit| read_plan(&mut Uvc::open(&node.node)?, unit, &node.function.streaming));
    match p {
        Ok(Plan::Set { mode, .. } | Plan::Already { mode, .. }) => {
            IrState::Supported { device, mode, pam: pam_sudo.is_some_and(|t| t.lines().any(|l| emitter_line(l) == Some(EmitterLine::Kast))) }
        }
        Ok(Plan::Refuse(why)) => IrState::Unusable { device, why },
        Err(e) => IrState::Unusable { device, why: e.to_string() },
    }
}

/// What the Services step found for Howdy's camera.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// kast can switch the emitter on with this mode
    Usable(Mode),
    /// No MSXU face-authentication control
    NotSupported,
    /// It has one, but kast won't or can't set it (why)
    Unusable(String),
}

impl Verdict {
    pub fn usable(&self) -> bool {
        matches!(self, Verdict::Usable(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    /// The node's by-path name (else the node as given): Howdy's device_path and the PAM line's
    pub device: String,
    pub verdict: Verdict,
}

impl Check {
    pub fn verdict_text(&self) -> String {
        match &self.verdict {
            Verdict::Usable(m) => format!("MSXU face authentication, {} mode", m.name()),
            Verdict::NotSupported => "no MSXU face-authentication control".into(),
            Verdict::Unusable(why) => why.clone(),
        }
    }
}

/// The Services step's check of `dev` (GET requests only).
pub fn check(dev: &Path) -> Check {
    let device = stable_name(dev).display().to_string();
    let node = match node_info(dev) {
        Ok(n) => n,
        Err(e) if !dev.exists() => return Check { device, verdict: Verdict::Unusable(format!("no IR camera found ({e})")) },
        Err(_) => return Check { device, verdict: Verdict::NotSupported },
    };
    let Some(unit) = node.function.face_auth_unit() else { return Check { device, verdict: Verdict::NotSupported } };
    let verdict = match Uvc::open(&node.node).and_then(|mut cam| read_plan(&mut cam, unit, &node.function.streaming)) {
        Ok(Plan::Set { mode, .. } | Plan::Already { mode, .. }) => Verdict::Usable(mode),
        Ok(Plan::Refuse(why)) => Verdict::Unusable(why),
        Err(e) => Verdict::Unusable(e.to_string()),
    };
    Check { device, verdict }
}

/// How to add a face model: with the emitter on when the camera has one.
pub fn enroll_command(user: &str, emitter: bool) -> String {
    if emitter { format!("sudo kast ir-emitter run -- howdy -U {user} add") } else { format!("sudo howdy -U {user} add") }
}

/// For a camera with an emitter and a face model already there.
pub fn reenroll_note(user: &str) -> String {
    format!("Models recorded before the IR emitter worked were captured in the dark; add a fresh one: {}", enroll_command(user, true))
}

// --- PAM lines ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitterLine {
    /// `pam_exec.so … ir-emitter enable …` (kast's)
    Kast,
    /// `pam_exec.so … ir-msxu …` (written by hand)
    Msxu,
}

/// Whether a PAM line switches an IR emitter on (comments never do).
pub fn emitter_line(line: &str) -> Option<EmitterLine> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let tokens: Vec<&str> = t.split_whitespace().collect();
    if !tokens.iter().any(|w| *w == "pam_exec.so" || w.ends_with("/pam_exec.so")) {
        return None;
    }
    if tokens.windows(2).any(|w| w == ["ir-emitter", "enable"]) {
        Some(EmitterLine::Kast)
    } else if tokens.iter().any(|w| matches!(Path::new(w).file_name().and_then(|n| n.to_str()), Some("ir-msxu" | "ir-msxu.py"))) {
        Some(EmitterLine::Msxu)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const FIXTURE: &[u8] = include_bytes!("testdata/uvc/30c9_00c2.descriptors");

    #[test]
    fn ioctl_struct_and_request() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<UvcXuControlQuery>(), 16);
            assert_eq!(nix::request_code_readwrite!(b'u', 0x21, std::mem::size_of::<UvcXuControlQuery>()) as u32, 0xc010_7521);
        }
    }

    #[test]
    fn laptop_descriptors() {
        let ir = parse_function(FIXTURE, 2);
        let msxu = ir.msxu().unwrap();
        assert_eq!((msxu.unit, msxu.selectors.as_slice()), (7, &[2, 3, 6, 9][..]));
        assert_eq!(guid_string(&msxu.guid), "0f3f95dc-2632-4c4e-92c9-a04782f43bc8");
        assert_eq!(ir.face_auth_unit(), Some(7));
        assert_eq!(ir.streaming, [3]);
        let rgb = parse_function(FIXTURE, 0);
        assert_eq!(rgb.streaming, [1]);
        let rgb_msxu = rgb.msxu().unwrap();
        assert_eq!((rgb_msxu.unit, rgb_msxu.selectors.as_slice()), (5, &[2, 3, 9][..]));
        assert_eq!(rgb.face_auth_unit(), None, "the RGB function is not a candidate");
        assert!(parse_function(FIXTURE, 4).xus.is_empty(), "DFU");
    }

    fn xu_desc(unit: u8, guid: &str, bm: [u8; 4]) -> Vec<u8> {
        let g: Vec<u8> = guid.replace('-', "").as_bytes().chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap()).collect();
        let le = [g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6]];
        let mut d = vec![0x1d, 0x24, 0x06, unit];
        d.extend(le);
        d.extend(&g[8..]);
        d.extend([0x04, 0x01, 0x02, 0x04]);
        d.extend(bm);
        d.push(0);
        d
    }

    #[test]
    fn built_descriptors() {
        let iface = |n: u8, sub: u8| vec![0x09, 0x04, n, 0x00, 0x00, 0x0e, sub, 0x00, 0x00];
        let header = |s: u8| vec![0x0d, 0x24, 0x01, 0x50, 0x01, 0x8c, 0x00, 0x00, 0x6c, 0xdc, 0x02, 0x01, s];
        let ir_msxu = [
            0x1d, 0x24, 0x06, 0x07, 0xdc, 0x95, 0x3f, 0x0f, 0x32, 0x26, 0x4e, 0x4c, 0x92, 0xc9, 0xa0, 0x47, 0x82, 0xf4, 0x3b, 0xc8, 0x04, 0x01, 0x02, 0x04,
            0x26, 0x01, 0x00, 0x00, 0x00,
        ];
        let mut d = iface(0, 1);
        d.extend(header(1));
        d.extend(xu_desc(5, "0f3f95dc-2632-4c4e-92c9-a04782f43bc8", [0x06, 0x01, 0x00, 0x00]));
        d.extend(iface(1, 2));
        d.extend(iface(2, 1));
        d.extend(header(3));
        d.extend(xu_desc(4, "63610682-5070-49ab-b8cc-b3855e8d221d", [0xff, 0xff, 0x71, 0x07]));
        d.extend(xu_desc(5, "52182091-70d0-4ff5-acbc-0d04cc5f5a64", [0x0a, 0x00, 0x00, 0x00]));
        d.extend(ir_msxu);
        d.extend(iface(3, 2));
        let ir = parse_function(&d, 2);
        assert_eq!(ir.xus.iter().map(|x| x.unit).collect::<Vec<_>>(), [4, 5, 7]);
        assert_eq!(ir.xus[1].selectors, [2, 4]);
        assert_eq!(guid_string(&ir.xus[0].guid), "63610682-5070-49ab-b8cc-b3855e8d221d");
        assert_eq!(ir.face_auth_unit(), Some(7));
        assert_eq!(ir.msxu().unwrap().selectors, [2, 3, 6, 9]);
        assert_eq!(parse_function(&d, 0).face_auth_unit(), None);
        // A truncated XU (bmControls past its end) is skipped, not a panic
        let mut cut = iface(2, 1);
        cut.extend([0x18, 0x24, 0x06]);
        cut.extend(&ir_msxu[3..24]);
        assert!(parse_function(&cut, 2).xus.is_empty());
    }

    #[test]
    fn payloads() {
        let max = [0x01, 0x03, 0x03, 0, 0, 0, 0, 0, 0];
        assert_eq!(entries(&max), [(3, 3)]);
        assert_eq!(payload(3, ALTERNATE, 9).unwrap(), [0x01, 0x03, 0x02, 0, 0, 0, 0, 0, 0]);
        let long = payload(3, ALTERNATE, 17).unwrap();
        assert_eq!(&long[..9], [0x01, 0x03, 0x02, 0, 0, 0, 0, 0, 0]);
        assert!(long[9..].iter().all(|b| *b == 0));
        assert_eq!(payload(3, ALTERNATE, 8), None);
        let cur = [0x01, 0x03, 0x01, 0, 0, 0, 0, 0, 0];
        assert_eq!(plan(0x03, 9, &max, &cur, &[3]), Plan::Set { interface: 3, mode: Mode::Alternate, payload: payload(3, ALTERNATE, 9).unwrap() });
        assert_eq!(describe(&max), "interface 3: disabled+alternate");
    }

    #[test]
    fn interface_choice() {
        let two = [0x02, 0x03, 0x03, 0, 0, 0, 0, 0, 0, 0x05, 0x05, 0, 0, 0, 0, 0, 0];
        assert!(matches!(plan(3, 17, &two, &[], &[3]), Plan::Set { interface: 3, mode: Mode::Alternate, .. }));
        assert!(matches!(plan(3, 17, &two, &[], &[5]), Plan::Set { interface: 5, mode: Mode::Subtract, .. }));
        assert!(matches!(plan(3, 17, &two, &[], &[]), Plan::Refuse(w) if w.contains("several")));
        assert!(matches!(plan(3, 17, &two, &[], &[3, 5]), Plan::Refuse(w) if w.contains("several")));
        assert!(matches!(plan(3, 17, &two, &[], &[7]), Plan::Refuse(w) if w.contains("none of them")));
        // reserved bits are never sent
        let reserved = [0x01, 0x03, 0xfb, 0x01, 0, 0, 0, 0, 0];
        assert_eq!(plan(3, 9, &reserved, &[], &[3]), Plan::Set { interface: 3, mode: Mode::Alternate, payload: payload(3, ALTERNATE, 9).unwrap() });
    }

    /// A camera answering from a table; records the SET_CURs it gets.
    struct Fake {
        values: HashMap<u8, Vec<u8>>,
        writes: Vec<Vec<u8>>,
    }

    impl Fake {
        fn new(info: u8, max: &[u8], cur: &[u8], def: &[u8]) -> Fake {
            let len = max.len() as u16;
            let values = HashMap::from([
                (GET_INFO, vec![info]),
                (GET_LEN, len.to_le_bytes().to_vec()),
                (GET_MAX, max.to_vec()),
                (GET_CUR, cur.to_vec()),
                (GET_DEF, def.to_vec()),
                (GET_MIN, vec![0; max.len()]),
                (GET_RES, vec![0; max.len()]),
            ]);
            Fake { values, writes: Vec::new() }
        }
    }

    impl Camera for Fake {
        fn query(&mut self, unit: u8, selector: u8, req: u8, data: &mut [u8]) -> Result<(), Errno> {
            if (unit, selector) != (7, SEL_FACE_AUTH) {
                return Err(Errno::ENOENT);
            }
            if req == SET_CUR {
                if self.values[&GET_INFO][0] & 2 == 0 {
                    return Err(Errno::EBADRQC);
                }
                self.writes.push(data.to_vec());
                self.values.insert(GET_CUR, data.to_vec());
                return Ok(());
            }
            let v = self.values.get(&req).ok_or(Errno::EBADRQC)?;
            if v.len() != data.len() {
                return Err(Errno::ENOBUFS);
            }
            data.copy_from_slice(v);
            Ok(())
        }
    }

    const DISABLED_CUR: [u8; 9] = [0x01, 0x03, 0x01, 0, 0, 0, 0, 0, 0];

    #[test]
    fn fake_camera_alternate_then_already() {
        let mut cam = Fake::new(0x03, &[0x01, 0x03, 0x03, 0, 0, 0, 0, 0, 0], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut cam, 7, &[3]), Ok(Plan::Set { interface: 3, mode: Mode::Alternate, .. })));
        assert_eq!(cam.writes, [vec![0x01, 0x03, 0x02, 0, 0, 0, 0, 0, 0]]);
        assert_eq!(enable_on(&mut cam, 7, &[3]), Ok(Plan::Already { interface: 3, mode: Mode::Alternate }));
        assert_eq!(cam.writes.len(), 1, "already set: no second write");
    }

    #[test]
    fn fake_camera_subtract_only() {
        let mut cam = Fake::new(0x03, &[0x01, 0x03, 0x05, 0, 0, 0, 0, 0, 0], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut cam, 7, &[3]), Ok(Plan::Set { mode: Mode::Subtract, .. })));
        assert_eq!(cam.writes, [vec![0x01, 0x03, 0x04, 0, 0, 0, 0, 0, 0]]);
    }

    #[test]
    fn fake_camera_refusals_write_nothing() {
        let mut ro = Fake::new(0x01, &[0x01, 0x03, 0x03, 0, 0, 0, 0, 0, 0], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut ro, 7, &[3]), Err(IrError::Refused(w)) if w.contains("read-only")));
        assert!(matches!(restore_on(&mut ro, 7), Err(IrError::Refused(_))));
        assert!(ro.writes.is_empty());
        let mut none = Fake::new(0x03, &[0x01, 0x03, 0x01, 0, 0, 0, 0, 0, 0], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut none, 7, &[3]), Err(IrError::Refused(w)) if w.contains("no illumination mode")));
        let mut empty = Fake::new(0x03, &[0x00; 9], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut empty, 7, &[3]), Err(IrError::Refused(_))));
        assert!(none.writes.is_empty() && empty.writes.is_empty());
        // A failing request is an error, not a refusal
        let mut cam = Fake::new(0x03, &[0x01, 0x03, 0x03, 0, 0, 0, 0, 0, 0], &DISABLED_CUR, &DISABLED_CUR);
        assert!(matches!(enable_on(&mut cam, 4, &[3]), Err(IrError::Failed(w)) if w.starts_with("GET_INFO: ")));
    }

    #[test]
    fn fake_camera_restore_writes_def() {
        let on = [0x01, 0x03, 0x02, 0, 0, 0, 0, 0, 0];
        let mut cam = Fake::new(0x03, &[0x01, 0x03, 0x03, 0, 0, 0, 0, 0, 0], &on, &DISABLED_CUR);
        assert_eq!(restore_on(&mut cam, 7), Ok(Restored::Written(DISABLED_CUR.to_vec())));
        assert_eq!(cam.writes, [DISABLED_CUR.to_vec()]);
        assert_eq!(restore_on(&mut cam, 7), Ok(Restored::AlreadyDefault(DISABLED_CUR.to_vec())));
        assert_eq!(cam.writes.len(), 1);
    }

    #[test]
    fn run_passes_the_exit_status() {
        let cmd = |s: &str| [OsString::from("sh"), "-c".into(), s.into()];
        assert_eq!(run_with(|| Ok("on".into()), &cmd("exit 7")), 7);
        let t = tempfile::tempdir().unwrap();
        let marker = t.path().join("ran");
        let rc = run_with(|| Err(IrError::NoCamera("none".into())), &cmd(&format!("touch {}; exit 3", marker.display())));
        assert_eq!(rc, 3);
        assert!(marker.exists(), "the command runs without a qualifying camera");
        assert_eq!(run_with(|| Ok("on".into()), &[OsString::from("/nonexistent/cmd")]), 127);
    }

    #[test]
    fn fake_sysfs_discovery() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let usb = root.join("sys/devices/pci0000:00/usb3/3-1");
        for (intf, vc) in [("3-1:1.0", "00"), ("3-1:1.2", "02")] {
            fs::create_dir_all(usb.join(intf)).unwrap();
            fs::write(usb.join(intf).join("bInterfaceNumber"), format!("{vc}\n")).unwrap();
        }
        fs::write(usb.join("idVendor"), "30c9\n").unwrap();
        fs::write(usb.join("idProduct"), "00c2\n").unwrap();
        fs::write(usb.join("descriptors"), FIXTURE).unwrap();
        let class = root.join("sys/class/video4linux");
        fs::create_dir_all(root.join("dev/v4l/by-path")).unwrap();
        for (n, intf, index) in [(0, "3-1:1.0", 0), (1, "3-1:1.0", 1), (2, "3-1:1.2", 0), (3, "3-1:1.2", 1)] {
            let d = class.join(format!("video{n}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("index"), format!("{index}\n")).unwrap();
            fs::write(d.join("name"), "Integrated RGB Camera\n").unwrap();
            symlink(usb.join(intf), d.join("device")).unwrap();
            fs::write(root.join(format!("dev/video{n}")), "").unwrap();
        }
        let by = root.join("dev/v4l/by-path");
        symlink("../../video2", by.join("pci-0000:63:00.4-usbv2-0:1:1.2-video-index0")).unwrap();
        symlink("../../video2", by.join("pci-0000:63:00.4-usb-0:1:1.2-video-index0")).unwrap();
        symlink("../../video0", by.join("pci-0000:63:00.4-usb-0:1:1.0-video-index0")).unwrap();
        let nodes = scan_at(root);
        assert_eq!(nodes.iter().map(|n| n.node.display().to_string()).collect::<Vec<_>>(), ["/dev/video0", "/dev/video2"]);
        let ir: Vec<&Node> = nodes.iter().filter(|n| n.qualifies()).collect();
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0].device(), Path::new("/dev/v4l/by-path/pci-0000:63:00.4-usb-0:1:1.2-video-index0"));
        assert_eq!((ir[0].vid.as_str(), ir[0].pid.as_str(), ir[0].vc_interface, ir[0].usb_path.as_str()), ("30c9", "00c2", 2, "3-1"));
        assert_eq!(node_at(root, "video3").unwrap().index, 1);
    }

    #[test]
    fn pam_line_kinds() {
        let kast = "auth      optional   pam_exec.so quiet /usr/local/bin/kast ir-emitter enable --device /dev/video2";
        assert_eq!(emitter_line(kast), Some(EmitterLine::Kast));
        assert_eq!(
            emitter_line("auth optional pam_exec.so log=/tmp/x /usr/local/bin/ir-msxu --device /dev/video2 set --mode alternate --yes"),
            Some(EmitterLine::Msxu)
        );
        assert_eq!(emitter_line(&format!("# {kast}")), None);
        assert_eq!(emitter_line("auth sufficient pam_howdy.so"), None);
        assert_eq!(emitter_line("auth optional pam_exec.so /usr/bin/true"), None);
    }
}
