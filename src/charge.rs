//! Battery charging on Lenovo IdeaPads: the charge mode (`charge_types` of ideapad_laptop:
//! Standard, Fast, Long_Life) and overnight charging, which holds the battery at about 80%
//! while plugged in at night. The kernel driver doesn't know about overnight charging; it lives
//! in the embedded controller, behind the ACPI method `VPC0.BNMC`, reached through acpi_call.
//!
//! The BNMC arguments come from Lenovo Legion Toolkit's BatteryNightChargeFeature and were
//! confirmed only on the Lenovo 83D3 (IdeaPad Pro 5 14AHP9 / XiaoXin Pro 14 AHP9): ACPI method
//! names and arguments are model-specific, so other machines are refused unless forced.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

/// (sys_vendor, product_name) where the BNMC values were confirmed.
const SUPPORTED: [(&str, &str); 1] = [("LENOVO", "83D3")];

const VPC: &str = r"\_SB.PCI0.LPC0.EC0.VPC0";
/// BNMC: bit 0 = supported, bit 4 = on
const NIGHT_QUERY: u64 = 0x11;
const NIGHT_OFF: u64 = 0x12;
const NIGHT_ON: u64 = 0x8000_0012;
/// GBMD bits
const GBMD_RAPID: u64 = 1 << 2;
const GBMD_CONSERVATION: u64 = 1 << 5;

/// Writes can time out (the EC mutex is shared with ucsi_acpi, kernel bug 221065): try this often.
const TRIES: usize = 3;
const RETRY_PAUSE: Duration = Duration::from_millis(300);

#[derive(Subcommand)]
pub enum ChargeCmd {
    /// Charge mode, overnight charging, level and power (reading overnight charging needs root)
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Switch the charge mode: standard, fast (rapid charge) or conservation (stops at about 80%)
    Mode(ModeArgs),
    /// Overnight charging on or off (holds the battery at about 80% while plugged in at night)
    #[command(visible_alias = "o9")]
    Overnight {
        state: OnOff,
        /// Call the ACPI method on a machine other than the Lenovo 83D3 (model-specific: it may do something else)
        #[arg(long)]
        force: bool,
    },
}

impl ChargeCmd {
    pub fn needs_root(&self) -> bool {
        !matches!(self, ChargeCmd::Status { .. })
    }
}

#[derive(Args)]
#[command(group(ArgGroup::new("which").required(true).args(["mode", "standard", "fast", "conservation"])))]
pub struct ModeArgs {
    mode: Option<Mode>,
    /// Standard
    #[arg(short, long)]
    standard: bool,
    /// Fast (rapid charge)
    #[arg(short, long)]
    fast: bool,
    /// Conservation (Long_Life)
    #[arg(short, long)]
    conservation: bool,
}

impl ModeArgs {
    fn mode(&self) -> Mode {
        match self.mode {
            Some(m) => m,
            None if self.fast => Mode::Fast,
            None if self.conservation => Mode::Conservation,
            None => Mode::Standard,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum Mode {
    Standard,
    #[value(alias = "rapid")]
    Fast,
    #[value(aliases = ["long-life", "long_life"])]
    Conservation,
}

impl Mode {
    /// The name in charge_types
    fn sysfs(self) -> &'static str {
        match self {
            Mode::Standard => "Standard",
            Mode::Fast => "Fast",
            Mode::Conservation => "Long_Life",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Standard => "standard",
            Mode::Fast => "fast",
            Mode::Conservation => "conservation",
        }
    }
}

/// A charge_types entry as kast names it (unknown ones as they are).
fn mode_name(sysfs: &str) -> String {
    [Mode::Standard, Mode::Fast, Mode::Conservation].into_iter().find(|m| m.sysfs() == sysfs).map_or_else(|| sysfs.to_string(), |m| m.name().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

pub fn run(cmd: Option<ChargeCmd>) -> Result<()> {
    let sys = Sys::live();
    match cmd.unwrap_or(ChargeCmd::Status { json: false }) {
        ChargeCmd::Status { json } => cmd_status(&sys, json),
        ChargeCmd::Mode(a) => cmd_mode(&sys, a.mode()),
        ChargeCmd::Overnight { state, force } => cmd_overnight(&sys, state == OnOff::On, force),
    }
}

// --- sysfs -------------------------------------------------------------------------------

/// Where sysfs and procfs are: `/` on the machine, a temporary tree in tests.
pub struct Sys {
    root: PathBuf,
}

impl Sys {
    pub fn live() -> Sys {
        Sys { root: PathBuf::from("/") }
    }

    fn supplies(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.root.join("sys/class/power_supply")).into_iter().flatten().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    }

    /// The battery with charge_types, else the first battery.
    fn battery(&self) -> Option<PathBuf> {
        let bats: Vec<PathBuf> = self.supplies().into_iter().filter(|p| read(&p.join("type")).as_deref() == Some("Battery")).collect();
        bats.iter().find(|p| p.join("charge_types").exists()).or(bats.first()).cloned()
    }

    /// Any mains or USB supply online; None when there is none to ask.
    fn adapter_online(&self) -> Option<bool> {
        let online: Vec<bool> = self
            .supplies()
            .iter()
            .filter(|p| matches!(read(&p.join("type")).as_deref(), Some("Mains" | "USB")))
            .filter_map(|p| read(&p.join("online")))
            .map(|o| o != "0")
            .collect();
        (!online.is_empty()).then(|| online.contains(&true))
    }

    fn dmi(&self, key: &str) -> String {
        read(&self.root.join("sys/class/dmi/id").join(key)).unwrap_or_default()
    }

    fn acpi_call(&self) -> PathBuf {
        self.root.join("proc/acpi/call")
    }

    /// Ok on a machine the BNMC values were confirmed on, else why not.
    fn supported(&self) -> Result<(), String> {
        let (vendor, product) = (self.dmi("sys_vendor"), self.dmi("product_name"));
        if SUPPORTED.iter().any(|(v, p)| *v == vendor && *p == product) {
            Ok(())
        } else {
            let name = [vendor.as_str(), product.as_str(), self.dmi("product_version").as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            Err(format!("only confirmed on the Lenovo 83D3, this is {}", if name.is_empty() { "an unknown machine" } else { &name }))
        }
    }
}

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// `Fast [Standard] Long_Life` → (all of them, the bracketed one).
fn parse_charge_types(s: &str) -> (Vec<String>, Option<String>) {
    let mut all = Vec::new();
    let mut active = None;
    for w in s.split_whitespace() {
        match w.strip_prefix('[').and_then(|w| w.strip_suffix(']')) {
            Some(a) => {
                active = Some(a.to_string());
                all.push(a.to_string());
            }
            None => all.push(w.to_string()),
        }
    }
    (all, active)
}

struct Battery {
    name: String,
    model: Option<String>,
    status: Option<String>,
    capacity: Option<u32>,
    /// W, from power_now or current_now × voltage_now
    power: Option<f64>,
    modes: Vec<String>,
    mode: Option<String>,
}

fn battery(sys: &Sys) -> Option<Battery> {
    let p = sys.battery()?;
    let num = |f: &str| read(&p.join(f)).and_then(|s| s.parse::<f64>().ok());
    let power = num("power_now").or_else(|| Some(num("current_now")? * num("voltage_now")? / 1e6)).map(|uw| uw.abs() / 1e6);
    let (modes, mode) = read(&p.join("charge_types")).map(|s| parse_charge_types(&s)).unwrap_or_default();
    Some(Battery {
        name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        model: read(&p.join("model_name")).filter(|s| !s.is_empty()),
        status: read(&p.join("status")),
        capacity: read(&p.join("capacity")).and_then(|s| s.parse().ok()),
        power,
        modes,
        mode,
    })
}

// --- ACPI --------------------------------------------------------------------------------

/// Calls into ACPI; the fake EC in the tests stands in for /proc/acpi/call.
pub trait Acpi {
    /// `method` with its arguments, as acpi_call takes them; the answer without NULs.
    fn call(&mut self, method: &str) -> Result<String>;
}

pub struct ProcCall(PathBuf);

impl Acpi for ProcCall {
    fn call(&mut self, method: &str) -> Result<String> {
        // One write per call: acpi_call parses each write on its own
        let mut f = fs::OpenOptions::new().write(true).open(&self.0).with_context(|| format!("opening {}", self.0.display()))?;
        f.write_all(method.as_bytes()).with_context(|| format!("calling {method}"))?;
        drop(f);
        let raw = fs::read(&self.0).with_context(|| format!("reading {}", self.0.display()))?;
        let answer = String::from_utf8_lossy(&raw).replace('\0', "").trim().to_string();
        if answer.starts_with("Error") || answer == "not called" {
            bail!("{method}: {answer}");
        }
        Ok(answer)
    }
}

/// /proc/acpi/call, loading acpi_call when it's missing.
fn open_acpi(sys: &Sys) -> Result<ProcCall> {
    let path = sys.acpi_call();
    if !path.exists() {
        let _ = std::process::Command::new("modprobe").arg("acpi_call").stderr(std::process::Stdio::null()).status();
    }
    if !path.exists() {
        bail!("the acpi_call module isn't loaded and modprobe can't load it: install acpi_call-dkms (acpi_call for the stock linux kernel)");
    }
    Ok(ProcCall(path))
}

/// The machine check, then acpi_call: everything an ACPI call needs.
fn acpi_for(sys: &Sys, force: bool) -> Result<ProcCall> {
    if let Err(why) = sys.supported() {
        if !force {
            bail!("overnight charging is a Lenovo ACPI method that kast only calls on known models ({why}); --force calls it anyway");
        }
        eprintln!("warning: {why}; calling BNMC anyway (--force)");
    }
    if !nix::unistd::Uid::effective().is_root() {
        bail!("overnight charging is reached through /proc/acpi/call, which needs root: sudo kast charge");
    }
    open_acpi(sys)
}

fn parse_hex(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Night {
    raw: u64,
}

impl Night {
    fn supported(self) -> bool {
        self.raw & 1 != 0
    }

    fn on(self) -> bool {
        self.raw & (1 << 4) != 0
    }
}

fn bnmc(acpi: &mut dyn Acpi, arg: u64) -> Result<String> {
    acpi.call(&format!("{VPC}.BNMC {arg:#x}"))
}

fn query_night(acpi: &mut dyn Acpi) -> Result<Night> {
    let r = bnmc(acpi, NIGHT_QUERY)?;
    let raw = parse_hex(&r).with_context(|| format!("BNMC {NIGHT_QUERY:#x} answered {r:?}, not a number"))?;
    Ok(Night { raw })
}

/// Switch it, re-reading after each try; the state the EC reports in the end.
fn set_night(acpi: &mut dyn Acpi, on: bool) -> Result<Night> {
    let mut last_err = None;
    for k in 0..TRIES {
        if k > 0 {
            std::thread::sleep(RETRY_PAUSE);
        }
        if let Err(e) = bnmc(acpi, if on { NIGHT_ON } else { NIGHT_OFF }) {
            last_err = Some(e);
        }
        match query_night(acpi) {
            Ok(n) if n.on() == on => return Ok(n),
            Ok(_) => {}
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => query_night(acpi),
    }
}

fn gbmd(acpi: &mut dyn Acpi) -> Option<u64> {
    acpi.call(&format!("{VPC}.GBMD")).ok().and_then(|r| parse_hex(&r))
}

// --- Commands ----------------------------------------------------------------------------

/// What `status` knows of overnight charging.
enum NightState {
    Known(Night),
    Unknown(String),
}

fn night_state(sys: &Sys) -> (NightState, Option<u64>) {
    if let Err(why) = sys.supported() {
        return (NightState::Unknown(format!("not checked: {why}")), None);
    }
    if !nix::unistd::Uid::effective().is_root() {
        return (NightState::Unknown("reading it needs root: sudo kast charge".into()), None);
    }
    let mut acpi = match open_acpi(sys) {
        Ok(a) => a,
        Err(e) => return (NightState::Unknown(e.to_string()), None),
    };
    let night = match query_night(&mut acpi) {
        Ok(n) if n.supported() => NightState::Known(n),
        Ok(n) => NightState::Unknown(format!("not supported by this firmware (BNMC answered {:#x})", n.raw)),
        Err(e) => NightState::Unknown(e.to_string()),
    };
    (night, gbmd(&mut acpi))
}

fn power_text(b: &Battery) -> String {
    let mut s = String::new();
    if let Some(c) = b.capacity {
        s += &format!("{c}%");
    }
    if let Some(st) = &b.status {
        s += &format!("{}{}", if s.is_empty() { "" } else { ", " }, st.to_lowercase());
    }
    if let Some(w) = b.power.filter(|w| *w > 0.05) {
        s += &format!(" at {w:.1} W");
    }
    s
}

/// Why a plugged-in battery below 95% isn't charging, as far as kast can tell.
fn held_back(b: &Battery, adapter: Option<bool>, night: &NightState) -> Option<String> {
    if adapter != Some(true) || b.status.as_deref() != Some("Not charging") || b.capacity.is_none_or(|c| c >= 95) {
        return None;
    }
    Some(if b.mode.as_deref() == Some(Mode::Conservation.sysfs()) {
        "Not charging: conservation mode stops at about 80% (kast charge mode standard)".into()
    } else {
        match night {
            NightState::Known(n) if n.on() => "Not charging: overnight charging holds it at about 80% (sudo kast charge overnight off)".into(),
            NightState::Known(_) => "Not charging although plugged in, with conservation and overnight charging off".into(),
            NightState::Unknown(_) => "Not charging although plugged in: overnight charging may be holding it (sudo kast charge)".into(),
        }
    })
}

fn cmd_status(sys: &Sys, as_json: bool) -> Result<()> {
    let Some(b) = battery(sys) else { bail!("no battery in /sys/class/power_supply") };
    let adapter = sys.adapter_online();
    let (night, gbmd) = night_state(sys);
    let mode = b.mode.as_deref().map(mode_name);
    let hint = held_back(&b, adapter, &night);

    if as_json {
        let night_v = match &night {
            NightState::Known(n) => json!({ "state": if n.on() { "on" } else { "off" }, "raw": format!("{:#x}", n.raw) }),
            NightState::Unknown(why) => json!({ "state": "unknown", "reason": why }),
        };
        let v: Value = json!({
            "battery": b.name,
            "model": b.model,
            "status": b.status,
            "capacity": b.capacity,
            "power_w": b.power.map(|w| (w * 10.0).round() / 10.0),
            "adapter_online": adapter,
            "mode": mode,
            "modes": b.modes.iter().map(|m| mode_name(m)).collect::<Vec<_>>(),
            "overnight": night_v,
            "gbmd": gbmd.map(|g| json!({
                "raw": format!("{g:#x}"),
                "rapid": g & GBMD_RAPID != 0,
                "conservation": g & GBMD_CONSERVATION != 0,
            })),
            "held_back": hint,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    let model = b.model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
    println!("Battery:            {}{model}: {}", b.name, power_text(&b));
    match adapter {
        Some(true) => println!("Adapter:            plugged in"),
        Some(false) => println!("Adapter:            unplugged"),
        None => {}
    }
    match &mode {
        Some(m) => {
            let others: Vec<String> = b.modes.iter().map(|m| mode_name(m)).collect();
            println!("Charge mode:        {m}  (of {})", others.join(", "));
        }
        None => println!("Charge mode:        unknown ({} has no charge_types; ideapad_laptop isn't loaded?)", b.name),
    }
    match &night {
        NightState::Known(n) => println!("Overnight charging: {}", if n.on() { "on (holds the battery at about 80% at night)" } else { "off" }),
        NightState::Unknown(why) => println!("Overnight charging: unknown ({why})"),
    }
    if let Some(h) = hint {
        println!("\n{h}");
    }
    Ok(())
}

/// Write the mode, re-reading after each try; the active mode in the end.
fn write_mode(path: &Path, m: Mode) -> Result<Option<String>> {
    let (mut err, mut active) = (None, None);
    for k in 0..TRIES {
        if k > 0 {
            std::thread::sleep(RETRY_PAUSE);
        }
        err = fs::write(path, m.sysfs()).err();
        active = read(path).and_then(|s| parse_charge_types(&s).1);
        if active.as_deref() == Some(m.sysfs()) {
            return Ok(active);
        }
    }
    match err {
        Some(e) => Err(e).with_context(|| format!("writing {}", path.display())),
        None => Ok(active),
    }
}

fn cmd_mode(sys: &Sys, m: Mode) -> Result<()> {
    let Some(bat) = sys.battery() else { bail!("no battery in /sys/class/power_supply") };
    let path = bat.join("charge_types");
    let Some(text) = read(&path) else { bail!("{} is missing: the charge mode needs the ideapad_laptop driver (kernel 6.18 or newer)", path.display()) };
    let (all, active) = parse_charge_types(&text);
    if !all.iter().any(|a| a == m.sysfs()) {
        bail!("this battery has no {} mode (charge_types offers {})", m.name(), all.iter().map(|a| mode_name(a)).collect::<Vec<_>>().join(", "));
    }
    if active.as_deref() == Some(m.sysfs()) {
        println!("Charge mode is already {}.", m.name());
        return Ok(());
    }
    let now = write_mode(&path, m)?;
    if now.as_deref() != Some(m.sysfs()) {
        bail!(
            "asked for {}, but charge_types still says {} (try again: the EC can be busy)",
            m.name(),
            now.as_deref().map_or_else(|| "nothing".into(), mode_name)
        );
    }
    println!("Charge mode: {} (was {})", m.name(), active.as_deref().map_or_else(|| "unknown".into(), mode_name));
    if m != Mode::Conservation
        && let Some(b) = battery(sys)
        && b.capacity.is_some_and(|c| c >= 95)
        && b.status.as_deref() != Some("Charging")
    {
        println!("The battery is at {}%: Lenovo's EC doesn't start charging again above 95%.", b.capacity.unwrap_or_default());
    }
    Ok(())
}

fn cmd_overnight(sys: &Sys, on: bool, force: bool) -> Result<()> {
    let mut acpi = acpi_for(sys, force)?;
    let before = query_night(&mut acpi)?;
    if !before.supported() {
        bail!("this firmware doesn't support overnight charging (BNMC {NIGHT_QUERY:#x} answered {:#x})", before.raw);
    }
    let word = |b: bool| if b { "on" } else { "off" };
    if before.on() == on {
        println!("Overnight charging is already {}.", word(on));
        return Ok(());
    }
    let after = set_night(&mut acpi, on)?;
    if after.on() != on {
        bail!("asked for {}, but the EC still reports overnight charging {} (BNMC answered {:#x})", word(on), word(after.on()), after.raw);
    }
    println!("Overnight charging: {} (was {})", word(on), word(before.on()));
    if !on {
        println!("Lenovo Vantage or PC Manager on Windows can switch it on again; the setting lives in the EC, for every OS.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, Sys) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let w = |p: &str, s: &str| {
            let p = root.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, s).unwrap();
        };
        w("sys/class/power_supply/BAT0/type", "Battery\n");
        w("sys/class/power_supply/BAT0/charge_types", "Fast [Standard] Long_Life\n");
        w("sys/class/power_supply/BAT0/status", "Not charging\n");
        w("sys/class/power_supply/BAT0/capacity", "79\n");
        w("sys/class/power_supply/BAT0/power_now", "0\n");
        w("sys/class/power_supply/BAT0/model_name", "L23N4PF1\n");
        w("sys/class/power_supply/ACAD/type", "Mains\n");
        w("sys/class/power_supply/ACAD/online", "1\n");
        w("sys/class/power_supply/ucsi-source-psy-USBC000:001/type", "USB\n");
        w("sys/class/power_supply/ucsi-source-psy-USBC000:001/online", "0\n");
        w("sys/class/dmi/id/sys_vendor", "LENOVO\n");
        w("sys/class/dmi/id/product_name", "83D3\n");
        let sys = Sys { root: root.to_path_buf() };
        (tmp, sys)
    }

    /// BNMC as the 83D3's EC answers it.
    struct FakeEc {
        on: bool,
        calls: Vec<String>,
        /// Ignore this many switch requests (a busy EC)
        ignore: usize,
    }

    impl Acpi for FakeEc {
        fn call(&mut self, method: &str) -> Result<String> {
            self.calls.push(method.to_string());
            let arg = method.strip_prefix(&format!("{VPC}.BNMC ")).and_then(parse_hex);
            match arg {
                Some(NIGHT_QUERY) => Ok(if self.on { "0x11" } else { "0x1" }.into()),
                Some(a @ (NIGHT_OFF | NIGHT_ON)) => {
                    if self.ignore > 0 {
                        self.ignore -= 1;
                    } else {
                        self.on = a == NIGHT_ON;
                    }
                    Ok("0x1".into())
                }
                _ => bail!("Error: AE_NOT_FOUND"),
            }
        }
    }

    #[test]
    fn charge_types() {
        assert_eq!(parse_charge_types("Fast [Standard] Long_Life\n"), (vec!["Fast".into(), "Standard".into(), "Long_Life".into()], Some("Standard".into())));
        assert_eq!(parse_charge_types("Standard Long_Life"), (vec!["Standard".into(), "Long_Life".into()], None));
        assert_eq!(mode_name("Long_Life"), "conservation");
        assert_eq!(mode_name("Trickle"), "Trickle");
    }

    #[test]
    fn hex_and_bits() {
        assert_eq!(parse_hex("0x11"), Some(0x11));
        assert_eq!(parse_hex("0x860004"), Some(0x860004));
        assert_eq!(parse_hex("Error: AE_NOT_FOUND"), None);
        assert!(Night { raw: 0x11 }.on() && Night { raw: 0x11 }.supported());
        assert!(!Night { raw: 0x1 }.on() && Night { raw: 0x1 }.supported());
        assert!(!Night { raw: 0 }.supported());
        assert_eq!(format!("{NIGHT_ON:#x}"), "0x80000012");
    }

    #[test]
    fn machine_check() {
        let (_tmp, sys) = tree();
        assert!(sys.supported().is_ok());
        fs::write(sys.root.join("sys/class/dmi/id/product_name"), "21KC\n").unwrap();
        let why = sys.supported().unwrap_err();
        assert!(why.contains("LENOVO 21KC"), "{why}");
    }

    #[test]
    fn reads_the_battery() {
        let (_tmp, sys) = tree();
        let b = battery(&sys).unwrap();
        assert_eq!((b.name.as_str(), b.capacity, b.mode.as_deref()), ("BAT0", Some(79), Some("Standard")));
        assert_eq!(sys.adapter_online(), Some(true));
        assert_eq!(power_text(&b), "79%, not charging");
        let h = held_back(&b, Some(true), &NightState::Known(Night { raw: 0x11 })).unwrap();
        assert!(h.contains("overnight off"), "{h}");
        assert!(held_back(&b, Some(false), &NightState::Known(Night { raw: 0x11 })).is_none());
    }

    #[test]
    fn mode_checks_before_writing() {
        let (_tmp, sys) = tree();
        let path = sys.root.join("sys/class/power_supply/BAT0/charge_types");
        cmd_mode(&sys, Mode::Standard).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "Fast [Standard] Long_Life\n");
        fs::write(&path, "[Standard] Long_Life\n").unwrap();
        let e = cmd_mode(&sys, Mode::Fast).unwrap_err().to_string();
        assert!(e.contains("no fast mode") && e.contains("standard, conservation"), "{e}");
        // A plain file isn't the driver: it keeps the bare name, which never reads back as active
        fs::write(&path, "Fast [Standard] Long_Life\n").unwrap();
        assert!(cmd_mode(&sys, Mode::Fast).unwrap_err().to_string().contains("still says nothing"));
    }

    #[test]
    fn switches_overnight_charging() {
        let mut ec = FakeEc { on: true, calls: vec![], ignore: 0 };
        let n = set_night(&mut ec, false).unwrap();
        assert!(!n.on());
        assert_eq!(ec.calls, [format!("{VPC}.BNMC 0x12"), format!("{VPC}.BNMC 0x11")]);
        assert!(set_night(&mut ec, true).unwrap().on());
        assert_eq!(ec.calls[2], format!("{VPC}.BNMC 0x80000012"));
    }

    #[test]
    fn retries_a_busy_ec_and_reports_what_it_says() {
        let mut ec = FakeEc { on: true, calls: vec![], ignore: 1 };
        assert!(!set_night(&mut ec, false).unwrap().on());
        assert_eq!(ec.calls.len(), 4);
        let mut stuck = FakeEc { on: true, calls: vec![], ignore: TRIES };
        assert!(set_night(&mut stuck, false).unwrap().on());
    }

    #[test]
    fn mode_flags() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            cmd: ChargeCmd,
        }
        let mode = |args: &[&str]| match T::try_parse_from([&["t"], args].concat()).map(|t| t.cmd) {
            Ok(ChargeCmd::Mode(a)) => Some(a.mode()),
            _ => None,
        };
        assert_eq!(mode(&["mode", "conservation"]), Some(Mode::Conservation));
        assert_eq!(mode(&["mode", "long-life"]), Some(Mode::Conservation));
        assert_eq!(mode(&["mode", "-f"]), Some(Mode::Fast));
        assert_eq!(mode(&["mode", "-s"]), Some(Mode::Standard));
        assert_eq!(mode(&["mode"]), None);
        assert_eq!(mode(&["mode", "-s", "-c"]), None);
        assert!(matches!(T::try_parse_from(["t", "o9", "off"]).map(|t| t.cmd), Ok(ChargeCmd::Overnight { state: OnOff::Off, force: false })));
    }
}
