// Based on konsave (https://github.com/Prayag2/konsave) by Prayag2 and
// contributors, and on the urban-1 fork (https://github.com/urban-1/konsave).
// Licensed under GPL-3.0. Translated to Rust and modified for kast, 2026.
// Studied versions: Prayag2/konsave abdb6d3, urban-1/konsave f07715e.

//! Where things live, placeholder expansion and the path rules everything else relies on.
//!
//! Every function that touches the user's files takes a [`Roots`], so tests point it at a
//! temporary directory and never at the real `~/.config`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// The folders the module works in. Only `from_ctx` reads the process context.
#[derive(Debug, Clone)]
pub struct Roots {
    pub home: PathBuf,
    pub config: PathBuf,
    pub share: PathBuf,
    pub bin: PathBuf,
    /// Where profiles are stored (one folder each)
    pub profiles: PathBuf,
    /// Owner for everything created when kast runs as root on behalf of a user
    pub owner: Option<(u32, u32)>,
}

impl Roots {
    /// The conventional layout below `home` (tests).
    #[cfg(test)]
    pub fn under_home(home: &Path) -> Self {
        Roots {
            home: home.to_path_buf(),
            config: home.join(".config"),
            share: home.join(".local/share"),
            bin: home.join(".local/bin"),
            profiles: home.join(".local/share/kast/plasma/profiles"),
            owner: None,
        }
    }

    /// The target user's real folders.
    pub fn from_ctx() -> Self {
        let c = crate::ctx::ctx();
        Roots {
            home: c.target_home.clone(),
            config: c.config_home.clone(),
            share: c.data_home.clone(),
            bin: c.target_home.join(".local/bin"),
            profiles: c.data_home.join("kast/plasma/profiles"),
            owner: (c.is_root && c.target_uid != 0).then_some((c.target_uid, c.target_gid)),
        }
    }

    /// The user's own list of entries, replacing the built-in one when present.
    pub fn user_config_file(&self) -> PathBuf {
        self.config.join("kast/plasma.toml")
    }

    fn allowed(&self, p: &Path) -> bool {
        [&self.home, &self.config, &self.share, &self.bin].iter().any(|r| p.starts_with(r))
    }
}

/// A profile name is a plain file name: no separators, no leading dot (hidden folders hold
/// staging areas), at most 64 characters.
pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A relative path made of plain components only (no `..`, no root, no `.`).
pub fn safe_rel(p: &str) -> Result<PathBuf> {
    if p.is_empty() {
        bail!("empty path");
    }
    let path = Path::new(p);
    for c in path.components() {
        match c {
            Component::Normal(_) => {}
            _ => bail!("unsafe path {p:?}: only plain relative components are allowed"),
        }
    }
    Ok(path.to_path_buf())
}

/// `*` (any run of characters) and `?` (one character) in a single file name. Like a shell,
/// a leading dot is only matched by a pattern that starts with one.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn has_glob(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// Does this config entry (a relative path, wildcards allowed in the last component) cover
/// `rel`, or one of the folders above it? This is the rule that keeps an applied profile
/// inside the entries of the local config.
pub fn entry_covers(entry: &str, rel: &Path) -> bool {
    let pat: Vec<&str> = entry.split('/').collect();
    let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if pat.is_empty() || comps.len() < pat.len() {
        return false;
    }
    pat.iter().enumerate().all(|(i, p)| if i + 1 == pat.len() { glob_match(p, &comps[i]) } else { *p == comps[i] })
}

/// The concrete relative paths below `location` an entry stands for that exist. A literal
/// entry gives itself, a wildcard in the last component gives every match (sorted).
pub fn resolve_entry(location: &Path, entry: &str) -> Result<Vec<PathBuf>> {
    let rel = safe_rel(entry)?;
    let comps: Vec<&str> = entry.split('/').collect();
    if comps[..comps.len() - 1].iter().any(|c| has_glob(c)) {
        bail!("{entry:?}: wildcards are only allowed in the last path component");
    }
    if !has_glob(entry) {
        return Ok(if location.join(&rel).symlink_metadata().is_ok() { vec![rel] } else { vec![] });
    }
    let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    let last = comps[comps.len() - 1];
    let mut names: Vec<String> = match fs::read_dir(location.join(&parent)) {
        Ok(rd) => rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| glob_match(last, n)).collect(),
        Err(_) => vec![],
    };
    names.sort();
    Ok(names.into_iter().map(|n| parent.join(n)).collect())
}

/// A location after expansion, with anything worth telling the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Expanded {
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// Expand `$HOME`, `$CONFIG_DIR`, `$SHARE_DIR`, `$BIN_DIR` and `${ENDS_WITH="x"}` /
/// `${BEGINS_WITH="x"}` in a section's location.
///
/// Unlike konsave: an unknown placeholder is an error (`$HOMEDIR` is not `$HOME` plus
/// "DIR"), the ENDS_WITH/BEGINS_WITH folders are sorted so the pick is the same every time
/// (several matches warn), no match is an error instead of a bogus path, and the result must
/// lie inside the user's folders.
pub fn expand_location(template: &str, roots: &Roots) -> Result<Expanded> {
    let mut out = String::new();
    let mut warnings = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(body) = rest.strip_prefix('{') {
            let Some(end) = body.find('}') else { bail!("{template:?}: unclosed ${{") };
            let (name, arg) = parse_function(&body[..end]).ok_or_else(|| anyhow::anyhow!("{template:?}: expected ${{NAME=\"text\"}}"))?;
            rest = &body[end + 1..];
            if !(out.ends_with('/') && (rest.is_empty() || rest.starts_with('/'))) {
                bail!("{template:?}: ${{{name}=...}} must be a whole path component");
            }
            let mut matches: Vec<String> = fs::read_dir(&out)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| if name == "ENDS_WITH" { n.ends_with(&arg) } else { n.starts_with(&arg) })
                        .collect()
                })
                .unwrap_or_default();
            matches.sort();
            let Some(first) = matches.first() else {
                bail!("{template:?}: no folder in {out} {} {arg:?}", if name == "ENDS_WITH" { "ends with" } else { "begins with" })
            };
            if matches.len() > 1 {
                warnings.push(format!("{template}: {} folders match {arg:?} ({}); using {first}", matches.len(), matches.join(", ")));
            }
            out.push_str(first);
        } else {
            let n = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
            let value = match &rest[..n] {
                "HOME" => &roots.home,
                "CONFIG_DIR" => &roots.config,
                "SHARE_DIR" => &roots.share,
                "BIN_DIR" => &roots.bin,
                other => bail!("{template:?}: unknown placeholder ${other}"),
            };
            out.push_str(&value.to_string_lossy());
            rest = &rest[n..];
        }
    }
    out.push_str(rest);
    let path = PathBuf::from(&out);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        bail!("{template:?}: expands to {out:?}, which is not a clean absolute path");
    }
    if !roots.allowed(&path) {
        bail!("{template:?}: expands to {out:?}, outside your home folder");
    }
    Ok(Expanded { path, warnings })
}

fn parse_function(inner: &str) -> Option<(String, String)> {
    let (name, value) = inner.split_once('=')?;
    if !matches!(name, "ENDS_WITH" | "BEGINS_WITH") {
        return None;
    }
    let q = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let arg = value.strip_prefix(q)?.strip_suffix(q)?;
    (!arg.is_empty() && !arg.contains('/')).then(|| (name.to_string(), arg.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (tempfile::TempDir, Roots) {
        let d = tempfile::tempdir().unwrap();
        let r = Roots::under_home(d.path());
        (d, r)
    }

    #[test]
    fn expands_the_four_keywords() {
        let (d, r) = roots();
        let h = d.path().display();
        assert_eq!(expand_location("$HOME/x", &r).unwrap().path, PathBuf::from(format!("{h}/x")));
        assert_eq!(expand_location("$CONFIG_DIR", &r).unwrap().path, PathBuf::from(format!("{h}/.config")));
        assert_eq!(expand_location("$SHARE_DIR/plasma", &r).unwrap().path, PathBuf::from(format!("{h}/.local/share/plasma")));
        assert_eq!(expand_location("$BIN_DIR", &r).unwrap().path, PathBuf::from(format!("{h}/.local/bin")));
    }

    #[test]
    fn unknown_or_longer_placeholder_is_an_error_not_home_plus_text() {
        let (_d, r) = roots();
        assert!(expand_location("$HOMEDIR/x", &r).is_err());
        assert!(expand_location("$NOPE", &r).is_err());
    }

    #[test]
    fn locations_must_stay_in_the_user_folders() {
        let (_d, r) = roots();
        assert!(expand_location("/etc", &r).is_err());
        assert!(expand_location("$HOME/../other", &r).is_err());
        assert!(expand_location("/home/someone-else/.config", &r).is_err());
        assert!(expand_location(".config", &r).is_err());
    }

    #[test]
    fn ends_with_and_begins_with_pick_sorted_and_warn_on_several() {
        let (d, r) = roots();
        fs::create_dir_all(d.path().join("ff/b.default-release")).unwrap();
        fs::create_dir_all(d.path().join("ff/a.default-release")).unwrap();
        fs::create_dir_all(d.path().join("ff/profiles.ini.d")).unwrap();
        let e = expand_location("$HOME/ff/${ENDS_WITH=\".default-release\"}/chrome", &r).unwrap();
        assert!(e.path.ends_with("ff/a.default-release/chrome"));
        assert_eq!(e.warnings.len(), 1);
        let e = expand_location("$HOME/ff/${BEGINS_WITH='profiles'}", &r).unwrap();
        assert!(e.path.ends_with("ff/profiles.ini.d") && e.warnings.is_empty());
    }

    #[test]
    fn function_without_a_match_or_not_a_whole_component_fails() {
        let (d, r) = roots();
        fs::create_dir_all(d.path().join("ff")).unwrap();
        assert!(expand_location("$HOME/ff/${ENDS_WITH='zzz'}", &r).is_err());
        assert!(expand_location("$HOME/ff/x${ENDS_WITH='zzz'}", &r).is_err());
        assert!(expand_location("$HOME/ff/${NOPE='zzz'}", &r).is_err());
    }

    #[test]
    fn glob_rules() {
        assert!(glob_match("plasma*rc", "plasmarc"));
        assert!(glob_match("plasma*rc", "plasma-org.kde.plasma.desktop-appletsrc"));
        assert!(!glob_match("plasma*rc", "plasmashellrc.bak"));
        assert!(!glob_match("k?inrc", "kwwinrc") && glob_match("k?inrc", "kwinrc"));
        assert!(!glob_match("*", ".hidden"));
        assert!(glob_match(".*", ".hidden"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn safe_rel_rejects_traversal_and_absolute() {
        assert!(safe_rel("a/b").is_ok());
        for bad in ["", "/etc", "../x", "a/../b", "./a", "a//.."] {
            assert!(safe_rel(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn profile_names() {
        for ok in ["work", "my-setup_2", "v1.2"] {
            assert!(valid_profile_name(ok));
        }
        for bad in ["", ".hidden", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(!valid_profile_name(bad), "{bad}");
        }
    }

    #[test]
    fn entry_covers_matches_parents_and_globs() {
        assert!(entry_covers("gtk-3.0", Path::new("gtk-3.0/settings.ini")));
        assert!(entry_covers("plasma/plasmoids", Path::new("plasma/plasmoids/x/main.qml")));
        assert!(entry_covers("plasma*rc", Path::new("plasma-foorc")));
        assert!(!entry_covers("kdeglobals", Path::new(".ssh/authorized_keys")));
        assert!(!entry_covers("plasma/plasmoids", Path::new("plasma")));
        assert!(!entry_covers("plasma/plasmoids", Path::new("plasma/other")));
    }

    #[test]
    fn resolve_entry_literal_and_wildcard() {
        let (d, _r) = roots();
        let loc = d.path();
        fs::write(loc.join("plasmarc"), "").unwrap();
        fs::write(loc.join("plasmashellrc"), "").unwrap();
        fs::create_dir_all(loc.join("sub")).unwrap();
        fs::write(loc.join("sub/a.conf"), "").unwrap();
        assert_eq!(resolve_entry(loc, "plasmarc").unwrap(), vec![PathBuf::from("plasmarc")]);
        assert!(resolve_entry(loc, "missing").unwrap().is_empty());
        assert_eq!(resolve_entry(loc, "plasma*rc").unwrap(), vec![PathBuf::from("plasmarc"), PathBuf::from("plasmashellrc")]);
        assert_eq!(resolve_entry(loc, "sub/*.conf").unwrap(), vec![PathBuf::from("sub/a.conf")]);
        assert!(resolve_entry(loc, "s*/a.conf").is_err());
        assert!(resolve_entry(loc, "../x").is_err());
    }
}
