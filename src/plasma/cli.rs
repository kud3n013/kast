//! `kast plasma ...`: the command line over the module. Works as your own user, no sudo.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Subcommand;

use super::apply;
use super::archive::{self, ExportOpts, ImportOpts};
use super::check;
use super::config::{Category, Config, Source};
use super::deps::{self, Pacman, PkgOwner};
use super::fsops::human_size;
use super::paths::Roots;
use super::profile::{self, Ambient, ProfileKind, SaveOpts};
use super::reload;

#[derive(Subcommand)]
pub enum PlasmaCmd {
    /// List saved profiles and safety snapshots
    List,
    /// Save the current Plasma settings as a profile
    Save {
        name: String,
        /// Replace a profile of that name (it is rebuilt, not merged)
        #[arg(long)]
        force: bool,
        /// Also store the large assets: themes, icons, plasmoids, wallpapers (can be big)
        #[arg(long)]
        assets: bool,
        /// Only these categories, e.g. panels,shortcuts
        #[arg(long)]
        only: Option<String>,
    },
    /// Write a profile back. Overwritten files are snapshotted first (pre-apply-<time>)
    #[command(after_help = "Categories: appearance, shortcuts, panels, windows, input, apps, gtk, autostart.\n\
                      Plasma rewrites its settings when it quits: use --restart-plasma (quit, apply, start) or log out and in.")]
    Apply {
        name: String,
        /// Only these categories, e.g. panels,shortcuts
        #[arg(long)]
        only: Option<String>,
        /// Show what would be created, overwritten or skipped, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Quit plasmashell before applying and start it again afterwards
        #[arg(long)]
        restart_plasma: bool,
    },
    /// Remove a profile
    Remove {
        name: String,
        /// Don't ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Export a profile as a .kast-plasma.tar.gz
    Export {
        name: String,
        /// Output file or folder (default: the current folder)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
        /// Leave out the large assets that are not in the profile
        #[arg(long)]
        no_assets: bool,
    },
    /// Import an exported profile (only creates a profile; `apply` writes it)
    Import {
        file: PathBuf,
        /// Profile name (default: from the file name)
        #[arg(long)]
        name: Option<String>,
        /// Replace an existing profile of that name
        #[arg(long)]
        force: bool,
        /// Check the archive and show what would be created; write nothing
        #[arg(long)]
        dry_run: bool,
        /// Install missing packages the profile needs (asks pacman, needs sudo)
        #[arg(long)]
        install_deps: bool,
    },
    /// Compare the list with ~/.config and ~/.local/share/plasma
    ConfigCheck {
        /// Also list tracked and app-data entries
        #[arg(long)]
        all: bool,
    },
    /// Packages a profile needs (themes, icon themes, plasmoids) and which are missing
    Deps {
        name: String,
        /// Install the missing ones with pacman
        #[arg(long)]
        install: bool,
    },
    /// Print the built-in list (a starting point for ~/.config/kast/plasma.toml)
    DefaultConfig,
}

fn categories(only: &Option<String>) -> Result<Option<Vec<Category>>> {
    only.as_deref().map(Category::parse_list).transpose()
}

fn load_config(roots: &Roots) -> Result<Config> {
    let (cfg, src) = Config::load(roots)?;
    if let Source::User(p) = src {
        println!("Using your list: {}", p.display());
    }
    Ok(cfg)
}

pub fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// `pacman -S --needed` for these packages, asking as pacman does (through sudo when not root).
pub fn install_packages(pkgs: &[String]) -> Result<()> {
    let mut cmd = if crate::ctx::ctx().is_root {
        std::process::Command::new("pacman")
    } else {
        let mut c = std::process::Command::new("sudo");
        c.arg("pacman");
        c
    };
    if !cmd.arg("-S").arg("--needed").args(pkgs).status().is_ok_and(|s| s.success()) {
        bail!("pacman could not install {}", pkgs.join(" "));
    }
    Ok(())
}

fn print_deps(p: &profile::Profile, install: bool) -> Result<()> {
    let pk = Pacman;
    if p.meta.requires.is_empty() {
        println!("{}: no system themes or plasmoids recorded.", p.name);
        return Ok(());
    }
    let missing = deps::missing(&p.meta.requires, &pk);
    for r in &p.meta.requires {
        let state = if missing.contains(&r.package) { "missing" } else { "installed" };
        println!("  {:<14} {:<28} {} ({state})", r.kind, r.name, r.package);
    }
    if missing.is_empty() {
        return Ok(());
    }
    let (ok, no) = deps::split_installable(&missing, &pk);
    if !no.is_empty() {
        println!("Not in any enabled repository (AUR?): {}", no.join(" "));
    }
    if ok.is_empty() {
        return Ok(());
    }
    if install {
        install_packages(&ok)
    } else {
        println!("Install the rest with: kast plasma deps {} --install   (or: sudo pacman -S --needed {})", p.name, ok.join(" "));
        Ok(())
    }
}

pub fn run(cmd: PlasmaCmd) -> Result<()> {
    let roots = Roots::from_ctx();
    match cmd {
        PlasmaCmd::List => {
            let l = profile::list(&roots)?;
            if l.is_empty() {
                println!("No profiles yet. Save one with: kast plasma save <name>");
            }
            for s in l {
                let kind = match s.meta.as_ref().map(|m| m.kind) {
                    Some(ProfileKind::Snapshot) => "snapshot",
                    Some(ProfileKind::Profile) => "profile",
                    None => "unreadable",
                };
                let when = s.meta.as_ref().map(|m| m.created.as_str()).unwrap_or("");
                let cats: Vec<&str> = s.categories.iter().map(|c| c.name()).collect();
                println!("{:<28} {:<9} {:<26} {:>5} files {:>9}  {}", s.name, kind, when, s.files, human_size(s.bytes), cats.join(","));
            }
        }
        PlasmaCmd::Save { name, force, assets, only } => {
            let cfg = load_config(&roots)?;
            let only = categories(&only)?;
            let rep = profile::save(&roots, &cfg, &name, SaveOpts { force, assets, only: only.as_deref() }, &Ambient::detect())?;
            for w in &rep.warnings {
                println!("Warning: {w}");
            }
            println!("Saved {:?}: {} files, {} ({} listed entries not present here).", rep.name, rep.files, human_size(rep.bytes), rep.missing.len());
            if !rep.requires.is_empty() {
                println!("Needs {} system package(s): see `kast plasma deps {}`.", rep.requires.len(), rep.name);
            }
            if !assets {
                println!("Themes, icons and plasmoids are not stored (add --assets), but `export` adds them to the archive.");
            }
        }
        PlasmaCmd::Apply { name, only, dry_run, restart_plasma } => {
            let cfg = load_config(&roots)?;
            let only = categories(&only)?;
            let p = profile::load(&roots, &name)?;
            let plan = apply::plan(&roots, &cfg, &p, only.as_deref())?;
            println!("{}", plan.render());
            if dry_run {
                if restart_plasma {
                    println!("(would quit plasmashell before and start it after)");
                }
                println!("Dry run: nothing was changed.");
                return Ok(());
            }
            if !plan.has_changes() {
                println!("Nothing to do.");
                return Ok(());
            }
            let amb = Ambient::detect();
            let tools = restart_plasma.then(|| reload::detect_tools(amb.plasma_version.as_deref(), &crate::ctx::have)).flatten();
            if restart_plasma && tools.is_none() {
                println!("Warning: kquitapp/kstart not found; applying without restarting Plasma.");
            }
            if let Some(t) = &tools {
                println!("Quitting plasmashell...");
                reload::quit_shell(t).map_err(anyhow::Error::msg)?;
            }
            let rep = apply::execute(&roots, &plan, true, &amb);
            if let Some(t) = &tools {
                println!("Starting plasmashell...");
                if let Err(e) = reload::start_shell(t) {
                    println!("Warning: {e}");
                }
            }
            let rep = rep?;
            println!("Applied: {} created, {} overwritten, {} unchanged, {} skipped.", rep.created, rep.overwritten, rep.unchanged, rep.skipped);
            if let Some(s) = &rep.snapshot {
                println!("Undo with: kast plasma apply {s}");
            }
            for f in &rep.failed {
                println!("Failed: {f}");
            }
            if tools.is_none() {
                println!("{}", reload::WARNING);
            }
            if !rep.failed.is_empty() {
                bail!("{} file(s) could not be written", rep.failed.len());
            }
        }
        PlasmaCmd::Remove { name, yes } => {
            profile::load(&roots, &name)
                .map(|_| ())
                .or_else(|e| if profile::profile_dir(&roots, &name).is_ok_and(|d| d.is_dir()) { Ok(()) } else { Err(e) })?;
            if !yes && !confirm(&format!("Remove profile {name:?}?")) {
                bail!("not removed (use --yes to skip the question)");
            }
            profile::remove(&roots, &name)?;
            println!("Removed {name:?}.");
        }
        PlasmaCmd::Export { name, output, force, no_assets } => {
            let cfg = load_config(&roots)?;
            let out = output.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let rep = archive::export(&roots, &cfg, &name, &out, ExportOpts { force, assets: !no_assets }, &Ambient::detect())?;
            for w in &rep.warnings {
                println!("Warning: {w}");
            }
            println!("Exported to {} ({}, {} asset files added from the live folders).", rep.path.display(), human_size(rep.bytes), rep.assets_added);
        }
        PlasmaCmd::Import { file, name, force, dry_run, install_deps } => {
            let rep = archive::import(&roots, &file, &ImportOpts { name, force, dry_run })?;
            let s = rep.stats;
            let verb = if dry_run { "Would create" } else { "Created" };
            println!(
                "{verb} profile {:?}{}: {} files, {} links, {}.",
                rep.name,
                if rep.replaces { " (replacing the existing one)" } else { "" },
                s.files,
                s.links,
                human_size(s.bytes)
            );
            println!("From {} (Plasma {}, kast {}).", rep.meta.hostname, rep.meta.plasma_version.as_deref().unwrap_or("?"), rep.meta.kast_version);
            if dry_run {
                println!("Dry run: nothing was written. The archive passed every safety check.");
                return Ok(());
            }
            println!("Nothing was applied yet. Preview with: kast plasma apply {} --dry-run", rep.name);
            let p = profile::load(&roots, &rep.name)?;
            print_deps(&p, install_deps)?;
        }
        PlasmaCmd::ConfigCheck { all } => {
            let cfg = load_config(&roots)?;
            print!("{}", check::render(&check::check(&roots, &cfg)?, all));
        }
        PlasmaCmd::Deps { name, install } => {
            let p = profile::load(&roots, &name)?;
            print_deps(&p, install)?;
        }
        PlasmaCmd::DefaultConfig => print!("{}", Config::default_text()),
    }
    let _: &dyn PkgOwner = &Pacman;
    Ok(())
}
