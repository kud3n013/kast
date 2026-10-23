use std::path::Path;

use crate::ctx::{ctx, have, ok, output, which};
use crate::ir::{self, IrState};
use crate::ui::{C_BOLD, C_CYAN, C_GRAY, C_GREEN, C_RESET, C_YELLOW};

use super::{find_config, find_zalo_flags, format_bytes, howdy_model_path, kde_format_total_size, kde_has_settings, pid_list, process_running, running_pids};

fn installed(label: &str) -> String {
    format!("  Status:      {C_GREEN}{label}{C_RESET}")
}

fn not_detected() -> String {
    format!("  Status:      {C_GRAY}Not detected{C_RESET}")
}

/// One application's section of `kast status`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppStatus {
    pub n: u32,
    pub title: &'static str,
    pub app: &'static str,
    /// (config dir, size) when installed
    pub config: Option<(String, String)>,
    /// Extra (label padded to 13 columns, value) lines
    pub extras: Vec<(String, String)>,
    pub pids: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    Enrolled {
        path: String,
        size: String,
        perms: String,
    },
    /// Not root and sudo would need a password
    NeedsSudo,
    NotEnrolled {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HowdyStatus {
    pub package: Option<String>,
    pub model: ModelStatus,
    /// Face profiles in the model (only when the howdy binary exists)
    pub faces: Option<String>,
    pub pids: Vec<i32>,
    pub ir: IrState,
    /// /etc/howdy or config.ini is readable by other users, so lock screens can't use Howdy
    pub config_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KdeStatus {
    pub detected: bool,
    pub config_home: String,
    pub data_home: String,
    pub snapshot_size: String,
    pub plasma_running: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HyprStatus {
    pub config: Option<(String, String)>,
    pub running: bool,
}

/// Everything `kast status` shows.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    pub user: String,
    pub apps: Vec<AppStatus>,
    pub howdy: HowdyStatus,
    pub kde: KdeStatus,
    pub hypr: HyprStatus,
}

fn media(cfg: &Path, extras: &mut Vec<(String, String)>) {
    if cfg.join("media").is_dir() {
        extras.push(("Media:       ".into(), format_bytes(&cfg.join("media"))));
    }
}

fn app_status(n: u32, title: &'static str, app: &'static str) -> AppStatus {
    let mut extras = Vec::new();
    let config = find_config(app).map(|cfg| {
        if app == "zalo"
            && let Some(flags) = find_zalo_flags()
        {
            extras.push(("Flags File:  ".into(), flags.display().to_string()));
        }
        if matches!(app, "beeper" | "zalo") {
            media(&cfg, &mut extras);
        }
        (cfg.display().to_string(), format_bytes(&cfg))
    });
    AppStatus { n, title, app, config, extras, pids: running_pids(app) }
}

fn pacman_version(pkg: &str) -> Option<String> {
    output("pacman", &["-Q", pkg]).map(|o| o.split_whitespace().nth(1).unwrap_or("").to_string())
}

fn count_faces(out: &str) -> usize {
    out.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())).count()
}

/// Detect everything (runs pgrep, du, pacman and maybe sudo -n: can take a moment).
pub fn collect_status() -> StatusReport {
    let c = ctx();
    let apps = vec![
        app_status(1, "BeeperTexts (~/.config/BeeperTexts)", "beeper"),
        app_status(2, "ZaloData (~/.config/ZaloData)", "zalo"),
        app_status(3, "Vesktop (~/.config/vesktop)", "vesktop"),
        app_status(4, "Zen Browser (~/.config/zen)", "zen"),
        app_status(5, "Helium (~/.config/net.imput.helium)", "helium"),
        app_status(6, "Obsidian (~/.config/obsidian)", "obsidian"),
        app_status(7, "Fcitx5 (~/.config/fcitx5)", "fcitx5"),
        app_status(8, "Antigravity (~/.config/Antigravity)", "antigravity"),
        app_status(9, "Free Download Manager (~/.config/Softdeluxe)", "fdm"),
        app_status(10, "fish (~/.config/fish)", "fish"),
        app_status(11, "fastfetch (~/.config/fastfetch)", "fastfetch"),
        app_status(12, "Scripts (~/.scripts)", "scripts"),
    ];

    let package = ["howdy-next", "howdy-next-bin", "howdy"]
        .into_iter()
        .find_map(|p| pacman_version(p).map(|v| format!("{p} ({v})")))
        .or_else(|| which("howdy").map(|p| format!("Installed ({})", p.display())));
    let model = howdy_model_path(&c.target_user);
    let model_s = model.to_string_lossy().into_owned();
    let (mut exists, mut size, mut perms) = (false, "0B".to_string(), String::new());
    if model.is_file() {
        exists = true;
        size = format_bytes(&model);
        perms = output("stat", &["-c", "%a %U:%G", &model_s]).unwrap_or_default().trim().to_string();
    } else if !c.is_root && have("sudo") && ok("sudo", &["-n", "test", "-f", &model_s]) {
        exists = true;
        size = output("sudo", &["-n", "du", "-sh", &model_s]).and_then(|o| o.split('\t').next().map(str::to_string)).unwrap_or_else(|| "unknown".into());
        perms = output("sudo", &["-n", "stat", "-c", "%a %U:%G", &model_s]).unwrap_or_default().trim().to_string();
    }
    let mut faces = None;
    let model_status = if exists {
        if have("howdy") {
            faces = Some(if c.is_root {
                count_faces(&output("howdy", &["-U", &c.target_user, "list"]).unwrap_or_default()).to_string()
            } else if ok("sudo", &["-n", "true"]) {
                output("sudo", &["-n", "howdy", "-U", &c.target_user, "list"]).map_or("?".into(), |o| count_faces(&o).to_string())
            } else {
                "Permissions required (run with sudo to inspect)".into()
            });
        }
        ModelStatus::Enrolled { path: model_s, size, perms }
    } else if !c.is_root && !ok("sudo", &["-n", "true"]) {
        ModelStatus::NeedsSudo
    } else {
        ModelStatus::NotEnrolled { path: model_s }
    };
    let device = std::fs::read_to_string("/etc/howdy/config.ini").ok().and_then(|ini| crate::setup::ini_value(&ini, "device_path"));
    let ir = ir::status(device.as_deref(), std::fs::read_to_string("/etc/pam.d/sudo").ok().as_deref());
    let config_open = !crate::setup::howdy_config_open().is_empty();
    let howdy = HowdyStatus { package, model: model_status, faces, pids: running_pids("howdy"), ir, config_open };

    let detected = kde_has_settings();
    let kde = KdeStatus {
        detected,
        config_home: c.config_home.display().to_string(),
        data_home: c.data_home.display().to_string(),
        snapshot_size: if detected { kde_format_total_size() } else { String::new() },
        plasma_running: process_running("plasmashell", None),
    };
    let hypr =
        HyprStatus { config: find_config("hypr").map(|cfg| (cfg.display().to_string(), format_bytes(&cfg))), running: process_running("Hyprland", None) };
    StatusReport { user: c.target_user.clone(), apps, howdy, kde, hypr }
}

pub fn cmd_status() {
    print_status(&collect_status());
}

/// Why lock screens fail with "Failed to open user model file", and the fix.
pub const LOCK_SCREEN_NOTE: &str = "can't use Howdy: /etc/howdy is readable by all users (sudo kast services makes it 0750)";

/// The IR emitter line of `kast status` (without colors).
pub fn ir_text(s: &IrState) -> String {
    match s {
        IrState::Supported { mode, pam: true, .. } => format!("supported ({}), switched on before Howdy by PAM", mode.name()),
        IrState::Supported { mode, pam: false, .. } => format!("supported ({}), PAM line not installed (sudo kast services)", mode.name()),
        IrState::Unusable { device, why } => format!("not usable on {device}: {why}"),
        IrState::NotSupported { device } => format!("not supported by this camera ({device})"),
        IrState::NoCamera => "no IR camera found".into(),
    }
}

fn print_status(r: &StatusReport) {
    println!("{C_BOLD}=== Applications Status & Storage Breakdown ==={C_RESET}\n");
    for a in &r.apps {
        println!("{C_CYAN}[{}] {}{C_RESET}", a.n, a.title);
        match &a.config {
            Some((dir, size)) => {
                println!("{}", installed("Installed"));
                println!("  Config Dir:  {dir} ({size})");
                for (label, value) in &a.extras {
                    println!("  {label}{value}");
                }
            }
            None => println!("{}", not_detected()),
        }
        if a.pids.is_empty() {
            println!("  Process:     {C_GRAY}Stopped{C_RESET}");
        } else {
            println!("  Process:     {C_YELLOW}Running (PID: {}){C_RESET}", pid_list(&a.pids));
        }
        println!();
    }

    // [8] Howdy
    let h = &r.howdy;
    println!("{C_CYAN}[8] Howdy Face Recognition (/etc/howdy/models){C_RESET}");
    match &h.package {
        Some(p) => println!("  Package:     {C_GREEN}{p}{C_RESET}"),
        None => println!("  Package:     {C_GRAY}Not detected (pacman / binary){C_RESET}"),
    }
    match &h.model {
        ModelStatus::Enrolled { path, size, perms } => {
            println!("  Face Model:  {C_GREEN}Enrolled{C_RESET} ({path}, {size})");
            println!("  Permissions: {perms} (Expected: 600 root:root)");
            if let Some(faces) = &h.faces {
                println!("  Enrolled ID: {C_BOLD}{faces} face profile(s){C_RESET}");
            }
        }
        ModelStatus::NeedsSudo => println!("  Face Model:  {C_GRAY}Run with sudo to inspect /etc/howdy/models{C_RESET}"),
        ModelStatus::NotEnrolled { path } => {
            println!("  Face Model:  {C_YELLOW}Not enrolled for {}{C_RESET} ({path})", r.user);
            println!("  Add one:     {}", ir::enroll_command(&r.user, h.ir.supported()));
        }
    }
    println!("  IR Emitter:  {}", ir_text(&h.ir));
    if h.ir.supported() && matches!(h.model, ModelStatus::Enrolled { .. }) {
        println!("  {C_YELLOW}Note:{C_RESET}       {}", ir::reenroll_note(&r.user));
    }
    if h.config_open {
        println!("  {C_YELLOW}Lock screen:{C_RESET} {}", LOCK_SCREEN_NOTE);
    }
    if h.pids.is_empty() {
        println!("  Process:     {C_GRAY}Idle / Standby{C_RESET}");
    } else {
        println!("  Process:     {C_YELLOW}Active (PID: {}){C_RESET}", pid_list(&h.pids));
    }
    println!();

    // [9] KDE Plasma
    let k = &r.kde;
    println!("{C_CYAN}[9] KDE Plasma (~/.config + ~/.local/share){C_RESET}");
    if k.detected {
        println!("{}", installed("Detected"));
        println!("  Config:      {} (kdeglobals, kwinrc, Plasma applets, shortcuts, …)", k.config_home);
        println!("  Data:        {} (plasma, kwin, icons, wallpapers, …)", k.data_home);
        println!("  Snapshot:    {} (curated settings only, not all of ~/.config)", k.snapshot_size);
    } else {
        println!("{}", not_detected());
    }
    if k.plasma_running {
        println!("  Session:     {C_YELLOW}plasmashell running{C_RESET} (log out after restore for a clean apply)");
    } else {
        println!("  Session:     {C_GRAY}plasmashell not running{C_RESET}");
    }
    println!();

    // [10] Hyprland
    println!("{C_CYAN}[10] Hyprland (~/.config/hypr){C_RESET}");
    match &r.hypr.config {
        Some((dir, size)) => {
            println!("{}", installed("Detected"));
            println!("  Config Dir:  {dir} ({size})");
        }
        None => println!("{}", not_detected()),
    }
    if r.hypr.running {
        println!("  Session:     {C_YELLOW}Hyprland running{C_RESET}");
    } else {
        println!("  Session:     {C_GRAY}Hyprland not running{C_RESET}");
    }
}
