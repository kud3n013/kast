//! kas (Kud3n's Arch Setup) - Arch Linux / CachyOS fast setup TUI with an embedded App Vault
//! (backup/restore of app data, KDE Plasma, Hyprland and Howdy face models).

mod ctx;
mod interactive;
mod setup;
mod ui;
mod vault;

use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::ctx::{ctx, Reported};
use crate::ui::{Key, C_BG_BLUE, C_BOLD, C_CYAN, C_GRAY, C_GREEN, C_RED, C_RESET, C_WHITE};
use crate::vault::backup::{cmd_backup, BackupOpts};
use crate::vault::import::{cmd_import, ImportOpts};

#[derive(Parser)]
#[command(
    name = "kas",
    version,
    about = "Arch Linux / CachyOS fast setup with an embedded App Vault backup engine",
    after_help = "Run without a command to launch the interactive TUI menu.\nSetup commands re-run themselves through sudo; vault commands work as your own user."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the full automatic setup: upgrade, repo, packages, services (no data partition or import)
    #[command(visible_alias = "automatic")]
    Auto,
    /// Set up the universal data partition (/media/data & user dir symlinks); not part of auto
    #[command(visible_alias = "data")]
    Partition,
    /// Add kud3n's repository ([archrepo] in /etc/pacman.conf)
    #[command(visible_alias = "archrepo")]
    Repo,
    /// Install packages (official, CachyOS, kud3n, add-ons)
    #[command(visible_alias = "pkgs")]
    Packages,
    /// Set up services (ProtonVPN & Fcitx5 packages, Howdy, Valkey/SearXNG)
    Services,
    /// Restore app data, KDE Plasma settings & face model (interactive without arguments)
    #[command(visible_alias = "restore")]
    Import(ImportArgs),
    /// Back up app data, KDE Plasma settings & face model (interactive without arguments)
    #[command(visible_alias = "backup")]
    Export(BackupArgs),
    /// Show detected applications & Howdy status, paths & sizes
    Status,
    /// Inspect a backup archive's manifest
    #[command(visible_alias = "inspect")]
    Info { file: Option<PathBuf> },
    /// Direct access to the app vault engine
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
    /// Do nothing (for checking that the tool runs)
    #[command(alias = "noop", hide = true)]
    Test,
}

#[derive(Subcommand)]
enum VaultCmd {
    Status,
    #[command(alias = "export")]
    Backup(BackupArgs),
    #[command(alias = "restore")]
    Import(ImportArgs),
    #[command(alias = "inspect")]
    Info { file: Option<PathBuf> },
}

const TARGETS: [&str; 16] = [
    "all", "beeper", "beepertexts", "zalo", "zalodata", "vesktop", "zen", "obsidian", "fcitx5", "antigravity", "howdy",
    "kde", "plasma", "kde-plasma", "hypr", "hyprland",
];

#[derive(Args, Default)]
struct BackupArgs {
    /// What to back up
    #[arg(value_parser = TARGETS)]
    target: Option<String>,
    /// Destination archive (.zip) or face model (.dat)
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Exclude large media folders (ZaloData/media, BeeperTexts/media, Zen web storage)
    #[arg(long)]
    no_media: bool,
    /// Kept for backward compatibility; does nothing
    #[arg(long, hide = true)]
    no_vaults: bool,
    /// Close/kill running target apps first to avoid dirty snapshots
    #[arg(short, long)]
    kill: bool,
    /// Skip confirmation prompts and proceed even if apps are running
    #[arg(short, long)]
    force: bool,
    /// Show actions without copying files or creating an archive
    #[arg(long)]
    dry_run: bool,
}

impl BackupArgs {
    fn is_empty(&self) -> bool {
        self.target.is_none() && self.output.is_none() && !self.no_media && !self.no_vaults && !self.kill && !self.force && !self.dry_run
    }

    fn into_opts(self) -> BackupOpts {
        BackupOpts {
            target: self.target.unwrap_or_else(|| "all".into()),
            output: self.output,
            include_media: !self.no_media,
            kill: self.kill,
            force: self.force,
            dry_run: self.dry_run,
        }
    }
}

#[derive(Args, Default)]
struct ImportArgs {
    /// Backup archive (.zip) or standalone face model (.dat)
    file: Option<PathBuf>,
    /// Only restore one app (beeper, zalo, vesktop, zen, obsidian, fcitx5, antigravity, howdy, kde, hypr)
    #[arg(long)]
    app: Option<String>,
    /// Skip the safety snapshot of existing files
    #[arg(long)]
    no_safety_backup: bool,
    /// Close/kill running target apps first
    #[arg(short, long)]
    kill: bool,
    /// Skip confirmation prompts and running-app checks
    #[arg(short, long)]
    force: bool,
    /// Show actions without making changes
    #[arg(long)]
    dry_run: bool,
}

impl ImportArgs {
    fn is_empty(&self) -> bool {
        self.file.is_none() && self.app.is_none() && !self.no_safety_backup && !self.kill && !self.force && !self.dry_run
    }

    fn into_opts(self) -> Result<ImportOpts> {
        let Some(file) = self.file else {
            vault::log_error("Missing zip archive parameter.");
            return Err(ctx::reported());
        };
        Ok(ImportOpts { file, app: self.app, safety_backup: !self.no_safety_backup, kill: self.kill, force: self.force, dry_run: self.dry_run })
    }
}

fn needs_root(cmd: &Option<Cmd>) -> bool {
    matches!(cmd, None | Some(Cmd::Auto | Cmd::Partition | Cmd::Repo | Cmd::Packages | Cmd::Services))
}

fn dispatch(cmd: Option<Cmd>) -> Result<()> {
    match cmd {
        None => run_menu(),
        Some(Cmd::Auto) => setup::run_automatic(),
        Some(Cmd::Partition) => setup::setup_data_partition(false, 0, 1, 1),
        Some(Cmd::Repo) => setup::add_kud3n_repo(false, 0, 1, 1),
        Some(Cmd::Packages) => {
            setup::install_packages(false, 0, 1, 1);
            Ok(())
        }
        Some(Cmd::Services) => setup::setup_services(false, 0, 1, 1),
        Some(Cmd::Import(a)) if a.is_empty() => interactive::import_data(),
        Some(Cmd::Import(a)) => cmd_import(a.into_opts()?),
        Some(Cmd::Export(a)) if a.is_empty() => interactive::export_data(),
        Some(Cmd::Export(a)) => cmd_backup(a.into_opts()),
        Some(Cmd::Status) | Some(Cmd::Vault { cmd: VaultCmd::Status }) => {
            vault::status::cmd_status();
            Ok(())
        }
        Some(Cmd::Info { file }) | Some(Cmd::Vault { cmd: VaultCmd::Info { file } }) => {
            vault::require_vault_tools()?;
            vault::info::cmd_info(file.as_deref())
        }
        Some(Cmd::Vault { cmd: VaultCmd::Backup(a) }) => cmd_backup(a.into_opts()),
        Some(Cmd::Vault { cmd: VaultCmd::Import(a) }) => cmd_import(a.into_opts()?),
        Some(Cmd::Test) => Ok(()),
    }
}

// --- Main menu -----------------------------------------------------------------------

const MENU_SHORT: [&str; 8] = [
    "1. Run full auto setup (Steps 3 -> 5)",
    "2. Setup data partition (optional)",
    "3. Add kud3n's repository",
    "4. Install packages (Official & custom)",
    "5. Setup services (VPN, IME, Howdy...)",
    "6. Import data (optional, not in auto)",
    "7. Export data (Vault, KDE & Face backup)",
    "Q. Exit",
];
const MENU_LONG: [&str; 8] = [
    "1. Run automatic script (Steps 3 -> 5)",
    "2. Setup data partition (optional, not in auto)",
    "3. Add kud3n's repository ([archrepo] in /etc/pacman.conf)",
    "4. Install packages (Official, CachyOS, kud3n, add-ons)",
    "5. Setup services (ProtonVPN, IME, Howdy, Valkey/SearXNG)",
    "6. Import data (App Vault, KDE & Face restore; not in auto)",
    "7. Export data (App Vault, KDE Plasma & Face Model backup)",
    "Q. Exit",
];

fn draw_menu(selected: usize) {
    let c = ctx();
    let (box_w, inner) = ui::term_dimensions();
    let hl = |l: &str, r: &str| format!("{C_CYAN}{l}{}{r}{C_RESET}\x1b[K\n", "─".repeat(box_w - 2));
    let mut buf = hl("╭", "╮");
    let (title, target) = if inner < 50 {
        ("◆ ARCH / CACHYOS SETUP ◆", format!("Target: {}", c.target_user))
    } else if inner < 68 {
        ("◆ ARCH LINUX / CACHYOS SETUP ◆", format!("Target: {} │ Home: {}", c.target_user, c.target_home.display()))
    } else {
        (
            "◆ ARCH LINUX / CACHYOS FAST SETUP CONTROLLER ◆",
            format!("Target: {} (UID: {}, GID: {}) │ Home: {}", c.target_user, c.target_uid, c.target_gid, c.target_home.display()),
        )
    };
    buf += &ui::box_line(&format!("{C_BOLD}{C_WHITE}{title}{C_RESET}"), inner);
    buf += &ui::box_line(&format!("{C_GRAY}{target}{C_RESET}"), inner);
    buf += &hl("├", "┤");
    let options = if inner < 64 { MENU_SHORT } else { MENU_LONG };
    for (i, opt) in options.iter().enumerate() {
        if i == selected {
            buf += &ui::box_line(&format!("{C_BG_BLUE}{C_WHITE}{C_BOLD} ❯ {opt} {C_RESET}"), inner);
        } else {
            buf += &ui::box_line(&format!("   {opt}"), inner);
        }
    }
    buf += &hl("├", "┤");
    let controls = if inner < 50 {
        "[↑/↓] Move │ [Enter] Run │ [Q] Quit"
    } else if inner < 68 {
        "[↑/↓] Move │ [1-7] Pick │ [Enter] Run │ [Q] Quit"
    } else {
        "Controls: [↑/↓] Navigate │ [1-7/Q] Select │ [Enter] Run │ [Ctrl+C] Exit"
    };
    buf += &ui::box_line(&format!("{C_GRAY}{controls}{C_RESET}"), inner);
    buf += &hl("╰", "╯");
    print!("\x1b[H{buf}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Run one menu action; a failure (or panic) returns to the menu instead of ending the TUI.
fn run_menu_action(action: usize) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| match action {
        0 => setup::run_automatic(),
        1 => setup::setup_data_partition(false, 0, 1, 1),
        2 => setup::add_kud3n_repo(false, 0, 1, 1),
        3 => {
            setup::install_packages(false, 0, 1, 1);
            Ok(())
        }
        4 => setup::setup_services(false, 0, 1, 1),
        5 => interactive::import_data(),
        _ => interactive::export_data(),
    }));
    ctx::cleanup_temp_state();
    let err = match result {
        Ok(Ok(())) => return,
        Ok(Err(e)) if e.is::<Reported>() => String::new(),
        Ok(Err(e)) => format!(": {e:#}"),
        Err(_) => " (internal error)".into(),
    };
    ui::restore_terminal_interactive();
    println!("\n{C_RED}[ERROR]{C_RESET} Action failed{err}. See {}", ctx().log_file.display());
    ui::read_choice("Press Enter to return to menu...");
}

fn run_menu() -> Result<()> {
    ui::init_terminal();
    let total = MENU_LONG.len();
    let mut selected = 0usize;
    loop {
        draw_menu(selected);
        let action = match ui::read_key() {
            Key::Up => {
                selected = (selected + total - 1) % total;
                None
            }
            Key::Down => {
                selected = (selected + 1) % total;
                None
            }
            Key::Enter if selected == total - 1 => break,
            Key::Enter => Some(selected),
            Key::Char(ch @ '1'..='7') => Some(ch as usize - '1' as usize),
            Key::Char('q' | 'Q') | Key::Eof => break,
            _ => None,
        };
        if let Some(a) = action {
            run_menu_action(a);
            ui::init_terminal();
        }
    }
    ui::cleanup_terminal();
    println!("{C_GREEN}Arch Linux Setup Controller exited.{C_RESET}");
    Ok(())
}

// --- Entry point --------------------------------------------------------------------

/// Ctrl+C / kill: undo temporary state (sudoers rule, staging dir, terminal) and exit
/// with the shell-style status.
fn install_signal_handlers() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) else { return };
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            ctx::cleanup_temp_state();
            ui::cleanup_terminal();
            std::process::exit(if sig == SIGINT { 130 } else { 143 });
        }
    });
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if needs_root(&cli.cmd) && !nix::unistd::Uid::effective().is_root() {
        println!("Privileges required. Re-executing with sudo...");
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(std::env::args().next().unwrap_or_default()));
        let err = Command::new("sudo").arg(exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("{C_RED}[ERROR]{C_RESET} Could not run sudo: {err}");
        return ExitCode::FAILURE;
    }

    ctx::init();
    install_signal_handlers();

    let result = panic::catch_unwind(AssertUnwindSafe(|| dispatch(cli.cmd)));
    ctx::cleanup_temp_state();
    ui::cleanup_terminal();
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            if !e.is::<Reported>() {
                eprintln!("{C_RED}[ERROR]{C_RESET} {e:#}");
            }
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}
