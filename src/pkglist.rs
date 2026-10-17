//! The package list: `packages.toml` from the repo's main branch, or the copy built into kast.
//!
//! The list decides what root installs, so a downloaded copy is untrusted input: every name
//! must look like a package name (no leading `-`, no whitespace or shell characters) and any
//! repo prefix must be one we know.

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The repo's `packages.toml` as of the commit this binary was built from.
pub const BUILTIN: &str = include_str!("../packages.toml");
/// What `builtin()` parses. Tests use a frozen copy (the list as of 0.1.3), so editing
/// packages.toml never breaks them; `shipped_packages_toml_is_valid` checks the real file.
#[cfg(not(test))]
const BUILTIN_TEXT: &str = BUILTIN;
#[cfg(test)]
pub const BUILTIN_TEXT: &str = include_str!("testdata/packages-frozen.toml");
/// The newest `schema` this build understands.
pub const SUPPORTED_SCHEMA: u32 = 1;
/// Known groups, in install order.
pub const GROUPS: [&str; 4] = ["official", "cachyos", "kud3n", "addons"];
/// Repo prefixes a name may carry (`archrepo/sine`).
pub const REPOS: [&str; 1] = ["archrepo"];
/// Where the current list lives, and how the source line names it.
pub const URL: &str = "https://raw.githubusercontent.com/kud3n013/kast/main/packages.toml";
pub const ORIGIN: &str = "kud3n013/kast@main";
const HOST: &str = "raw.githubusercontent.com";
/// Larger downloads are refused (curl's --max-filesize, and checked again here).
pub const MAX_BYTES: usize = 256 * 1024;
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
    parse(BUILTIN_TEXT).expect("the built-in packages.toml is valid")
}

impl Parsed {
    /// Whether the yay fallback may build `name` from the AUR. Only for packages the
    /// built-in list names (as a package or a fallback): a pushed edit to the online list
    /// can choose among repo packages, but can't get arbitrary AUR code built.
    pub fn allows_aur(&self, name: &str) -> bool {
        self.packages().any(|p| p.name == name || p.fallback.as_deref() == Some(name))
    }
}

/// Why the built-in list is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Offline,
    NoCurl,
    /// DNS, connect or timeout
    Network(String),
    Download(String),
    Tls(String),
    Empty,
    TooLarge,
    Invalid(Invalid),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Reason::Offline => write!(f, "offline: --offline given"),
            Reason::NoCurl => write!(f, "curl not found"),
            Reason::Network(d) => write!(f, "offline: could not reach {HOST} ({d})"),
            Reason::Download(d) => write!(f, "download failed ({d})"),
            Reason::Tls(d) => write!(f, "TLS error reaching {HOST} ({d})"),
            Reason::Empty => write!(f, "the online list is empty"),
            Reason::TooLarge => write!(f, "the online list is over {} KiB", MAX_BYTES / 1024),
            Reason::Invalid(Invalid::NewerSchema(n)) => {
                write!(f, "online list uses format v{n}; this kast understands v{SUPPORTED_SCHEMA}. Using the built-in list. Update kast to get the new format")
            }
            Reason::Invalid(i) => write!(f, "the online list {i}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Downloaded from `URL`; `fetched` is the local time, HH:MM
    Online {
        fetched: String,
    },
    BuiltIn(Reason),
    Local(PathBuf),
}

/// The list in use for this run and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgList {
    pub source: Source,
    pub list: Parsed,
}

/// Packages added and removed by a list, compared with the built-in one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// "+2 added / −1 removed vs built-in"
    pub fn summary(&self) -> String {
        format!("+{} added / −{} removed vs built-in", self.added.len(), self.removed.len())
    }
}

/// "kast 0.1.3 (caf3328)"; the commit only when the build could read it.
pub fn build_id() -> String {
    if cfg!(test) {
        // Stable across releases, for the snapshots
        return "kast 0.0.0".into();
    }
    match option_env!("KAST_COMMIT") {
        Some(c) if !c.is_empty() => format!("kast {} ({c})", env!("CARGO_PKG_VERSION")),
        _ => format!("kast {}", env!("CARGO_PKG_VERSION")),
    }
}

impl PkgList {
    pub fn builtin(reason: Reason) -> Self {
        PkgList { source: Source::BuiltIn(reason), list: builtin() }
    }

    pub fn is_builtin(&self) -> bool {
        matches!(self.source, Source::BuiltIn(_))
    }

    /// One line naming the source, e.g. "Online · kud3n013/kast@main · fetched 04:39 · 36 packages".
    pub fn source_line(&self) -> String {
        let n = self.list.count();
        match &self.source {
            Source::Online { fetched } => format!("Online · {ORIGIN} · fetched {fetched} · {n} packages"),
            Source::BuiltIn(r) => format!("Built-in · {} · {n} packages · {r}", build_id()),
            Source::Local(p) => format!("Local file · {} · {n} packages", p.display()),
        }
    }

    pub fn reason(&self) -> Option<&Reason> {
        match &self.source {
            Source::BuiltIn(r) => Some(r),
            _ => None,
        }
    }

    pub fn diff(&self) -> Diff {
        diff(&builtin(), &self.list)
    }
}

pub fn diff(base: &Parsed, list: &Parsed) -> Diff {
    Diff {
        added: list.packages().filter(|p| !base.contains(&p.name)).map(|p| p.name.clone()).collect(),
        removed: base.packages().filter(|p| !list.contains(&p.name)).map(|p| p.name.clone()).collect(),
    }
}

/// curl's arguments for `url`: HTTPS only, bounded in time and size, the body on stdout.
pub fn curl_args(url: &str) -> Vec<String> {
    let max = MAX_BYTES.to_string();
    ["-fsSL", "--proto", "=https", "--tlsv1.2", "--connect-timeout", "5", "--max-time", "15", "--max-filesize", &max, url].map(String::from).to_vec()
}

/// What a failed curl run means, from its exit code and `-sS` error line.
pub fn curl_reason(code: Option<i32>, stderr: &str) -> Reason {
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    // "curl: (22) The requested URL returned error: 404"
    let detail = line.split_once(") ").map_or(line, |(_, d)| d).chars().filter(|c| !c.is_control()).take(120).collect::<String>();
    match code {
        Some(5 | 6) => Reason::Network("DNS lookup failed".into()),
        Some(7) => Reason::Network("connection failed".into()),
        Some(28) => Reason::Network("timeout".into()),
        Some(22) => Reason::Download(detail.rsplit_once("error: ").map_or(detail.clone(), |(_, s)| format!("HTTP {s}"))),
        Some(35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91) => Reason::Tls(detail),
        Some(63) => Reason::TooLarge,
        Some(n) => Reason::Download(if detail.is_empty() { format!("curl exit {n}") } else { format!("curl exit {n}: {detail}") }),
        None => Reason::Download("curl was killed".into()),
    }
}

/// Download `url` with curl (on Arch it's always there: pacman depends on it).
pub fn curl_fetch(url: &str) -> Result<Vec<u8>, Reason> {
    let out = Command::new("curl").args(curl_args(url)).stdin(Stdio::null()).output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Reason::NoCurl,
        _ => Reason::Download(format!("could not run curl: {e}")),
    })?;
    if out.status.success() { Ok(out.stdout) } else { Err(curl_reason(out.status.code(), &String::from_utf8_lossy(&out.stderr))) }
}

fn from_body(body: &[u8]) -> Result<Parsed, Reason> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(Reason::Empty);
    }
    if body.len() > MAX_BYTES {
        return Err(Reason::TooLarge);
    }
    let text = std::str::from_utf8(body).map_err(|_| Reason::Invalid(Invalid::Parse("not UTF-8".into())))?;
    parse(text).map_err(Reason::Invalid)
}

/// The online list, or the built-in one (with the reason) when it can't be had or used.
/// `fetch` downloads a URL; tests pass a stand-in.
pub fn load(offline: bool, fetch: impl FnOnce(&str) -> Result<Vec<u8>, Reason>) -> PkgList {
    let got = if offline { Err(Reason::Offline) } else { fetch(URL).and_then(|b| from_body(&b)) };
    let pl = match got {
        Ok(list) => PkgList { source: Source::Online { fetched: chrono::Local::now().format("%H:%M").to_string() }, list },
        Err(reason) => PkgList::builtin(reason),
    };
    log_source(&pl);
    pl
}

/// `--packages-file`: a local file, for trying edits before pushing them. Errors are
/// errors here, not a fallback: the user asked for this file.
pub fn load_local(path: &Path) -> anyhow::Result<PkgList> {
    let mut body = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_BYTES as u64 + 1).read_to_end(&mut body))
        .map_err(|e| anyhow::anyhow!("Could not read {}: {e}", path.display()))?;
    let list = match from_body(&body) {
        Ok(l) => l,
        Err(Reason::Invalid(i)) => anyhow::bail!("{} {i}", path.display()),
        Err(Reason::Empty) => anyhow::bail!("{} is empty", path.display()),
        Err(r) => anyhow::bail!("{}: {r}", path.display()),
    };
    let pl = PkgList { source: Source::Local(path.to_path_buf()), list };
    log_source(&pl);
    Ok(pl)
}

fn log_source(pl: &PkgList) {
    use crate::ctx::log_file_only;
    log_file_only("INFO", &format!("Package list: {}", pl.source_line()));
    match &pl.source {
        Source::Local(_) => {}
        _ => log_file_only("INFO", &format!("Package list URL: {URL}")),
    }
    if let Source::BuiltIn(r) = &pl.source {
        log_file_only("WARN", &format!("Using the built-in package list: {r}"));
    }
    for w in &pl.list.warnings {
        log_file_only("WARN", &format!("Package list: {w}"));
    }
}

// --- kast packages --list ------------------------------------------------------------

/// A package's rules, as `--list` shows them: "→ fallback vesktop · needs zotero".
pub fn rules(p: &Pkg) -> String {
    let mut r = Vec::new();
    if let Some(f) = &p.fallback {
        r.push(format!("→ fallback {f}"));
    }
    if let Some(x) = &p.replaces {
        r.push(format!("replaces {x}"));
    }
    if let Some(x) = &p.requires {
        r.push(format!("needs {x}"));
    }
    r.join(" · ")
}

/// `kast packages --list`. `installed` holds installed package names (bare), when known.
pub fn list_text(pl: &PkgList, installed: Option<&HashSet<String>>) -> String {
    let mut out = format!("Package list: {}\n", pl.source_line());
    if !pl.is_builtin() {
        let d = pl.diff();
        if !d.is_empty() {
            out += &format!("  {}", d.summary());
            for n in &d.added {
                out += &format!("\n    + {n}");
            }
            for n in &d.removed {
                out += &format!("\n    − {n}");
            }
            out.push('\n');
        }
    }
    for w in &pl.list.warnings {
        out += &format!("  warning: {w}\n");
    }
    let width = pl.list.packages().map(|p| p.name.chars().count()).max().unwrap_or(0);
    for g in pl.list.groups.iter().filter(|g| !g.packages.is_empty()) {
        out += &format!("\n{} ({})\n", g.name, g.packages.len());
        for p in &g.packages {
            let mark = match installed {
                Some(set) if set.contains(p.bare()) => "✔ ",
                _ => "  ",
            };
            let line = format!("  {mark}{:width$}  {}", p.name, rules(p));
            out += line.trim_end();
            out.push('\n');
        }
    }
    if installed.is_some() {
        out += "\n✔ already installed\n";
    }
    out
}

/// `kast packages --list --json`.
pub fn list_json(pl: &PkgList, installed: Option<&HashSet<String>>) -> serde_json::Value {
    use serde_json::json;
    let (source, fetched) = match &pl.source {
        Source::Online { fetched } => ("online", Some(fetched.clone())),
        Source::BuiltIn(_) => ("builtin", None),
        Source::Local(_) => ("local", None),
    };
    let location = match &pl.source {
        Source::Online { .. } => URL.to_string(),
        Source::BuiltIn(_) => build_id(),
        Source::Local(p) => p.display().to_string(),
    };
    let d = if pl.is_builtin() { Diff::default() } else { pl.diff() };
    let groups: Vec<_> = pl
        .list
        .groups
        .iter()
        .map(|g| {
            let pkgs: Vec<_> = g
                .packages
                .iter()
                .map(|p| {
                    let mut v = serde_json::to_value(p).unwrap_or_default();
                    if let (Some(set), Some(o)) = (installed, v.as_object_mut()) {
                        o.insert("installed".into(), set.contains(p.bare()).into());
                    }
                    v
                })
                .collect();
            json!({ "id": g.id, "name": g.name, "packages": pkgs })
        })
        .collect();
    json!({
        "source": source,
        "line": pl.source_line(),
        "location": location,
        "fetched": fetched,
        "schema": pl.list.schema,
        "reason": pl.reason().map(|r| r.to_string()),
        "warnings": pl.list.warnings,
        "count": pl.list.count(),
        "added": d.added,
        "removed": d.removed,
        "groups": groups,
    })
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

    /// The one test that reads the real packages.toml (CI runs it on every change to the file).
    #[test]
    fn shipped_packages_toml_is_valid() {
        let p = parse(BUILTIN).unwrap_or_else(|e| panic!("packages.toml {e}"));
        assert!(p.warnings.is_empty(), "packages.toml: {:?}", p.warnings);
        assert!(p.count() > 0);
        assert!(p.schema <= SUPPORTED_SCHEMA);
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

    fn loaded(body: &str) -> PkgList {
        let body = body.as_bytes().to_vec();
        load(false, move |url| {
            assert_eq!(url, URL);
            Ok(body)
        })
    }

    fn fell_back(pl: &PkgList) -> &Reason {
        assert_eq!(pl.list, builtin(), "the built-in list is used");
        pl.reason().expect("built-in source")
    }

    #[test]
    fn online_list_is_used_when_valid() {
        let pl = loaded("schema = 1\n[[group]]\nid = \"official\"\npackages = [{ name = \"zotero\" }, { name = \"gimp\" }]\n");
        assert!(matches!(pl.source, Source::Online { .. }));
        assert_eq!(pl.list.count(), 2);
        assert!(pl.source_line().starts_with("Online · kud3n013/kast@main · fetched "));
        assert!(pl.source_line().ends_with(" · 2 packages"));
        let d = pl.diff();
        assert_eq!(d.added, ["gimp"]);
        assert_eq!(d.removed.len(), 35);
        assert_eq!(d.summary(), "+1 added / −35 removed vs built-in");
        assert!(loaded(BUILTIN_TEXT).diff().is_empty());
    }

    #[test]
    fn every_bad_download_falls_back_with_its_reason() {
        let dash = "schema = 1\n[[group]]\nid = \"official\"\npackages = [{ name = \"-Syu\" }]\n";
        let prefix = "schema = 1\n[[group]]\nid = \"official\"\npackages = [{ name = \"core/linux\" }]\n";
        let dup = "schema = 1\n[[group]]\nid = \"official\"\npackages = [{ name = \"a\" }, { name = \"a\" }]\n";
        let dangling = "schema = 1\n[[group]]\nid = \"addons\"\npackages = [{ name = \"a\", requires = \"b\" }]\n";
        let big = format!("schema = 1\n# {}\n", "x".repeat(MAX_BYTES));
        type Check = Box<dyn Fn(&Reason) -> bool>;
        let cases: Vec<(&str, Check)> = vec![
            ("schema = [", Box::new(|r| matches!(r, Reason::Invalid(Invalid::Parse(_))))),
            ("schema = 2\n", Box::new(|r| *r == Reason::Invalid(Invalid::NewerSchema(2)))),
            (dash, Box::new(|r| matches!(r, Reason::Invalid(Invalid::Rule(e)) if e.contains("-Syu")))),
            (prefix, Box::new(|r| matches!(r, Reason::Invalid(Invalid::Rule(e)) if e.contains("unknown repository")))),
            (dup, Box::new(|r| matches!(r, Reason::Invalid(Invalid::Rule(e)) if e.contains("twice")))),
            (dangling, Box::new(|r| matches!(r, Reason::Invalid(Invalid::Rule(e)) if e.contains("requires b")))),
            (&big, Box::new(|r| *r == Reason::TooLarge)),
            ("", Box::new(|r| *r == Reason::Empty)),
            (" \n", Box::new(|r| *r == Reason::Empty)),
        ];
        for (body, want) in cases {
            let pl = loaded(body);
            let r = fell_back(&pl);
            assert!(want(r), "{:.40?} gave {r:?}", body);
        }
        assert_eq!(
            Reason::Invalid(Invalid::NewerSchema(2)).to_string(),
            "online list uses format v2; this kast understands v1. Using the built-in list. Update kast to get the new format"
        );
        let r = fell_back(&load(false, |_| Ok(vec![0xff, 0xfe]))).clone();
        assert_eq!(r.to_string(), "the online list doesn't parse: not UTF-8");
    }

    #[test]
    fn offline_and_fetch_errors_use_the_built_in_list() {
        let pl = load(true, |_| panic!("--offline must not fetch"));
        assert_eq!(*fell_back(&pl), Reason::Offline);
        assert_eq!(pl.source_line(), "Built-in · kast 0.0.0 · 36 packages · offline: --offline given");
        let pl = load(false, |_| Err(Reason::Network("timeout".into())));
        assert_eq!(pl.source_line(), "Built-in · kast 0.0.0 · 36 packages · offline: could not reach raw.githubusercontent.com (timeout)");
        assert_eq!(*fell_back(&load(false, |_| Err(Reason::NoCurl))), Reason::NoCurl);
        assert!(pl.diff().is_empty());
    }

    #[test]
    fn curl_is_called_safely_and_its_errors_read() {
        assert_eq!(
            curl_args(URL),
            [
                "-fsSL",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--connect-timeout",
                "5",
                "--max-time",
                "15",
                "--max-filesize",
                "262144",
                "https://raw.githubusercontent.com/kud3n013/kast/main/packages.toml"
            ]
        );
        assert_eq!(curl_reason(Some(6), "curl: (6) Could not resolve host: raw.githubusercontent.com\n"), Reason::Network("DNS lookup failed".into()));
        assert_eq!(curl_reason(Some(28), "curl: (28) Connection timed out after 5001 milliseconds"), Reason::Network("timeout".into()));
        assert_eq!(curl_reason(Some(22), "curl: (22) The requested URL returned error: 404\n"), Reason::Download("HTTP 404".into()));
        assert_eq!(
            curl_reason(Some(60), "curl: (60) SSL certificate problem: unable to get local issuer certificate"),
            Reason::Tls("SSL certificate problem: unable to get local issuer certificate".into())
        );
        assert_eq!(curl_reason(Some(63), ""), Reason::TooLarge);
        assert_eq!(curl_reason(Some(3), ""), Reason::Download("curl exit 3".into()));
        assert_eq!(curl_reason(None, ""), Reason::Download("curl was killed".into()));
        assert_eq!(Reason::Network("timeout".into()).to_string(), "offline: could not reach raw.githubusercontent.com (timeout)");
    }

    #[test]
    fn local_files_are_named_and_errors_are_errors() {
        let t = tempfile::tempdir().unwrap();
        let good = t.path().join("packages.toml");
        std::fs::write(&good, BUILTIN_TEXT).unwrap();
        let pl = load_local(&good).unwrap();
        assert_eq!(pl.source, Source::Local(good.clone()));
        assert_eq!(pl.source_line(), format!("Local file · {} · 36 packages", good.display()));
        let bad = t.path().join("bad.toml");
        std::fs::write(&bad, "schema = 3\n").unwrap();
        assert!(load_local(&bad).unwrap_err().to_string().ends_with("bad.toml uses format v3; this kast understands v1"));
        assert!(load_local(&t.path().join("missing")).unwrap_err().to_string().starts_with("Could not read"));
    }

    #[test]
    fn only_built_in_packages_may_be_built_from_the_aur() {
        let b = builtin();
        assert!(b.allows_aur("zen-browser-bin"));
        assert!(b.allows_aur("vesktop"), "a built-in fallback");
        assert!(!b.allows_aur("some-aur-thing"));
    }

    fn online(body: &str) -> PkgList {
        PkgList { source: Source::Online { fetched: "04:39".into() }, list: parse(body).unwrap() }
    }

    #[test]
    fn list_output_text_and_json() {
        let installed: HashSet<String> = ["zotero", "sine", "fcitx5"].map(String::from).into();
        let pl = PkgList::builtin(Reason::Network("timeout".into()));
        insta::assert_snapshot!("list_builtin", list_text(&pl, Some(&installed)));
        insta::assert_snapshot!("list_builtin_json", serde_json::to_string_pretty(&list_json(&pl, Some(&installed))).unwrap());

        let edited = BUILTIN_TEXT
            .replace("  { name = \"steam\" },\n", "")
            .replace("  { name = \"wine\" },", "  { name = \"wine\" },\n  { name = \"gimp\", description = \"Image editor\" },");
        let pl = online(&format!("{edited}\n[[group]]\nid = \"games\"\npackages = [{{ name = \"x\" }}]\n"));
        insta::assert_snapshot!("list_online", list_text(&pl, None));
        let j = list_json(&pl, None);
        assert_eq!(j["source"], "online");
        assert_eq!(j["added"], serde_json::json!(["gimp"]));
        assert_eq!(j["removed"], serde_json::json!(["steam"]));
        assert_eq!(j["warnings"][0], "skipped unknown group \"games\" (1 packages)");
        assert_eq!(j["groups"][0]["packages"][16]["description"], "Image editor");
        assert!(j["groups"][0]["packages"][0].get("installed").is_none());
    }
}
