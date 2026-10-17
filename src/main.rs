//! kast (Kud3n's Arch Setup Tool) - Arch Linux / CachyOS fast setup TUI with an embedded App Vault
//! (backup/restore of app data, KDE Plasma, Hyprland and Howdy face models).

mod ctx;
mod interactive;
mod logio;
mod report;
mod setup;
mod tui;
mod ui;
mod vault;

use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::ctx::Reported;
use crate::ui::{C_RED, C_RESET};
use crate::vault::backup::{BackupOpts, cmd_backup};
use crate::vault::import::{ImportOpts, cmd_import};

#[derive(Parser)]
#[command(
    name = "kast",
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
    /// Print the log file's path, its last lines, or open or copy it
    #[command(
        visible_alias = "logs",
        after_help = "Which log: root runs (the app, sudo kast ...) write /var/log/kast.log; runs as your own user write\n\
                      ~/.local/state/kast.log. Without root, kast log reads the latter, or /var/log/kast.log when\n\
                      yours is empty or missing.\n\n\
                      --copy tries wl-copy (Wayland), xclip and xsel (X11), then the terminal (OSC 52). It copies the\n\
                      whole file, or its last 256 KiB when it is longer."
    )]
    Log(LogArgs),
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
    Info {
        file: Option<PathBuf>,
    },
}

const TARGETS: [&str; 16] = [
    "all",
    "beeper",
    "beepertexts",
    "zalo",
    "zalodata",
    "vesktop",
    "zen",
    "obsidian",
    "fcitx5",
    "antigravity",
    "howdy",
    "kde",
    "plasma",
    "kde-plasma",
    "hypr",
    "hyprland",
];

#[derive(Args)]
struct LogArgs {
    /// Print the last N lines
    #[arg(long, value_name = "N", conflicts_with_all = ["open", "copy"])]
    tail: Option<usize>,
    /// Open the log with the desktop's default app
    #[arg(long, conflicts_with = "copy")]
    open: bool,
    /// Copy the log to the clipboard
    #[arg(long)]
    copy: bool,
}

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
        Ok(ImportOpts {
            file,
            app: self.app,
            safety_backup: !self.no_safety_backup,
            kill: self.kill,
            force: self.force,
            confirmed: self.force,
            dry_run: self.dry_run,
        })
    }
}

/// The interactive restore/backup screens (no arguments, on a terminal) run as root like
/// the menu, so sudo asks for the password once, before the full-screen UI starts.
fn interactive_vault(cmd: &Option<Cmd>) -> bool {
    match cmd {
        Some(Cmd::Import(a)) => a.is_empty() && interactive_terminal(),
        Some(Cmd::Export(a)) => a.is_empty() && interactive_terminal(),
        _ => false,
    }
}

fn needs_root(cmd: &Option<Cmd>) -> bool {
    matches!(cmd, None | Some(Cmd::Auto | Cmd::Partition | Cmd::Repo | Cmd::Packages | Cmd::Services)) || interactive_vault(cmd)
}

fn dispatch(cmd: Option<Cmd>) -> Result<()> {
    match cmd {
        None => tui::run(tui::Start::Menu),
        Some(Cmd::Auto) => setup::run_automatic(),
        Some(Cmd::Partition) => setup::setup_data_partition(false, 0, 1, 1),
        Some(Cmd::Repo) => setup::add_kud3n_repo(false, 0, 1, 1),
        Some(Cmd::Packages) => {
            setup::install_packages(false, 0, 1, 1);
            Ok(())
        }
        Some(Cmd::Services) => setup::setup_services(false, 0, 1, 1),
        Some(Cmd::Import(a)) if a.is_empty() && interactive_terminal() => tui::run(tui::Start::Import),
        Some(Cmd::Import(a)) if a.is_empty() => interactive::import_data(),
        Some(Cmd::Import(a)) => cmd_import(a.into_opts()?),
        Some(Cmd::Export(a)) if a.is_empty() && interactive_terminal() => tui::run(tui::Start::Export),
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
        Some(Cmd::Log(a)) => logio::os::cmd_log(a.tail, a.open, a.copy),
        Some(Cmd::Test) => Ok(()),
    }
}

// --- Entry point --------------------------------------------------------------------

/// Ctrl+C / kill: undo temporary state (sudoers rule, staging dir, terminal) and exit
/// with the shell-style status.
fn install_signal_handlers() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) else { return };
    std::thread::spawn(move || {
        for sig in signals.forever() {
            // Ctrl+C in a pager or editor the app handed the terminal to is for that program
            if sig == SIGINT && tui::suspended() {
                continue;
            }
            let interrupted = ctx::pacman_interrupted();
            report::kill_child(std::time::Duration::from_secs(2));
            ctx::cleanup_temp_state();
            tui::restore_terminal();
            if ctx::stale_pacman_lock(interrupted, std::time::Duration::from_secs(2)) {
                eprintln!("pacman was stopped mid-run and left {0} behind. No pacman is running; remove it with: sudo rm {0}", ctx::PACMAN_LOCK);
            }
            std::process::exit(if sig == SIGINT { 130 } else { 143 });
        }
    });
}

/// Variables the sudo re-exec keeps.
const CARRIED_ENV: [&str; 10] = ["NO_COLOR", "KAST_PAGER", "PAGER", "VISUAL", "EDITOR", "TMUX", "STY", "WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY"];

/// The full-screen UI needs a terminal on both ends.
fn interactive_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.cmd.is_none() && !interactive_terminal() {
        // No terminal for the menu (piped, cron, ...): show what can be scripted instead
        let _ = <Cli as clap::CommandFactory>::command().print_help();
        return ExitCode::FAILURE;
    }

    if needs_root(&cli.cmd) && !nix::unistd::Uid::effective().is_root() {
        println!("Privileges required. Re-executing with sudo...");
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(std::env::args().next().unwrap_or_default()));
        let mut sudo = Command::new("sudo");
        // sudo resets the environment; carry over what the app uses: NO_COLOR, the user's
        // pager/editor for `e`/`E`, and the session/multiplexer for opening and copying the log
        let carried: Vec<std::ffi::OsString> = CARRIED_ENV
            .iter()
            .filter_map(|k| {
                let v = std::env::var_os(k).filter(|v| !v.is_empty())?;
                let mut kv = std::ffi::OsString::from(format!("{k}="));
                kv.push(v);
                Some(kv)
            })
            .collect();
        if !carried.is_empty() {
            sudo.arg("env").args(carried);
        }
        let err = sudo.arg(exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("{C_RED}[ERROR]{C_RESET} Could not run sudo: {err}");
        return ExitCode::FAILURE;
    }

    ctx::init();
    install_signal_handlers();

    let result = panic::catch_unwind(AssertUnwindSafe(|| dispatch(cli.cmd)));
    ctx::cleanup_temp_state();
    tui::restore_terminal();
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
