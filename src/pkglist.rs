//! The package list: `packages.toml` from the repo's main branch, or the copy built into kast.
//!
//! The list decides what root installs, so a downloaded copy is untrusted input: every name
//! must look like a package name (no leading `-`, no whitespace or shell characters) and any
//! repo prefix must be one we know.

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The repo's `packages.toml` as of the commit this binary was built from.
pub const BUILTIN: &str = include_str!("../packages.toml");
/// The newest `schema` this build understands.
pub const SUPPORTED_SCHEMA: u32 = 1;
/// Known groups, in install order.
pub const GROUPS: [&str; 4] = ["official", "cachyos", "kud3n", "addons"];
/// Repo prefixes a name may carry (`archrepo/sine`).
pub const REPOS: [&str; 1] = ["archrepo"];
const NAME_MAX: usize = 100;
const TEXT_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Pkg {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Installed instead when `name` isn't in the sync databases but this is
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// An installed legacy package swapped out, when `name` declares a conflict with it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The package this add-on belongs to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
}

impl Pkg {
    /// The name without its repo prefix.
    pub fn bare(&self) -> &str {
        self.name.split_once('/').map_or(&self.name, |(_, n)| n)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub packages: Vec<Pkg>,
}

#[derive(Deserialize)]
struct RawGroup {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    packages: Vec<Pkg>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    group: Vec<RawGroup>,
}

/// A parsed and validated list. `warnings` are things skipped (unknown groups).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub schema: u32,
    pub groups: Vec<Group>,
    pub warnings: Vec<String>,
}

impl Parsed {
    /// Every package, in install order.
    pub fn packages(&self) -> impl Iterator<Item = &Pkg> {
        self.groups.iter().flat_map(|g| &g.packages)
    }

    pub fn count(&self) -> usize {
        self.groups.iter().map(|g| g.packages.len()).sum()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.packages().any(|p| p.name == name)
    }
}

/// Why a list was not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    Parse(String),
    NoSchema,
    NewerSchema(i64),
    Rule(String),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Invalid::Parse(e) => write!(f, "doesn't parse: {e}"),
            Invalid::NoSchema => write!(f, "has no valid schema"),
            Invalid::NewerSchema(n) => write!(f, "uses format v{n}; this kast understands v{SUPPORTED_SCHEMA}"),
            Invalid::Rule(e) => write!(f, "is invalid: {e}"),
        }
    }
}

fn name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:([a-z0-9][a-z0-9_-]*)/)?[a-z0-9@_+][a-z0-9@._+-]*$").expect("valid regex"))
}

/// A package name as pacman takes it, with at most a known repo prefix.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.len() > NAME_MAX {
        return Err(format!("name longer than {NAME_MAX} characters: {}…", name.chars().take(20).collect::<String>()));
    }
    let Some(caps) = name_regex().captures(name) else { return Err(format!("not a package name: {name:?}")) };
    match caps.get(1) {
        Some(repo) if !REPOS.contains(&repo.as_str()) => Err(format!("unknown repository {:?} in {name:?}", repo.as_str())),
        _ => Ok(()),
    }
}

/// Text shown in the UI: one line, no control characters.
fn valid_text(what: &str, text: &str) -> Result<(), String> {
    if text.chars().count() > TEXT_MAX || text.chars().any(char::is_control) {
        return Err(format!("{what} too long or has control characters"));
    }
    Ok(())
}

pub fn default_group_name(id: &str) -> &'static str {
    match id {
        "official" => "Official repositories",
        "cachyos" => "CachyOS repositories",
        "kud3n" => "kud3n's repository",
        "addons" => "Add-ons",
        _ => "",
    }
}

/// Parse `text` and check every rule. Unknown fields are ignored; unknown groups are skipped
/// with a warning.
pub fn parse(text: &str) -> Result<Parsed, Invalid> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| Invalid::Parse(e.message().to_string()))?;
    // The schema first: a newer format may not deserialize below, and that's what to report
    let schema = match table.get("schema") {
        Some(toml::Value::Integer(n)) if *n > SUPPORTED_SCHEMA as i64 => return Err(Invalid::NewerSchema(*n)),
        Some(toml::Value::Integer(n)) if *n >= 1 => *n as u32,
        _ => return Err(Invalid::NoSchema),
    };
    let raw: RawFile = table.try_into().map_err(|e: toml::de::Error| Invalid::Parse(e.message().to_string()))?;

    let rule = Invalid::Rule;
    let mut groups: Vec<Group> = Vec::new();
    let mut warnings = Vec::new();
    for g in raw.group {
        if !GROUPS.contains(&g.id.as_str()) {
            let id: String = g.id.chars().filter(|c| !c.is_control()).take(40).collect();
            warnings.push(format!("skipped unknown group {id:?} ({} packages)", g.packages.len()));
            continue;
        }
        if groups.iter().any(|x| x.id == g.id) {
            return Err(rule(format!("group {} appears twice", g.id)));
        }
        let name = g.name.unwrap_or_else(|| default_group_name(&g.id).to_string());
        valid_text("group name", &name).map_err(rule)?;
        groups.push(Group { id: g.id, name, packages: g.packages });
    }
    // Install order is the known order, whatever order the file lists the groups in
    groups.sort_by_key(|g| GROUPS.iter().position(|k| *k == g.id));

    let parsed = Parsed { schema, groups, warnings };
    let mut seen = HashSet::new();
    for p in parsed.packages() {
        valid_name(&p.name).map_err(rule)?;
        for n in [&p.fallback, &p.replaces, &p.requires].into_iter().flatten() {
            valid_name(n).map_err(|e| rule(format!("{}: {e}", p.name)))?;
        }
        if let Some(d) = &p.description {
            valid_text("description", d).map_err(|e| rule(format!("{}: {e}", p.name)))?;
        }
        // `sine` and `archrepo/sine` are the same package to pacman
        if !seen.insert(p.bare().to_string()) {
            return Err(rule(format!("{} is listed twice", p.name)));
        }
    }
    if seen.is_empty() {
        return Err(rule("no packages".into()));
    }
    for p in parsed.packages() {
        if let Some(r) = p.requires.as_ref().filter(|r| !parsed.contains(r)) {
            return Err(rule(format!("{} requires {r}, which is not in the list", p.name)));
        }
    }
    Ok(parsed)
}

/// The list built into this binary (checked by the tests, so it always parses).
pub fn builtin() -> Parsed {
    parse(BUILTIN).expect("the built-in packages.toml is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "schema = 1\n[[group]]\nid = \"official\"\n";

    fn one(pkgs: &str) -> Result<Parsed, Invalid> {
        parse(&format!("{HEAD}packages = [{pkgs}]\n"))
    }

    fn rule_err(r: Result<Parsed, Invalid>) -> String {
        match r {
            Err(Invalid::Rule(e)) => e,
            other => panic!("expected a rule error, got {other:?}"),
        }
    }

    #[test]
    fn builtin_reproduces_the_old_hard_coded_list() {
        // What PKGS_OFFICIAL, PKGS_KUD3N and PKGS_ADDONS installed, in order, before the file
        const OLD: &[&str] = &[
            "zen-browser-bin",
            "zotero",
            "proton-vpn-cli",
            "inkscape",
            "chromium",
            "obsidian",
            "fcitx5",
            "fcitx5-configtool",
            "fcitx5-gtk",
            "fcitx5-qt",
            "fcitx5-unikey",
            "vesktop-bin",
            "valkey",
            "kdenlive",
            "steam",
            "zed-globalmenu",
            "wine",
            "wine-gecko",
            "wine-mono",
            "gwenview",
            "haruna",
            "libreoffice-fresh",
            "libreoffice-fresh-vi",
            "beeper",
            "howdy-next",
            "gdlauncher-carbon",
            "searxng",
            "archrepo/freedownloadmanager",
            "archrepo/sine",
            "freedownloadmanager-elephant",
            "zotero-better-bibtex",
            "zotmoov",
            "zotero-better-notes",
            "zotero-ocr",
            "zotero-pdf-translate",
            "zotero-mcp",
        ];
        let b = builtin();
        assert_eq!(b.schema, 1);
        assert!(b.warnings.is_empty());
        assert_eq!(b.packages().map(|p| p.name.as_str()).collect::<Vec<_>>(), OLD);
        assert_eq!(b.groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), GROUPS);

        // The rules that used to be hard-coded in install_packages
        let rules: Vec<(&str, Option<&str>, Option<&str>)> = b
            .packages()
            .filter(|p| p.fallback.is_some() || p.replaces.is_some())
            .map(|p| (p.name.as_str(), p.fallback.as_deref(), p.replaces.as_deref()))
            .collect();
        assert_eq!(
            rules,
            [
                ("vesktop-bin", Some("vesktop"), None),
                ("zed-globalmenu", None, Some("zed")),
                ("archrepo/freedownloadmanager", None, Some("freedownloadmanager-bin")),
                ("archrepo/sine", None, Some("sine-bin")),
            ]
        );
        let addons = &b.groups[3].packages;
        assert!(addons.iter().all(|p| p.requires.is_some()));
        assert_eq!(addons[0].requires.as_deref(), Some("archrepo/freedownloadmanager"));
    }

    #[test]
    fn names_are_checked_strictly() {
        for good in ["zotero", "archrepo/sine", "libreoffice-fresh-vi", "lib32-gcc-libs", "gtk2+", "python@3", "r_pkg", "x.y"] {
            assert_eq!(valid_name(good), Ok(()), "{good}");
        }
        for bad in ["-Syu", "--overwrite=*", "foo bar", "a;b", "$(x)", "Zotero", "", "/sine", "a/b/c", ".hidden", "core/../x", "a\nb"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name("evil/zotero").unwrap_err().contains("unknown repository"));
        assert!(valid_name(&"a".repeat(NAME_MAX + 1)).is_err());
    }

    #[test]
    fn broken_files_are_rejected_with_the_reason() {
        assert!(matches!(parse("schema = 1\n[[group]\n"), Err(Invalid::Parse(_))));
        assert_eq!(parse("[[group]]\nid = \"official\"\npackages = [{ name = \"a\" }]\n"), Err(Invalid::NoSchema));
        assert_eq!(parse("schema = \"1\"\n"), Err(Invalid::NoSchema));
        // A newer format is reported as such even when its shape no longer fits ours
        assert_eq!(parse("schema = 2\ngroup = \"something else\"\n"), Err(Invalid::NewerSchema(2)));
        assert!(rule_err(one("{ name = \"-Syu\" }")).contains("not a package name"));
        assert!(rule_err(one("{ name = \"zotero\", fallback = \"--noconfirm\" }")).contains("zotero"));
        assert!(rule_err(one("{ name = \"evil/zotero\" }")).contains("unknown repository"));
        assert!(rule_err(one("{ name = \"a\" }, { name = \"a\" }")).contains("twice"));
        assert!(rule_err(one("{ name = \"sine\" }, { name = \"archrepo/sine\" }")).contains("twice"));
        assert!(rule_err(one("{ name = \"a\", requires = \"b\" }")).contains("not in the list"));
        assert!(rule_err(one("{ name = \"a\", description = \"x\\u001b[2J\" }")).contains("control"));
        assert!(rule_err(one("")).contains("no packages"));
        assert!(rule_err(parse("schema = 1\n")).contains("no packages"));
        assert!(rule_err(parse(&format!("{HEAD}packages = []\n[[group]]\nid = \"official\"\npackages = []\n"))).contains("twice"));
    }

    #[test]
    fn unknown_fields_are_ignored_and_unknown_groups_skipped() {
        let p = parse(
            "schema = 1\nfuture = true\n[[group]]\nid = \"games\"\npackages = [{ name = \"x\" }]\n\
             [[group]]\nid = \"addons\"\npackages = [{ name = \"b\", requires = \"a\", aur = true }]\n\
             [[group]]\nid = \"official\"\nicon = \"star\"\npackages = [{ name = \"a\", since = \"0.2\" }]\n",
        )
        .unwrap();
        assert_eq!(p.packages().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["a", "b"], "groups come in the known order");
        assert_eq!(p.groups[0].name, "Official repositories");
        assert_eq!(p.warnings, ["skipped unknown group \"games\" (1 packages)"]);
    }
}
