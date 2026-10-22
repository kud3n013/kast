// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! The list of what a profile contains (konsave's `conf.yaml`, as TOML): sections with a
//! location, entries, a kind and a category.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::paths::{Roots, safe_rel};

const DEFAULT: &str = include_str!("default.toml");

/// Groups that can be applied on their own (`apply --only panels,shortcuts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Appearance,
    Shortcuts,
    Panels,
    Windows,
    Input,
    Apps,
    Gtk,
    Autostart,
}

impl Category {
    pub const ALL: [Category; 8] =
        [Category::Appearance, Category::Shortcuts, Category::Panels, Category::Windows, Category::Input, Category::Apps, Category::Gtk, Category::Autostart];

    pub fn name(self) -> &'static str {
        match self {
            Category::Appearance => "appearance",
            Category::Shortcuts => "shortcuts",
            Category::Panels => "panels",
            Category::Windows => "windows",
            Category::Input => "input",
            Category::Apps => "apps",
            Category::Gtk => "gtk",
            Category::Autostart => "autostart",
        }
    }

    /// What the category covers, for the TUI and `--help`.
    pub fn describe(self) -> &'static str {
        match self {
            Category::Appearance => "themes, colors, icons, fonts, splash, lock screen",
            Category::Shortcuts => "global and per-app shortcuts, their launchers and commands",
            Category::Panels => "panels, widgets, desktop layout, plasmoids",
            Category::Windows => "KWin, window rules, screens, session, KWin scripts",
            Category::Input => "mouse, touchpad, keyboard layout",
            Category::Apps => "Dolphin, Konsole, Kate, Klipper, KRunner, ...",
            Category::Gtk => "GTK 2/3/4 settings",
            Category::Autostart => "autostart entries",
        }
    }

    /// `panels,shortcuts` -> [Panels, Shortcuts]; an unknown word is an error.
    pub fn parse_list(s: &str) -> Result<Vec<Category>> {
        let mut out = Vec::new();
        for w in s.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            let c = Category::ALL
                .iter()
                .find(|c| c.name() == w.to_ascii_lowercase())
                .with_context(|| format!("unknown category {w:?} (use: {})", Category::ALL.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")))?;
            if !out.contains(c) {
                out.push(*c);
            }
        }
        if out.is_empty() {
            bail!("no category given");
        }
        Ok(out)
    }
}

/// `save` sections hold small settings; `export` sections large assets (themes, plasmoids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Save,
    Export,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub category: Category,
    #[serde(default)]
    pub kind: Kind,
    pub location: String,
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default, rename = "section")]
    pub sections: Vec<Section>,
}

fn one() -> u32 {
    1
}

/// Where the effective list came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    BuiltIn,
    User(PathBuf),
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let c: Config = toml::from_str(text).context("reading the Plasma list")?;
        c.validate()?;
        Ok(c)
    }

    pub fn built_in() -> Config {
        Config::parse(DEFAULT).expect("built-in Plasma list is valid")
    }

    pub fn default_text() -> &'static str {
        DEFAULT
    }

    /// The user's `~/.config/kast/plasma.toml` when there is one, else the built-in list.
    pub fn load(roots: &Roots) -> Result<(Config, Source)> {
        let path = roots.user_config_file();
        match fs::read_to_string(&path) {
            Ok(text) => Ok((Config::parse(&text).with_context(|| path.display().to_string())?, Source::User(path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::built_in(), Source::BuiltIn)),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported list version {} (this kast reads version 1)", self.version);
        }
        let mut seen = HashSet::new();
        for s in &self.sections {
            if s.id.is_empty() || !s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                bail!("section id {:?}: use lowercase letters, digits and dashes", s.id);
            }
            if !seen.insert(&s.id) {
                bail!("section id {:?} is used twice", s.id);
            }
            if s.location.trim().is_empty() {
                bail!("section {}: empty location", s.id);
            }
            for e in &s.entries {
                safe_rel(e).with_context(|| format!("section {}", s.id))?;
            }
        }
        Ok(())
    }

    pub fn section(&self, id: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_list_is_valid_and_complete() {
        let c = Config::built_in();
        for cat in Category::ALL {
            assert!(c.sections.iter().any(|s| s.category == cat), "no section for {}", cat.name());
        }
        assert!(c.sections.iter().any(|s| s.kind == Kind::Export));
        let saved = |e: &str| c.sections.iter().any(|s| s.kind == Kind::Save && s.entries.iter().any(|x| x == e));
        assert!(saved("konsole") && saved("konsolesshconfig"), "Konsole profiles are settings, not assets");
        let all: Vec<&str> = c.sections.iter().flat_map(|s| s.entries.iter().map(String::as_str)).collect();
        for must in ["kdeglobals", "kglobalshortcutsrc", "applications", "kxmlgui5", "kwin/scripts", "plasma-org.kde.plasma.desktop-appletsrc", "kwinrc", "plasma/plasmoids", "color-schemes", "konsole"] {
            assert!(all.contains(&must), "{must}");
        }
    }

    #[test]
    fn categories_parse_and_reject_unknown() {
        assert_eq!(Category::parse_list("panels, Shortcuts,panels").unwrap(), vec![Category::Panels, Category::Shortcuts]);
        assert!(Category::parse_list("panels,nope").is_err());
        assert!(Category::parse_list(" , ").is_err());
    }

    #[test]
    fn bad_lists_are_rejected() {
        let dup = "[[section]]\nid='a'\ncategory='apps'\nlocation='$HOME'\nentries=[]\n[[section]]\nid='a'\ncategory='apps'\nlocation='$HOME'\nentries=[]\n";
        assert!(Config::parse(dup).is_err());
        let up = "[[section]]\nid='a'\ncategory='apps'\nlocation='$HOME'\nentries=['../x']\n";
        assert!(Config::parse(up).is_err());
        let abs = "[[section]]\nid='a'\ncategory='apps'\nlocation='$HOME'\nentries=['/etc/passwd']\n";
        assert!(Config::parse(abs).is_err());
        let cat = "[[section]]\nid='a'\ncategory='bogus'\nlocation='$HOME'\nentries=[]\n";
        assert!(Config::parse(cat).is_err());
        assert!(Config::parse("version = 2").is_err());
    }

    #[test]
    fn user_file_replaces_the_built_in_list() {
        let d = tempfile::tempdir().unwrap();
        let r = Roots::under_home(d.path());
        assert_eq!(Config::load(&r).unwrap().1, Source::BuiltIn);
        fs::create_dir_all(r.config.join("kast")).unwrap();
        fs::write(r.user_config_file(), "[[section]]\nid='mine'\ncategory='apps'\nlocation='$HOME'\nentries=['x']\n").unwrap();
        let (c, src) = Config::load(&r).unwrap();
        assert!(matches!(src, Source::User(_)));
        assert_eq!(c.sections.len(), 1);
        fs::write(r.user_config_file(), "not toml [").unwrap();
        assert!(Config::load(&r).is_err());
    }
}
