//! kast (Kud3n's Arch Setup Tool) - Arch Linux / CachyOS fast setup TUI with an embedded App Vault
//! (backup/restore of app data, KDE Plasma, Hyprland and Howdy face models).

mod ctx;
mod drive;
mod interactive;
mod ir;
mod journal;
mod logio;
mod partition;
mod perms;
mod pkglist;
mod plasma;
mod proton;
mod qr;
mod report;
mod schedule;
mod settings;
mod setup;
mod snapshot;
mod tui;
mod ui;
mod undo;
mod vault;

use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::ctx::Reported;
use crate::ui::{C_RED, C_RESET};
use crate::vault::backup::{self, BackupOpts, cmd_backup};
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
    /// For the menu: which package list to use (see `kast packages --help`)
    #[command(flatten)]
    list: ListSource,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the full automatic setup: upgrade, repo, packages, services (no data partition or import)
    #[command(visible_alias = "automatic")]
    Auto(AutoArgs),
    /// Mount a partition through /etc/fstab (NTFS, exFAT, FAT, ext4, btrfs, XFS); reversible with undo
    #[command(
        visible_alias = "data",
        after_help = "Without arguments: the interactive screen. `--list` shows the partitions and why a row can't be used;\n\
                      `--device … --dry-run` shows the exact fstab line without changing anything.\n\
                      Undo: `sudo kast undo --last --step partition`, or Undo changes in the menu."
    )]
    Partition(partition::cli::PartitionArgs),
    /// Add kud3n's repository ([archrepo] in /etc/pacman.conf)
    #[command(visible_alias = "archrepo")]
    Repo,
    /// Install packages (official, CachyOS, kud3n, add-ons)
    #[command(
        visible_alias = "pkgs",
        after_help = "The list is packages.toml from kud3n013/kast's main branch, downloaded on every run. When it can't be\n\
                      downloaded or isn't valid, kast uses the copy built into it and says why."
    )]
    Packages(PackagesArgs),
    /// Set up services (ProtonVPN & Fcitx5 packages, Howdy with the camera's IR emitter, Valkey/SearXNG)
    Services,
    /// Restore app data, KDE Plasma settings & face model (interactive without arguments)
    #[command(visible_alias = "restore")]
    Import(ImportArgs),
    /// Back up app data, KDE Plasma settings & face model (interactive without arguments)
    #[command(visible_alias = "backup")]
    Export(BackupArgs),
    /// List what kast changed (per run) and undo it
    #[command(after_help = "kast records its changes in /var/lib/kast (root only), so this runs through sudo, --list too.\n\
                      Examples:\n  kast undo --list\n  kast undo --last --dry-run\n  kast undo 20261017-101500 --step services\n  kast undo --prune --keep 5")]
    Undo(UndoCli),
    /// Remove apps of kast's package list (checks what still needs them first)
    Uninstall {
        /// Packages, as `kast packages --list` names them
        #[arg(required = true)]
        pkgs: Vec<String>,
        /// Only show what would be removed
        #[arg(long)]
        dry_run: bool,
    },
    /// Proton Drive: sign in or out, and whether it's ready (no root needed)
    #[command(after_help = "Uses Proton's own proton-drive CLI, run as you. Exit status 3: Proton Drive isn't ready\n\
                      (not installed, not signed in, keyring locked, offline).")]
    Drive {
        #[command(subcommand)]
        cmd: DriveCmd,
    },
    /// Back up to Proton Drive on a schedule (a systemd user timer; no root needed)
    #[command(after_help = "The timer runs while you're logged in (your keyring is unlocked then); a run missed while the\n\
                      computer was off happens at your next login. Running apps are left out, never closed; the face\n\
                      model isn't included. Examples:\n  kast schedule enable --targets all --every daily\n  kast schedule status")]
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },
    /// KDE Plasma settings: save, apply, export and import profiles (a Rust port of konsave's logic)
    #[command(after_help = "Works as your own user. Applying snapshots what it overwrites first (pre-apply-<time>), and --dry-run shows the plan.\n\
                      Examples:\n  kast plasma save work\n  kast plasma apply work --only panels,shortcuts --dry-run\n  kast plasma export work -o ~/Downloads/\n  kast plasma config-check")]
    Plasma {
        #[command(subcommand)]
        cmd: plasma::cli::PlasmaCmd,
    },
    /// Repair settings left root-owned or read-only by an older restore ("Configuration file ... not writable")
    #[command(
        visible_alias = "fix-perms",
        after_help = "Checks the KDE and app settings kast manages for files not owned by you or without owner write, lists them\nand fixes them after asking. Never follows symlinks and never leaves your home. Run with sudo when it says\nsomething needs root (chown); --dry-run only lists."
    )]
    FixPermissions {
        /// Only list what would be repaired
        #[arg(long)]
        dry_run: bool,
        /// Don't ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Show detected applications & Howdy status, paths & sizes
    Status,
    /// The IR emitter of a Windows Hello camera: probe it, switch it on for Howdy (no root needed)
    #[command(
        name = "ir-emitter",
        visible_alias = "ir",
        after_help = "Cameras whose IR function has Microsoft's camera extension unit with the face-authentication\n\
                      control (UVC 1.5) qualify; the Services step adds `enable` above Howdy in PAM. Exit status:\n\
                      3 no qualifying camera, 4 the camera's answers rule a write out, 5 a request failed.\n\
                      Examples:\n  kast ir-emitter probe\n  sudo kast ir-emitter run -- howdy -U $USER add"
    )]
    IrEmitter {
        #[command(subcommand)]
        cmd: IrCmd,
    },
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
enum DriveCmd {
    /// Sign in through the browser (prints a link and a QR code; any device works)
    Login,
    /// Sign out and clear proton-drive's session and caches on this computer
    Logout,
    /// Upload an archive made earlier (e.g. one a failed upload kept)
    Upload {
        file: PathBuf,
        /// Backups to keep per target (default: proton.keep in settings, 5)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        keep: Option<u32>,
    },
    /// Whether proton-drive is installed and signed in (exit 3 if not)
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Where proton-drive keeps its session: the desktop keyring, or pass (sign in again after)
    Store {
        #[arg(value_parser = ["keychain", "pass"])]
        store: String,
    },
}

#[derive(Subcommand)]
enum IrCmd {
    /// Show the camera's extension units and face-authentication control (GET requests only)
    Probe {
        /// IR capture node (default: the camera that qualifies)
        #[arg(long, value_name = "PATH")]
        device: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Switch the emitter on for the next stream (what PAM runs before Howdy)
    Enable {
        #[arg(long, value_name = "PATH")]
        device: Option<PathBuf>,
    },
    /// Write the camera's default (GET_DEF) back
    Restore {
        #[arg(long, value_name = "PATH")]
        device: Option<PathBuf>,
    },
    /// Switch the emitter on, then run a command (howdy add / howdy test) and exit with its status
    Run {
        #[arg(long, value_name = "PATH")]
        device: Option<PathBuf>,
        /// The command and its arguments, after --
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<std::ffi::OsString>,
    },
}

/// `kast ir-emitter`: no sudo re-exec, no log, no journal (PAM runs `enable` as the caller,
/// and re-running through sudo would recurse into PAM).
fn run_ir(cmd: IrCmd) -> u8 {
    match cmd {
        IrCmd::Probe { device, json } => ir::cmd_probe(device.as_deref(), json),
        IrCmd::Enable { device } => ir::cmd_enable(device.as_deref()),
        IrCmd::Restore { device } => ir::cmd_restore(device.as_deref()),
        IrCmd::Run { device, command } => ir::cmd_run(device.as_deref(), &command),
    }
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Turn it on (again with no options: the last targets and frequency)
    Enable {
        /// What to back up (several allowed; default: all, without the face model)
        #[arg(long, num_args = 1.., value_parser = schedule::TARGETS)]
        targets: Vec<String>,
        #[arg(long, value_parser = ["daily", "weekly"])]
        every: Option<String>,
        /// Backups kept on Proton Drive per target (default: proton.keep, 5)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        keep: Option<u32>,
    },
    /// Turn it off and remove the timer
    Disable,
    /// On or off, next and last run, what was left out
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Start a scheduled backup now
    Run,
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

const TARGETS: [&str; 24] = [
    "all",
    "beeper",
    "beepertexts",
    "zalo",
    "zalodata",
    "vesktop",
    "zen",
    "helium",
    "helium-browser",
    "obsidian",
    "fcitx5",
    "antigravity",
    "fdm",
    "freedownloadmanager",
    "free-download-manager",
    "fish",
    "fastfetch",
    "scripts",
    "howdy",
    "kde",
    "plasma",
    "kde-plasma",
    "hypr",
    "hyprland",
];

/// Which package list to use.
#[derive(Args, Default, Clone)]
struct ListSource {
    /// Don't download the package list; use the one built into kast
    #[arg(long, conflicts_with = "packages_file")]
    offline: bool,
    /// Use this package list instead of downloading it (to try edits to packages.toml)
    #[arg(long, value_name = "PATH")]
    packages_file: Option<PathBuf>,
}

impl ListSource {
    /// The subcommand's choice, else the one given before it.
    fn or(self, top: ListSource) -> ListSource {
        if self.offline || self.packages_file.is_some() { self } else { top }
    }

    /// The list when it's known without a download: `--offline` or `--packages-file`.
    fn known(&self) -> Result<Option<pkglist::PkgList>> {
        Ok(match &self.packages_file {
            Some(p) => Some(pkglist::load_local_discovering(p)?),
            None if self.offline => Some(pkglist::load_online(true)),
            None => None,
        })
    }

    fn load(&self) -> Result<pkglist::PkgList> {
        Ok(match self.known()? {
            Some(l) => l,
            None => pkglist::load_online(false),
        })
    }
}

#[derive(Args)]
struct AutoArgs {
    /// Don't take a snapshot before the upgrade (by default kast takes one when / is btrfs with
    /// snapper or Timeshift, unless snap-pac already does; without this, a failed snapshot stops
    /// the run before the upgrade)
    #[arg(long)]
    no_snapshot: bool,
    #[command(flatten)]
    source: ListSource,
}

#[derive(Args)]
struct PackagesArgs {
    /// Show the list and where it came from, without installing (no root needed)
    #[arg(long)]
    list: bool,
    /// With --list: print JSON
    #[arg(long, requires = "list")]
    json: bool,
    /// Install only these packages (comma-separated, as --list names them)
    #[arg(long, value_delimiter = ',', value_name = "PKGS", conflicts_with_all = ["skip", "list"])]
    only: Vec<String>,
    /// Install every package except these (comma-separated)
    #[arg(long, value_delimiter = ',', value_name = "PKGS", conflicts_with = "list")]
    skip: Vec<String>,
    #[command(flatten)]
    source: ListSource,
}

#[derive(Args)]
struct UndoCli {
    /// The run to undo (an id from --list; its start is enough)
    run: Option<String>,
    /// List the runs and their changes
    #[arg(long, conflicts_with_all = ["run", "last", "prune"])]
    list: bool,
    /// With --list: print JSON
    #[arg(long, requires = "list")]
    json: bool,
    /// Undo the newest run that still has something to undo
    #[arg(long, conflicts_with_all = ["run", "prune"])]
    last: bool,
    /// Only this step's changes
    #[arg(long, value_parser = ["partition", "packages", "repo", "services", "import", "export", "upgrade", "uninstall"])]
    step: Option<String>,
    /// Show what would be undone, change nothing
    #[arg(long)]
    dry_run: bool,
    /// Also restore files changed since kast wrote them
    #[arg(long)]
    force: bool,
    /// Delete the file copies of old or fully undone runs
    #[arg(long, conflicts_with_all = ["run", "last"])]
    prune: bool,
    /// With --prune: how many runs to keep (default 10, or keep_runs in /var/lib/kast/settings.json)
    #[arg(long, value_name = "N", requires = "prune")]
    keep: Option<usize>,
}

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
    /// What to back up (several: an archive each)
    #[arg(value_parser = TARGETS)]
    target: Vec<String>,
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
    /// Don't start the apps -k (or the prompt) closed again after the backup
    #[arg(long)]
    no_relaunch: bool,
    /// Upload the backup to Proton Drive (sign in first: kast drive login)
    #[arg(long, value_name = "WHERE", value_parser = ["proton"])]
    upload: Option<String>,
    /// Backups to keep on Proton Drive per target (default: proton.keep in settings, 5)
    #[arg(long, value_name = "N", requires = "upload", value_parser = clap::value_parser!(u32).range(1..))]
    keep: Option<u32>,
    /// Also keep the archive in this folder
    #[arg(long, value_name = "DIR", requires = "upload")]
    also_local: Option<PathBuf>,
    /// Upload even when nothing changed since the last upload
    #[arg(long, requires = "upload")]
    force_upload: bool,
    /// The scheduled backup: never prompts, skips running apps and the face model
    #[arg(long, hide = true, requires = "upload")]
    scheduled: bool,
}

impl BackupArgs {
    fn is_empty(&self) -> bool {
        self.target.is_empty()
            && self.output.is_none()
            && !self.no_media
            && !self.no_vaults
            && !self.kill
            && !self.force
            && !self.dry_run
            && !self.no_relaunch
            && self.upload.is_none()
            && self.keep.is_none()
            && self.also_local.is_none()
            && !self.force_upload
            && !self.scheduled
    }

    fn opts_for(&self, target: &str) -> BackupOpts {
        BackupOpts {
            target: target.to_string(),
            output: self.output.clone(),
            include_media: !self.no_media,
            kill: self.kill,
            force: self.force,
            dry_run: self.dry_run,
            no_relaunch: self.no_relaunch,
            ..Default::default()
        }
    }

    fn targets(&self) -> Vec<String> {
        if self.target.is_empty() { vec!["all".into()] } else { self.target.clone() }
    }
}

/// `kast backup`: each target in turn, uploaded with `--upload proton`.
fn run_backup(a: BackupArgs) -> Result<()> {
    if a.target.len() > 1 && a.output.is_some() {
        anyhow::bail!("--output works with a single target");
    }
    if a.scheduled {
        let keep = a.keep.unwrap_or_else(|| settings::load().keep());
        return schedule::run_scheduled(&a.targets(), keep);
    }
    if a.upload.is_some() {
        if a.output.as_deref().is_some_and(|o| !vault::is_dat(o)) {
            anyhow::bail!("--output with --upload only picks the face model's .dat format; use --also-local DIR to keep a copy");
        }
        let s = settings::load();
        let up = drive::UploadOpts {
            keep: a.keep.unwrap_or_else(|| s.keep()),
            also_local: a.also_local.clone().or_else(|| if a.scheduled { s.also_local() } else { None }),
            if_unchanged: if a.force_upload {
                drive::IfUnchanged::Upload
            } else if a.scheduled {
                drive::IfUnchanged::Skip
            } else {
                drive::IfUnchanged::Ask
            },
            retention_ok: a.scheduled,
            name_target: None,
        };
        let opts: Vec<BackupOpts> = a.targets().iter().map(|t| a.opts_for(t)).collect();
        drive::cmd_backup_upload(opts, up)?;
        return Ok(());
    }
    for t in a.targets() {
        recorded_backup(a.opts_for(&t))?;
    }
    Ok(())
}

#[derive(Args, Default)]
struct ImportArgs {
    /// Backup archive (.zip) or standalone face model (.dat)
    file: Option<PathBuf>,
    /// Only restore one app (beeper, zalo, vesktop, zen, helium, obsidian, fcitx5, antigravity, fdm, fish, fastfetch, scripts, howdy, kde, hypr)
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
    /// Don't start the apps -k (or the prompt) closed again after the restore
    #[arg(long)]
    no_relaunch: bool,
}

impl ImportArgs {
    fn is_empty(&self) -> bool {
        self.file.is_none() && self.app.is_none() && !self.no_safety_backup && !self.kill && !self.force && !self.dry_run && !self.no_relaunch
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
            no_relaunch: self.no_relaunch,
        })
    }
}

/// The interactive restore/backup screens (no arguments, on a terminal) run as root like
/// the menu, so sudo asks for the password once, before the full-screen UI starts.
fn interactive_vault(cmd: &Option<Cmd>) -> bool {
    match cmd {
        Some(Cmd::Partition(a)) => a.is_empty() && interactive_terminal(),
        Some(Cmd::Import(a)) => a.is_empty() && interactive_terminal(),
        Some(Cmd::Export(a)) => a.is_empty() && interactive_terminal(),
        _ => false,
    }
}

fn needs_root(cmd: &Option<Cmd>) -> bool {
    match cmd {
        Some(Cmd::Packages(a)) => !a.list,
        Some(Cmd::Partition(a)) => a.needs_root(),
        // The journal is root's: undo and uninstall run through sudo, listing included
        _ => matches!(cmd, None | Some(Cmd::Auto(_) | Cmd::Repo | Cmd::Services | Cmd::Undo(_) | Cmd::Uninstall { .. })) || interactive_vault(cmd),
    }
}

/// Runs as your own user can't write the root-only journal: say so instead.
fn unrecorded_note(dry_run: bool) {
    if !dry_run && !ctx::ctx().is_root {
        println!("Note: not recorded for undo (run with sudo to record it).");
    }
}

fn recorded_import(o: vault::import::ImportOpts) -> Result<()> {
    unrecorded_note(o.dry_run);
    let dry = o.dry_run;
    journal::recorded(&["import"], dry, || cmd_import(o))
}

fn recorded_backup(o: vault::backup::BackupOpts) -> Result<()> {
    unrecorded_note(o.dry_run);
    let dry = o.dry_run;
    journal::recorded(&["export"], dry, || cmd_backup(o))
}

/// Load the list and say which one it is, before anything is installed.
fn announce_list(source: &ListSource) -> Result<pkglist::PkgList> {
    let pl = source.load()?;
    println!("Package list: {}", pl.source_line());
    for w in &pl.list.warnings {
        println!("  warning: {w}");
    }
    Ok(pl)
}

fn dispatch(cmd: Option<Cmd>, top: ListSource) -> Result<()> {
    match cmd {
        None => tui::run(tui::Start::Menu, top.known()?),
        Some(Cmd::Auto(a)) => {
            let det = snapshot::detect();
            for w in det.warning() {
                println!("{w}");
            }
            let snap = !a.no_snapshot && det.default_on();
            if det.offered() {
                println!(
                    "{}",
                    if snap { format!("Taking a {} snapshot before the upgrade.", det.tool_name()) } else { "No snapshot before the upgrade.".into() }
                );
            }
            setup::run_automatic(&announce_list(&a.source.or(top))?, snap)
        }
        Some(Cmd::Partition(a)) if a.is_empty() && interactive_terminal() => tui::run(tui::Start::Partition, top.known()?),
        Some(Cmd::Partition(a)) => partition::cli::run(a),
        Some(Cmd::Repo) => journal::recorded(&["repo"], false, || setup::add_kud3n_repo(false, 0, 1, 1)),
        Some(Cmd::Packages(a)) if a.list => {
            // A fallback is a valid answer: it's shown with its reason, and the exit status is 0
            let pl = a.source.or(top).load()?;
            let installed: Option<std::collections::HashSet<String>> = ctx::output("pacman", &["-Qq"]).map(|o| o.lines().map(str::to_string).collect());
            if a.json {
                println!("{}", serde_json::to_string_pretty(&pkglist::list_json(&pl, installed.as_ref()))?);
            } else {
                print!("{}", pkglist::list_text(&pl, installed.as_ref()));
            }
            Ok(())
        }
        Some(Cmd::Packages(a)) => {
            let pl = announce_list(&a.source.or(top))?;
            let list = setup::installable(&pl.list, setup::repo_configured());
            if list.count() < pl.list.count() {
                println!("{} packages from archrepo are left out: [archrepo] isn't in pacman.conf (run kast repo first)", pl.list.count() - list.count());
            }
            let selection = list.select(&a.only, &a.skip).map_err(|e| anyhow::anyhow!(e))?;
            if let Some(s) = &selection {
                println!("Selected: {} of {} packages", s.len(), list.count());
            }
            journal::recorded(&["packages"], false, || setup::install_packages(&list, selection.as_deref(), false, 0, 1, 1));
            // The dashboard drew over the line above
            println!("Package list: {}", pl.source_line());
            Ok(())
        }
        Some(Cmd::Services) => {
            journal::recorded(&["services"], false, || setup::setup_services(false, 0, 1, 1))?;
            // The dashboard drew over the log lines
            setup::services_footer();
            Ok(())
        }
        Some(Cmd::Import(a)) if a.is_empty() && interactive_terminal() => tui::run(tui::Start::Import, top.known()?),
        Some(Cmd::Import(a)) if a.is_empty() => journal::recorded(&["import"], false, interactive::import_data),
        Some(Cmd::Import(a)) => recorded_import(a.into_opts()?),
        Some(Cmd::Export(a)) if a.is_empty() && interactive_terminal() => tui::run(tui::Start::Export, top.known()?),
        Some(Cmd::Export(a)) if a.is_empty() => journal::recorded(&["export"], false, interactive::export_data),
        Some(Cmd::Export(a)) => run_backup(a),
        Some(Cmd::Drive { cmd }) => match cmd {
            DriveCmd::Login => drive::cmd_login(),
            DriveCmd::Logout => drive::cmd_logout(),
            DriveCmd::Upload { file, keep } => drive::cmd_upload_file(&backup::absolutize(&file), keep),
            DriveCmd::Status { json } => drive::cmd_status(json),
            DriveCmd::Store { store } => drive::cmd_store(&store),
        },
        Some(Cmd::Schedule { cmd }) => match cmd {
            ScheduleCmd::Enable { targets, every, keep } => schedule::cmd_enable(targets, every, keep),
            ScheduleCmd::Disable => schedule::cmd_disable(),
            ScheduleCmd::Status { json } => schedule::cmd_status(json),
            ScheduleCmd::Run => schedule::cmd_run(),
        },
        Some(Cmd::Status) | Some(Cmd::Vault { cmd: VaultCmd::Status }) => {
            vault::status::cmd_status();
            Ok(())
        }
        Some(Cmd::Info { file }) | Some(Cmd::Vault { cmd: VaultCmd::Info { file } }) => {
            vault::require_vault_tools()?;
            vault::info::cmd_info(file.as_deref())
        }
        Some(Cmd::Vault { cmd: VaultCmd::Backup(a) }) => run_backup(a),
        Some(Cmd::Vault { cmd: VaultCmd::Import(a) }) => recorded_import(a.into_opts()?),
        Some(Cmd::Log(a)) => logio::os::cmd_log(a.tail, a.open, a.copy),
        Some(Cmd::Undo(a)) => undo::cmd_undo(&undo::UndoArgs {
            list: a.list,
            json: a.json,
            run: a.run.as_deref(),
            last: a.last,
            step: a.step.as_deref(),
            dry_run: a.dry_run,
            force: a.force,
            prune: a.prune,
            keep: a.keep,
        }),
        Some(Cmd::Uninstall { pkgs, dry_run }) => undo::cmd_uninstall(&pkgs, dry_run),
        Some(Cmd::Plasma { cmd }) => plasma::cli::run(cmd),
        Some(Cmd::FixPermissions { dry_run, yes }) => vault::fix::cmd_fix_permissions(dry_run, yes),
        Some(Cmd::IrEmitter { cmd }) => std::process::exit(run_ir(cmd).into()),
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
            ctx::cleanup_on_exit();
            tui::restore_terminal();
            if ctx::stale_pacman_lock(interrupted, std::time::Duration::from_secs(2)) {
                eprintln!("pacman was stopped mid-run and left {0} behind. No pacman is running; remove it with: sudo rm {0}", ctx::PACMAN_LOCK);
            }
            std::process::exit(if sig == SIGINT { 130 } else { 143 });
        }
    });
}

/// Variables the sudo re-exec keeps.
const CARRIED_ENV: [&str; 16] = [
    "NO_COLOR",
    "KAST_PAGER",
    "PAGER",
    "VISUAL",
    "EDITOR",
    "TMUX",
    "STY",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "no_proxy",
    "NO_PROXY",
];

/// The full-screen UI needs a terminal on both ends.
fn interactive_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();

    // Before anything that needs a terminal, root, the log or the journal
    cli.cmd = match cli.cmd {
        Some(Cmd::IrEmitter { cmd }) => return ExitCode::from(run_ir(cmd)),
        c => c,
    };

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
        // pager/editor for `e`/`E`, the session/multiplexer for opening and copying the log, and
        // the proxy for downloading the package list
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

    let result = panic::catch_unwind(AssertUnwindSafe(|| dispatch(cli.cmd, cli.list)));
    ctx::cleanup_on_exit();
    tui::restore_terminal();
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) if e.is::<drive::NotReady>() => {
            eprintln!("{C_RED}[ERROR]{C_RESET} {e}");
            ExitCode::from(drive::EXIT_NOT_READY)
        }
        Ok(Err(e)) => {
            if !e.is::<Reported>() {
                eprintln!("{C_RED}[ERROR]{C_RESET} {e:#}");
            }
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}
