use std::path::Path;

use crate::ctx::{ctx, have, ok, output, which};
use crate::ui::{C_BOLD, C_CYAN, C_GRAY, C_GREEN, C_RESET, C_YELLOW};

use super::{find_config, find_zalo_flags, format_bytes, howdy_model_path, kde_format_total_size, kde_has_settings, pid_list, process_running, running_pids};

fn installed(label: &str) -> String {
    format!("  Status:      {C_GREEN}{label}{C_RESET}")
}

fn not_detected() -> String {
    format!("  Status:      {C_GRAY}Not detected{C_RESET}")
}

fn app_section(n: u32, title: &str, app: &str, extra: impl Fn(&Path)) {
    println!("{C_CYAN}[{n}] {title}{C_RESET}");
    match find_config(app) {
        Some(cfg) => {
            println!("{}", installed("Installed"));
            println!("  Config Dir:  {} ({})", cfg.display(), format_bytes(&cfg));
            extra(&cfg);
        }
        None => println!("{}", not_detected()),
    }
    let pids = running_pids(app);
    if pids.is_empty() {
        println!("  Process:     {C_GRAY}Stopped{C_RESET}");
    } else {
        println!("  Process:     {C_YELLOW}Running (PID: {}){C_RESET}", pid_list(&pids));
    }
    println!();
}

fn media(cfg: &Path) {
    if cfg.join("media").is_dir() {
        println!("  Media:       {}", format_bytes(&cfg.join("media")));
    }
}

fn pacman_version(pkg: &str) -> Option<String> {
    output("pacman", &["-Q", pkg]).map(|o| o.split_whitespace().nth(1).unwrap_or("").to_string())
}

fn count_faces(out: &str) -> usize {
    out.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())).count()
}

pub fn cmd_status() {
    let c = ctx();
    println!("{C_BOLD}=== Applications Status & Storage Breakdown ==={C_RESET}\n");

    app_section(1, "BeeperTexts (~/.config/BeeperTexts)", "beeper", media);
    app_section(2, "ZaloData (~/.config/ZaloData)", "zalo", |cfg| {
        if let Some(flags) = find_zalo_flags() {
            println!("  Flags File:  {}", flags.display());
        }
        media(cfg);
    });
    app_section(3, "Vesktop (~/.config/vesktop)", "vesktop", |_| {});
    app_section(4, "Zen Browser (~/.config/zen)", "zen", |_| {});
    app_section(5, "Obsidian (~/.config/obsidian)", "obsidian", |_| {});
    app_section(6, "Fcitx5 (~/.config/fcitx5)", "fcitx5", |_| {});
    app_section(7, "Antigravity (~/.config/Antigravity)", "antigravity", |_| {});

    // [8] Howdy
    println!("{C_CYAN}[8] Howdy Face Recognition (/etc/howdy/models){C_RESET}");
    let pkg = ["howdy-next", "howdy-next-bin", "howdy"]
        .into_iter()
        .find_map(|p| pacman_version(p).map(|v| format!("{p} ({v})")))
        .or_else(|| which("howdy").map(|p| format!("Installed ({})", p.display())));
    match &pkg {
        Some(p) => println!("  Package:     {C_GREEN}{p}{C_RESET}"),
        None => println!("  Package:     {C_GRAY}Not detected (pacman / binary){C_RESET}"),
    }
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
    if exists {
        println!("  Face Model:  {C_GREEN}Enrolled{C_RESET} ({model_s}, {size})");
        println!("  Permissions: {perms} (Expected: 600 root:root)");
        if have("howdy") {
            let faces = if c.is_root {
                count_faces(&output("howdy", &["-U", &c.target_user, "list"]).unwrap_or_default()).to_string()
            } else if ok("sudo", &["-n", "true"]) {
                output("sudo", &["-n", "howdy", "-U", &c.target_user, "list"]).map_or("?".into(), |o| count_faces(&o).to_string())
            } else {
                "Permissions required (run with sudo to inspect)".into()
            };
            println!("  Enrolled ID: {C_BOLD}{faces} face profile(s){C_RESET}");
        }
    } else if !c.is_root && !ok("sudo", &["-n", "true"]) {
        println!("  Face Model:  {C_GRAY}Run with sudo to inspect /etc/howdy/models{C_RESET}");
    } else {
        println!("  Face Model:  {C_YELLOW}Not enrolled for {}{C_RESET} ({model_s})", c.target_user);
    }
    let howdy_pids = running_pids("howdy");
    if howdy_pids.is_empty() {
        println!("  Process:     {C_GRAY}Idle / Standby{C_RESET}");
    } else {
        println!("  Process:     {C_YELLOW}Active (PID: {}){C_RESET}", pid_list(&howdy_pids));
    }
    println!();

    // [9] KDE Plasma
    println!("{C_CYAN}[9] KDE Plasma (~/.config + ~/.local/share){C_RESET}");
    if kde_has_settings() {
        println!("{}", installed("Detected"));
        println!("  Config:      {} (kdeglobals, kwinrc, Plasma applets, shortcuts, …)", c.config_home.display());
        println!("  Data:        {} (plasma, kwin, icons, wallpapers, …)", c.data_home.display());
        println!("  Snapshot:    {} (curated settings only, not all of ~/.config)", kde_format_total_size());
    } else {
        println!("{}", not_detected());
    }
    if process_running("plasmashell", None) {
        println!("  Session:     {C_YELLOW}plasmashell running{C_RESET} (log out after restore for a clean apply)");
    } else {
        println!("  Session:     {C_GRAY}plasmashell not running{C_RESET}");
    }
    println!();

    // [10] Hyprland
    println!("{C_CYAN}[10] Hyprland (~/.config/hypr){C_RESET}");
    match find_config("hypr") {
        Some(cfg) => {
            println!("{}", installed("Detected"));
            println!("  Config Dir:  {} ({})", cfg.display(), format_bytes(&cfg));
        }
        None => println!("{}", not_detected()),
    }
    if process_running("Hyprland", None) {
        println!("  Session:     {C_YELLOW}Hyprland running{C_RESET}");
    } else {
        println!("  Session:     {C_GRAY}Hyprland not running{C_RESET}");
    }
}
