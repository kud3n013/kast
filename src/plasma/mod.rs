// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! KDE Plasma settings: save, apply, export and import of profiles. A Rust port of the
//! logic of konsave (see `docs/konsave-notes.md` for what was taken and what differs).
//!
//! Public API, used by the CLI (`cli`) and the TUI screen:
//! - [`profile::save`], [`profile::list`], [`profile::remove`], [`profile::load`]
//! - [`apply::plan`] then [`apply::execute`] (a dry run is just the plan)
//! - [`archive::export`], [`archive::import`]
//! - [`check::check`], [`deps`], [`reload`]
//!
//! Everything takes a [`paths::Roots`], so tests use a temporary home.

pub mod apply;
pub mod archive;
pub mod check;
pub mod cli;
pub mod config;
pub mod deps;
pub mod fsops;
pub mod paths;
pub mod profile;
pub mod reload;
