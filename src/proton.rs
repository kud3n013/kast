//! Proton Drive through Proton's own CLI, `proton-drive` (github.com/ProtonDriveApps/sdk, `cli/`).
//!
//! Every invocation goes through here: it always runs as the target user (as root it would see
//! root's empty keyring and home), with stdin on /dev/null, `--json` for anything parsed, both
//! conflict strategies on every transfer (without them a conflict prompt reads EOF and silently
//! skips the file), and stderr scrubbed of the sign-in URL before it reaches the log.
//!
//! The command builders and parsers are pure; `Cli::run` is the only place that spawns.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::ctx::UserEnv;
use crate::logio::Launch;

/// The oldest CLI kast works with, and the release line it was tested with.
pub const PROTON_MIN: &str = "0.9.0";
/// The archrepo package that ships the CLI (once it exists).
// TODO(proton-pkg): once archrepo has it, add it to packages.toml in a new "backup" group,
// unchecked by default (the schema needs a per-package default for that).
pub const PROTON_PKG: &str = "proton-drive-cli-bin";
pub const BIN: &str = "proton-drive";
pub const DOWNLOAD_PAGE: &str = "https://proton.me/download/drive/cli/index.html";
pub const MY_FILES: &str = "/my-files";
/// What the sign-in URL is replaced with wherever text is logged or shown as output.
pub const URL_HIDDEN: &str = "<sign-in URL hidden>";
/// `auth login` gives up after this long.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
const SMALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A transfer has no timeout of its own: only cancel (or the CLI giving up) ends it.
const NO_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 24);

// --- Versions -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"(\d+)\.(\d+)\.(\d+)").unwrap());
        let c = re.captures(s)?;
        Some(Version(c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?))
    }
}

pub fn min_version() -> Version {
    Version::parse(PROTON_MIN).unwrap()
}

/// The CLI's version from `proton-drive version` ("Proton Drive CLI <name>@0.9.0", then the SDK's).
pub fn parse_version(out: &str) -> Option<Version> {
    out.lines().find(|l| l.to_ascii_lowercase().contains("cli")).and_then(Version::parse).or_else(|| out.lines().find_map(Version::parse))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    Ok,
    /// Older than `PROTON_MIN`: please update
    TooOld,
    /// A newer major (or 1.x): usable, but kast was tested with 0.9.x
    NewerMajor,
}

pub fn compat(v: Version) -> Compat {
    let min = min_version();
    if v < min {
        Compat::TooOld
    } else if v.0 > min.0 {
        Compat::NewerMajor
    } else {
        Compat::Ok
    }
}

// --- Finding the binary ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    Missing,
    Found {
        path: PathBuf,
        version: Version,
        compat: Compat,
    },
    /// There, but `version` didn't work (no AVX2, not executable, ...)
    Broken {
        path: PathBuf,
        problem: Problem,
    },
}

impl Install {
    /// Usable for Drive commands (an old version is not).
    pub fn usable(&self) -> bool {
        matches!(self, Install::Found { compat: Compat::Ok | Compat::NewerMajor, .. })
    }

    /// One line for the details pane and `kast drive status`.
    pub fn describe(&self) -> String {
        match self {
            Install::Missing => "proton-drive is not installed".into(),
            Install::Found { path, version, compat: Compat::Ok } => format!("proton-drive {version} ({})", path.display()),
            Install::Found { path, version, compat: Compat::TooOld } => {
                format!("proton-drive {version} ({}) is too old: kast needs {PROTON_MIN} or newer, please update it", path.display())
            }
            Install::Found { path, version, compat: Compat::NewerMajor } => {
                format!("proton-drive {version} ({}): kast was tested with 0.9.x; it may not work as expected", path.display())
            }
            Install::Broken { path, problem } => format!("proton-drive ({}) doesn't run: {problem}", path.display()),
        }
    }
}

/// Where to look, in order: an override (debug builds), our PATH, the user's ~/.local/bin,
/// then the usual system places. As root our PATH is sudo's, so the user's own bin is explicit.
pub fn candidates(path_env: &str, home: &str, overridden: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(o) = overridden.filter(|o| !o.is_empty()) {
        return vec![PathBuf::from(o)];
    }
    for dir in path_env.split(':').filter(|d| d.starts_with('/')) {
        out.push(Path::new(dir).join(BIN));
    }
    for dir in [format!("{home}/.local/bin"), "/usr/local/bin".into(), "/usr/bin".into()] {
        out.push(Path::new(&dir).join(BIN));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `KAST_PROTON_BIN` points kast at a fake CLI for smoke tests; release builds ignore it.
pub fn override_bin() -> Option<String> {
    if cfg!(debug_assertions) { std::env::var("KAST_PROTON_BIN").ok().filter(|v| !v.is_empty()) } else { None }
}

/// The CPU build to download: x64 needs AVX2.
pub fn cpu_build(cpuinfo: &str) -> &'static str {
    let avx2 = cpuinfo.lines().filter(|l| l.starts_with("flags")).any(|l| l.split_whitespace().any(|f| f == "avx2"));
    if avx2 { "linux/x64" } else { "linux/x64-baseline" }
}

/// How kast runs the CLI found at `bin` for the target user, with their credential store.
pub fn system_cli(bin: PathBuf) -> Cli {
    Cli { bin, root: crate::ctx::ctx().is_root, user: crate::ctx::user_env(), store: crate::settings::load().store() }
}

/// Find the CLI and ask its version. The `Cli` is there whenever a binary was found.
pub fn detect() -> (Install, Option<Cli>) {
    let c = crate::ctx::ctx();
    let path = std::env::var("PATH").unwrap_or_default();
    let found = candidates(&path, &c.target_home.display().to_string(), override_bin().as_deref()).into_iter().find(|p| executable(p));
    match found {
        Some(bin) => {
            let cli = system_cli(bin);
            (cli.version(), Some(cli))
        }
        None => (Install::Missing, None),
    }
}

/// Whether pacman knows the CLI's package (kud3n's repository ships it).
pub fn package_available() -> bool {
    crate::ctx::ok("pacman", &["-Si", PROTON_PKG])
}

static STATUS: std::sync::Mutex<Option<Status>> = std::sync::Mutex::new(None);

/// The sign-in status, checked once per session (`forget_status` after sign-in/out or `r`).
pub fn cached_status(cli: &Cli) -> Status {
    if let Some(s) = STATUS.lock().unwrap().clone() {
        return s;
    }
    let s = cli.status();
    *STATUS.lock().unwrap() = Some(s.clone());
    s
}

pub fn forget_status() {
    *STATUS.lock().unwrap() = None;
}

// --- What went wrong ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    NotInstalled,
    /// No session on this computer, or the server revoked it
    SignedOut,
    /// The credential store (Secret Service / KWallet / pass) can't be used
    Keyring(String),
    Offline(String),
    /// The account or plan can't use the CLI
    NotAllowed(String),
    QuotaFull(String),
    RateLimited(String),
    /// Killed by SIGILL: this CPU needs the x64-baseline build
    NeedsBaseline,
    NotFound(String),
    Exists(String),
    TimedOut,
    Canceled,
    Other(String),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::NotInstalled => f.write_str("proton-drive is not installed"),
            Problem::SignedOut => f.write_str("Not signed in to Proton Drive (run `kast drive login` first)"),
            Problem::Keyring(m) => write!(f, "The keyring that holds the Proton Drive session can't be used: {m}"),
            Problem::Offline(m) => write!(f, "Can't reach Proton Drive (offline?): {m}"),
            Problem::NotAllowed(m) => write!(f, "This Proton account can't use the Proton Drive CLI: {m}"),
            Problem::QuotaFull(_) => f.write_str("Proton Drive is full"),
            Problem::RateLimited(_) => f.write_str("Proton Drive is rate-limiting this account, try later"),
            Problem::NeedsBaseline => f.write_str("proton-drive crashed with \"Illegal instruction\": this CPU needs the linux/x64-baseline build"),
            Problem::NotFound(m) => write!(f, "Not found on Proton Drive: {m}"),
            Problem::Exists(m) => write!(f, "Already exists on Proton Drive: {m}"),
            Problem::TimedOut => f.write_str("proton-drive took too long and was stopped"),
            Problem::Canceled => f.write_str("Canceled"),
            Problem::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Problem {}

impl Problem {}

fn any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

/// Classify a failed call from its exit and stderr (and stdout, where `--json` failures land).
/// The texts come from the CLI and SDK sources; anything unknown is `Other` with the text.
pub fn classify(exit: &Exit, text: &str) -> Problem {
    match exit {
        Exit::TimedOut => return Problem::TimedOut,
        Exit::Canceled => return Problem::Canceled,
        Exit::Signal(4) | Exit::Code(132) => return Problem::NeedsBaseline,
        _ => {}
    }
    let clean = scrub(text);
    let l = clean.to_ascii_lowercase();
    // The CLI prints the error message last (after any progress or browser noise); skip stack frames
    let first = clean.lines().map(str::trim).rfind(|l| !l.is_empty() && !l.starts_with("at ") && !l.starts_with("===")).unwrap_or("").to_string();
    let msg = if first.is_empty() { format!("{exit}") } else { first };
    if l.contains("illegal instruction") {
        Problem::NeedsBaseline
    } else if l.contains("you need to login first") {
        Problem::SignedOut
    } else if any(
        &l,
        &[
            "failed to load session from secrets",
            "failed to load session from pass",
            "session in pass",
            "secret service",
            "org.freedesktop.secrets",
            "libsecret",
            "kwallet",
            "keyring",
            "no such interface",
            "dbus",
            "d-bus",
            "gpg-agent",
            "gpg: decryption failed",
        ],
    ) {
        Problem::Keyring(msg)
    } else if any(
        &l,
        &[
            "401",
            "unauthorized",
            "invalid access token",
            "invalid refresh token",
            "session expired",
            "session has expired",
            "re-authenticate",
            "please log in",
            "please sign in",
        ],
    ) {
        Problem::SignedOut
    } else if any(&l, &["ratelimitederror", "too many server requests", "too many requests", " 429", "rate limit", "rate-limit"]) {
        Problem::RateLimited(msg)
    } else if any(
        &l,
        &[
            "insufficient quota",
            "insufficient space",
            "insufficient storage",
            "not enough space",
            "not enough storage",
            "out of storage",
            "storage quota",
            "quota exceeded",
            "storage is full",
            "run out of storage",
        ],
    ) {
        Problem::QuotaFull(msg)
    } else if any(
        &l,
        &[
            "connectionerror",
            "offlineerror",
            "enotfound",
            "eai_again",
            "econnrefused",
            "econnreset",
            "etimedout",
            "getaddrinfo",
            "unable to connect",
            "network error",
            "fetch failed",
            "certificate",
            "self signed",
            "tls",
            "offline",
        ],
    ) {
        Problem::Offline(msg)
    } else if any(&l, &["node not found", "device not found", "root node not found"]) {
        Problem::NotFound(msg)
    } else if l.contains("already exists") {
        Problem::Exists(msg)
    } else if any(
        &l,
        &[
            "not available on your plan",
            "not allowed for this plan",
            "requires a business",
            "upgrade your plan to use",
            "subscription required",
            "plan does not allow",
        ],
    ) {
        Problem::NotAllowed(msg)
    } else {
        Problem::Other(msg)
    }
}

// --- Sign-in URL ----------------------------------------------------------------------------

/// The sign-in URL carries the key handover: it is never logged, never in the journal, and
/// prints as `<sign-in URL hidden>` everywhere except where it's deliberately shown.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInUrl(String);

impl SignInUrl {
    /// The real URL, for the QR code, the clipboard and the browser.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SignInUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(URL_HIDDEN)
    }
}

impl fmt::Display for SignInUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(URL_HIDDEN)
    }
}

/// The host of an `https://` URL (no userinfo, no port), lowercased.
fn https_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') || authority.contains(':') || authority.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(authority.to_ascii_lowercase())
}

pub fn valid_sign_in_url(url: &str) -> bool {
    https_host(url).is_some_and(|h| h == "proton.me" || h.ends_with(".proton.me")) && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The `{"signInUrl":"…"}` line of `auth login --json`. None for any other line; an error
/// (never containing the URL) for a URL kast won't show.
pub fn parse_sign_in(line: &str) -> Option<Result<SignInUrl, String>> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let url = v.get("signInUrl")?.as_str()?;
    Some(if valid_sign_in_url(url) {
        Ok(SignInUrl(url.to_string()))
    } else {
        Err("proton-drive gave a sign-in address that isn't an https:// proton.me address; not opening it".into())
    })
}

/// Hide sign-in material in text that is about to be logged or shown: the value of any
/// `signInUrl` and any proton.me URL with a path, query or fragment.
pub fn scrub(text: &str) -> String {
    static JSON: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let json = JSON.get_or_init(|| Regex::new(r#""signInUrl"\s*:\s*"(?:[^"\\]|\\.)*""#).unwrap());
    let url = URL.get_or_init(|| Regex::new(r#"(?i)https?://(?:[a-z0-9-]+\.)*proton\.me(?::\d+)?[/?#][^\s"'<>]*"#).unwrap());
    let t = json.replace_all(text, format!("\"signInUrl\":\"{URL_HIDDEN}\"").as_str());
    url.replace_all(&t, |c: &regex::Captures| {
        // The download page is fine to show
        if c[0].trim_end_matches(['.', ',', ')']) == DOWNLOAD_PAGE { c[0].to_string() } else { URL_HIDDEN.to_string() }
    })
    .into_owned()
}

// --- Transfer summaries and listings --------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub node_uid: Option<String>,
    #[serde(default)]
    pub error: String,
}

/// The JSON summary `filesystem upload/download --json` prints last.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub transferred_items: u64,
    #[serde(default)]
    pub transferred_bytes: u64,
    #[serde(default)]
    pub skipped_items: u64,
    pub failed_items: u64,
    #[serde(default)]
    pub failures: Vec<Failure>,
}

/// The last line of `stdout` that parses as a summary; None when there isn't one (a crash,
/// or output kast doesn't understand), which callers treat as a failure.
pub fn parse_summary(stdout: &str) -> Option<Summary> {
    stdout.lines().rev().map(str::trim).filter(|l| l.starts_with('{')).find_map(|l| serde_json::from_str::<Summary>(l).ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    Other,
}

/// What kast reads of a Drive node. The CLI prints raw SDK objects whose fields may change
/// between versions, so only these are read, each from a few possible places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub uid: String,
    pub kind: Kind,
    pub size: Option<u64>,
    /// ISO 8601 as printed (content modification time, else creation time)
    pub modified: Option<String>,
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, k| cur.get(k)).filter(|v| !v.is_null())
}

fn first_str(v: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|p| at(v, p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
}

fn first_u64(v: &Value, paths: &[&[&str]]) -> Option<u64> {
    paths.iter().find_map(|p| at(v, p).and_then(|x| x.as_u64().or_else(|| x.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))))
}

/// One node; None when it has no readable name (a name that failed to decrypt).
pub fn parse_node(v: &Value) -> Option<Node> {
    // name: a plain string, or the SDK's Result `{ ok: true, value }`
    let name = match v.get("name") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(obj @ Value::Object(_)) if obj.get("ok").and_then(Value::as_bool) != Some(false) => obj.get("value").and_then(Value::as_str).map(String::from),
        _ => first_str(v, &[&["nodeName"], &["fileName"]]),
    }
    .filter(|n| !n.is_empty())?;
    let uid = first_str(v, &[&["uid"], &["nodeUid"], &["id"]]).unwrap_or_default();
    let kind = match first_str(v, &[&["type"], &["nodeType"], &["kind"]]).map(|t| t.to_ascii_lowercase()).as_deref() {
        Some("file") => Kind::File,
        Some("folder") => Kind::Folder,
        _ => Kind::Other,
    };
    let size = first_u64(v, &[&["activeRevision", "claimedSize"], &["claimedSize"], &["size"], &["activeRevision", "storageSize"], &["totalStorageSize"]]);
    let modified = first_str(
        v,
        &[&["activeRevision", "claimedModificationTime"], &["modificationTime"], &["modifiedTime"], &["activeRevision", "creationTime"], &["creationTime"]],
    );
    Some(Node { name, uid, kind, size, modified })
}

/// `filesystem list --json`: a JSON array (streamed, one object per line). Entries kast can't
/// read are skipped; output that isn't an array at all is an error.
pub fn parse_list(stdout: &str) -> Result<Vec<Node>, String> {
    let v: Value = serde_json::from_str(stdout.trim()).map_err(|e| format!("proton-drive printed a listing kast can't read ({e})"))?;
    let arr = v.as_array().ok_or_else(|| "proton-drive printed a listing kast can't read (not a list)".to_string())?;
    Ok(arr.iter().filter_map(parse_node).collect())
}

/// `filesystem info --json`: one object.
pub fn parse_info(stdout: &str) -> Option<Node> {
    let line = stdout.lines().map(str::trim).rfind(|l| l.starts_with('{'))?;
    parse_node(&serde_json::from_str(line).ok()?)
}

// --- Commands -------------------------------------------------------------------------------

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub fn args_version() -> Vec<String> {
    strs(&["version"])
}

/// The status check: the cheapest call that needs a working session.
pub fn args_status() -> Vec<String> {
    strs(&["filesystem", "info", MY_FILES, "--json"])
}

pub fn args_login() -> Vec<String> {
    strs(&["auth", "login", "--json"])
}

pub fn args_logout() -> Vec<String> {
    strs(&["auth", "logout"])
}

pub fn args_list(path: &str) -> Vec<String> {
    vec!["filesystem".into(), "list".into(), path.into(), "--json".into()]
}

pub fn args_info(path: &str) -> Vec<String> {
    vec!["filesystem".into(), "info".into(), path.into(), "--json".into()]
}

pub fn args_create_folder(parent: &str, name: &str) -> Vec<String> {
    vec!["filesystem".into(), "create-folder".into(), parent.into(), name.into(), "--json".into()]
}

/// Upload: a name clash is renamed, never replaced (nothing on Drive is overwritten).
pub fn args_upload(files: &[PathBuf], parent: &str) -> Vec<String> {
    let mut v = strs(&["filesystem", "upload"]);
    v.extend(files.iter().map(|f| f.display().to_string()));
    v.push(parent.into());
    v.extend(strs(&["--json", "--file-conflict-strategy", "rename", "--folder-conflict-strategy", "merge", "--skip-thumbnails"]));
    v
}

/// Download into a fresh folder: renaming makes a clash impossible to lose data on.
pub fn args_download(remote: &[String], dir: &Path) -> Vec<String> {
    let mut v = strs(&["filesystem", "download"]);
    v.extend(remote.iter().cloned());
    v.push(dir.display().to_string());
    v.extend(strs(&["--json", "--file-conflict-strategy", "rename", "--folder-conflict-strategy", "rename"]));
    v
}

/// Move to Drive's trash (recoverable there). kast never deletes or empties the trash.
pub fn args_trash(paths: &[String]) -> Vec<String> {
    let mut v = strs(&["filesystem", "trash"]);
    v.extend(paths.iter().cloned());
    v.push("--json".into());
    v
}

/// `/my-files/kast-backups` + `host` + `name`, without doubled slashes.
pub fn join(base: &str, parts: &[&str]) -> String {
    let mut s = base.trim_end_matches('/').to_string();
    for p in parts {
        s.push('/');
        s.push_str(p.trim_matches('/'));
    }
    s
}

// --- Running it -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    TimedOut,
    Canceled,
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "proton-drive exited with code {c}"),
            Exit::Signal(s) => write!(f, "proton-drive was killed by signal {s}"),
            Exit::TimedOut => f.write_str("proton-drive timed out"),
            Exit::Canceled => f.write_str("canceled"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub exit: Exit,
    /// Raw: may hold the sign-in URL (only `auth login` prints one); parse, don't log
    pub stdout: String,
    /// Already scrubbed
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.exit == Exit::Code(0)
    }

    /// The problem, when the call failed.
    pub fn problem(&self) -> Option<Problem> {
        (!self.ok()).then(|| classify(&self.exit, &format!("{}\n{}", self.stderr, scrub(&self.stdout))))
    }
}

/// How `proton-drive` is run: the binary, as whom, and the credential store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub bin: PathBuf,
    /// kast is root: go through `sudo -u <user> env …`
    pub root: bool,
    pub user: UserEnv,
    /// `PROTON_DRIVE_CREDENTIALS_STORE` (only "pass" is ever set; unsafe_file never)
    pub store: Option<String>,
}

impl Cli {
    /// The command line for `args`, as the target user.
    pub fn launch(&self, args: &[String]) -> Launch {
        let mut argv = vec![self.bin.display().to_string()];
        argv.extend_from_slice(args);
        let extra: Vec<(String, String)> = self
            .store
            .iter()
            .filter(|s| s.as_str() == "pass" || s.as_str() == "keychain")
            .map(|s| ("PROTON_DRIVE_CREDENTIALS_STORE".to_string(), s.clone()))
            .collect();
        Launch::as_user(self.root, &self.user, &extra, &argv)
    }

    /// The process for `args`: stdin /dev/null, stdout and stderr piped, in its own session
    /// (no controlling terminal: neither it nor sudo can touch ours; its process group id is
    /// its pid, which `kill_group` uses).
    pub fn command(&self, args: &[String]) -> std::process::Command {
        let mut cmd = self.launch(args).command();
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no memory of ours
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd
    }

    /// Run `args`, waiting at most `timeout`. Each stderr line is scrubbed, logged and handed
    /// to `on_line`, which also gets None about once a second (for an elapsed-time display).
    /// On a TUI job thread the child is registered, so a cancel stops it.
    pub fn run(&self, args: &[String], timeout: Duration, on_line: &mut dyn FnMut(Option<&str>)) -> io::Result<Output> {
        self.run_with(args, timeout, on_line, None)
    }

    /// `run`, also stopped when `cancel` is set (a download outside a job: Esc sets it).
    fn run_with(
        &self,
        args: &[String],
        timeout: Duration,
        on_line: &mut dyn FnMut(Option<&str>),
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> io::Result<Output> {
        let mut child = self.command(args).spawn()?;
        let pgid = child.id() as i32;
        let tracked = crate::report::owns_terminal();
        if tracked {
            crate::report::set_child(Some(pgid));
        }
        let mut out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = out.read_to_end(&mut s);
            String::from_utf8_lossy(&s).into_owned()
        });
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let err_reader = std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let deadline = Instant::now() + timeout;
        let mut stderr = String::new();
        let handle = |line: String, stderr: &mut String, on_line: &mut dyn FnMut(Option<&str>)| {
            let clean = crate::ui::sanitize_log(&scrub(&line));
            crate::ctx::log_file_only("PROTON", &clean);
            stderr.push_str(&clean);
            stderr.push('\n');
            on_line(Some(&clean));
        };
        let mut next_tick = Instant::now() + Duration::from_secs(1);
        let exit = loop {
            while let Ok(line) = rx.try_recv() {
                handle(line, &mut stderr, on_line);
            }
            if let Some(st) = child.try_wait()? {
                break match (st.code(), st.signal()) {
                    (Some(c), _) => Exit::Code(c),
                    (None, Some(s)) => Exit::Signal(s),
                    _ => Exit::Code(-1),
                };
            }
            if Instant::now() >= next_tick {
                on_line(None);
                next_tick += Duration::from_secs(1);
            }
            let canceled = (tracked && crate::report::cancel_requested()) || cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst));
            if canceled || Instant::now() >= deadline {
                kill_group(pgid);
                let _ = child.wait();
                break if canceled { Exit::Canceled } else { Exit::TimedOut };
            }
            std::thread::sleep(Duration::from_millis(40));
        };
        if tracked {
            crate::report::set_child(None);
        }
        let _ = err_reader.join();
        while let Ok(line) = rx.try_recv() {
            handle(line, &mut stderr, on_line);
        }
        let stdout = reader.join().unwrap_or_default();
        Ok(Output { exit, stdout, stderr })
    }

    pub fn run_quiet(&self, args: &[String], timeout: Duration) -> io::Result<Output> {
        self.run(args, timeout, &mut |_| {})
    }

    /// Run and turn a failure (or a spawn error) into a `Problem`.
    pub fn call(&self, args: &[String], timeout: Duration, on_line: &mut dyn FnMut(Option<&str>)) -> Result<Output, Problem> {
        let out = self.run(args, timeout, on_line).map_err(|e| spawn_problem(&e))?;
        match out.problem() {
            Some(p) => Err(p),
            None => Ok(out),
        }
    }

    pub fn version(&self) -> Install {
        match self.run_quiet(&args_version(), VERSION_TIMEOUT) {
            Err(e) => Install::Broken { path: self.bin.clone(), problem: spawn_problem(&e) },
            Ok(o) => match (o.ok(), parse_version(&o.stdout)) {
                (true, Some(v)) => Install::Found { path: self.bin.clone(), version: v, compat: compat(v) },
                (true, None) => Install::Broken { path: self.bin.clone(), problem: Problem::Other("`proton-drive version` printed no version".into()) },
                (false, _) => Install::Broken { path: self.bin.clone(), problem: o.problem().unwrap_or(Problem::Other(o.exit.to_string())) },
            },
        }
    }

    /// Signed in? (see `Status`)
    pub fn status(&self) -> Status {
        match self.run_quiet(&args_status(), STATUS_TIMEOUT) {
            Err(e) => Status::from_problem(spawn_problem(&e)),
            Ok(o) => match o.problem() {
                None => Status::SignedIn,
                Some(p) => Status::from_problem(p),
            },
        }
    }

    pub fn list(&self, path: &str) -> Result<Vec<Node>, Problem> {
        let o = self.call(&args_list(path), SMALL_TIMEOUT, &mut |_| {})?;
        parse_list(&o.stdout).map_err(Problem::Other)
    }

    /// None when the path doesn't exist.
    pub fn info(&self, path: &str) -> Result<Option<Node>, Problem> {
        match self.call(&args_info(path), SMALL_TIMEOUT, &mut |_| {}) {
            Ok(o) => Ok(parse_info(&o.stdout)),
            Err(Problem::NotFound(_)) => Ok(None),
            Err(p) => Err(p),
        }
    }

    /// Create `parent/name`; one that already exists is fine.
    pub fn create_folder(&self, parent: &str, name: &str) -> Result<(), Problem> {
        match self.call(&args_create_folder(parent, name), SMALL_TIMEOUT, &mut |_| {}) {
            Ok(_) | Err(Problem::Exists(_)) => Ok(()),
            Err(p) => Err(p),
        }
    }

    /// Upload; the summary even when items failed (the CLI exits 1 then).
    pub fn upload(&self, files: &[PathBuf], parent: &str, on_line: &mut dyn FnMut(Option<&str>)) -> Result<Summary, Problem> {
        transfer(self.run(&args_upload(files, parent), NO_TIMEOUT, on_line))
    }

    pub fn download(&self, remote: &[String], dir: &Path, on_line: &mut dyn FnMut(Option<&str>)) -> Result<Summary, Problem> {
        transfer(self.run(&args_download(remote, dir), NO_TIMEOUT, on_line))
    }

    /// A download that `cancel_transfer` can stop (the restore screen's, outside a job).
    pub fn download_cancelable(&self, remote: &[String], dir: &Path, on_line: &mut dyn FnMut(Option<&str>)) -> Result<Summary, Problem> {
        reset_transfer_cancel();
        let r = transfer(self.run_with(&args_download(remote, dir), NO_TIMEOUT, on_line, Some(&TRANSFER_CANCEL)));
        reset_transfer_cancel();
        r
    }

    pub fn trash(&self, paths: &[String]) -> Result<(), Problem> {
        self.call(&args_trash(paths), SMALL_TIMEOUT, &mut |_| {}).map(|_| ())
    }

    pub fn logout(&self) -> Result<(), Problem> {
        self.call(&args_logout(), SMALL_TIMEOUT, &mut |_| {}).map(|_| ())
    }
}

/// A transfer's result: the summary when there is one (failures inside it are the caller's to
/// judge), else what went wrong.
fn transfer(r: io::Result<Output>) -> Result<Summary, Problem> {
    let o = r.map_err(|e| spawn_problem(&e))?;
    match parse_summary(&o.stdout) {
        Some(s) if o.ok() || s.failed_items > 0 => Ok(s),
        _ => Err(o.problem().unwrap_or_else(|| Problem::Other("proton-drive printed no transfer summary".into()))),
    }
}

fn spawn_problem(e: &io::Error) -> Problem {
    if e.kind() == io::ErrorKind::NotFound { Problem::NotInstalled } else { Problem::Other(format!("Could not run proton-drive: {e}")) }
}

/// SIGTERM the group, SIGKILL it if it's still there 2 s later.
pub fn kill_group(pgid: i32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let group = Pid::from_raw(-pgid);
    if kill(group, Signal::SIGTERM).is_err() {
        return;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        if kill(group, None).is_err() {
            return;
        }
    }
    let _ = kill(group, Signal::SIGKILL);
}

// --- Signing in -----------------------------------------------------------------------------

/// The running `auth login`, killed on cancel and by `ctx::cleanup_temp_state` on the way out
/// (an orphan would keep polling Proton for ten minutes).
static LOGIN_PGID: std::sync::Mutex<Option<i32>> = std::sync::Mutex::new(None);

/// Stops a transfer running outside a job (the restore screen's download): Esc sets it.
static TRANSFER_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn cancel_transfer() {
    TRANSFER_CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn reset_transfer_cancel() {
    TRANSFER_CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
}

pub fn kill_login() {
    if let Some(pgid) = LOGIN_PGID.lock().unwrap().take() {
        kill_group(pgid);
    }
}

pub struct Login {
    child: std::process::Child,
    pgid: i32,
    lines: std::sync::mpsc::Receiver<String>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    err_reader: Option<std::thread::JoinHandle<()>>,
}

impl Cli {
    /// Start `auth login --json` as the user. kast never types into it or passes a password.
    pub fn start_login(&self) -> Result<Login, Problem> {
        let mut child = self.command(&args_login()).spawn().map_err(|e| spawn_problem(&e))?;
        let pgid = child.id() as i32;
        *LOGIN_PGID.lock().unwrap() = Some(pgid);
        let out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = stderr.clone();
        let err_reader = std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let clean = crate::ui::sanitize_log(&scrub(&line));
                crate::ctx::log_file_only("PROTON", &clean);
                let mut s = sink.lock().unwrap();
                s.push_str(&clean);
                s.push('\n');
            }
        });
        Ok(Login { child, pgid, lines, stderr, err_reader: Some(err_reader) })
    }
}

impl Login {
    /// Wait for the sign-in URL (the first `{"signInUrl":…}` line). Other lines are logged,
    /// scrubbed. Fails if the CLI ends first or prints a URL kast won't show.
    pub fn url(&mut self, timeout: Duration) -> Result<SignInUrl, Problem> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(line) => match parse_sign_in(&line) {
                    Some(Ok(url)) => return Ok(url),
                    Some(Err(e)) => {
                        self.kill();
                        return Err(Problem::Other(e));
                    }
                    None => crate::ctx::log_file_only("PROTON", &crate::ui::sanitize_log(&scrub(&line))),
                },
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.finish(Duration::from_secs(5)).err().unwrap_or(Problem::Other("proton-drive printed no sign-in address".into())));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                    self.kill();
                    return Err(Problem::TimedOut);
                }
                Err(_) => {}
            }
        }
    }

    /// Wait for the sign-in to complete (exit 0) or fail.
    pub fn finish(&mut self, timeout: Duration) -> Result<(), Problem> {
        let deadline = Instant::now() + timeout;
        let exit = loop {
            match self.child.try_wait() {
                Ok(Some(st)) => break st.code().map(Exit::Code).or(st.signal().map(Exit::Signal)).unwrap_or(Exit::Code(-1)),
                Ok(None) if Instant::now() >= deadline => {
                    self.kill();
                    break Exit::TimedOut;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(Problem::Other(e.to_string())),
            }
        };
        {
            let mut g = LOGIN_PGID.lock().unwrap();
            if *g == Some(self.pgid) {
                *g = None;
            }
        }
        if let Some(h) = self.err_reader.take() {
            let _ = h.join();
        }
        if exit == Exit::Code(0) {
            return Ok(());
        }
        // Whatever it printed on stdout may carry the URL: scrub before classifying
        let rest: Vec<String> = self.lines.try_iter().collect();
        let text = format!("{}\n{}", self.stderr.lock().unwrap(), scrub(&rest.join("\n")));
        Err(classify(&exit, &text))
    }

    pub fn kill(&mut self) {
        kill_group(self.pgid);
        let _ = self.child.wait();
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
        let mut g = LOGIN_PGID.lock().unwrap();
        if *g == Some(self.pgid) {
            *g = None;
        }
    }
}

// --- Sign-in status -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    SignedIn,
    SignedOut,
    KeyringUnavailable(String),
    Offline(String),
    NotAllowed(String),
    Other(String),
}

impl Status {
    pub fn from_problem(p: Problem) -> Status {
        match p {
            Problem::SignedOut => Status::SignedOut,
            Problem::Keyring(m) => Status::KeyringUnavailable(m),
            Problem::Offline(m) => Status::Offline(m),
            Problem::NotAllowed(m) => Status::NotAllowed(m),
            // The status check doesn't transfer anything: anything else is just "other"
            p => Status::Other(p.to_string()),
        }
    }

    /// The machine-readable name (`kast drive status --json`).
    pub fn key(&self) -> &'static str {
        match self {
            Status::SignedIn => "signed_in",
            Status::SignedOut => "signed_out",
            Status::KeyringUnavailable(_) => "keyring_unavailable",
            Status::Offline(_) => "offline",
            Status::NotAllowed(_) => "not_allowed",
            Status::Other(_) => "other",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::SignedIn | Status::SignedOut => None,
            Status::KeyringUnavailable(m) | Status::Offline(m) | Status::NotAllowed(m) | Status::Other(m) => Some(m),
        }
    }

    pub fn problem(&self) -> Option<Problem> {
        match self {
            Status::SignedIn => None,
            Status::SignedOut => Some(Problem::SignedOut),
            Status::KeyringUnavailable(m) => Some(Problem::Keyring(m.clone())),
            Status::Offline(m) => Some(Problem::Offline(m.clone())),
            Status::NotAllowed(m) => Some(Problem::NotAllowed(m.clone())),
            Status::Other(m) => Some(Problem::Other(m.clone())),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn user() -> UserEnv {
        UserEnv { user: "p3ter".into(), uid: 1000, home: "/home/p3ter".into(), dbus: "unix:path=/run/user/1000/bus".into(), ..Default::default() }
    }

    fn cli(root: bool, store: Option<&str>) -> Cli {
        Cli { bin: PathBuf::from("/home/p3ter/.local/bin/proton-drive"), root, user: user(), store: store.map(String::from) }
    }

    pub const FAKE: &str = include_str!("testdata/fake-proton-drive.sh");

    /// A fake `proton-drive` in its own temp dir, in `mode`, with an empty /my-files.
    pub fn fake(mode: &str) -> (tempfile::TempDir, Cli) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("proton-drive");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("mode"), mode).unwrap();
        std::fs::create_dir_all(dir.path().join("drive/my-files")).unwrap();
        let cli = Cli { bin, root: false, user: user(), store: None };
        (dir, cli)
    }

    pub fn set_mode(dir: &tempfile::TempDir, mode: &str) {
        std::fs::write(dir.path().join("mode"), mode).unwrap();
    }

    #[test]
    fn fake_cli_status_and_version() {
        let (dir, cli) = fake("ok");
        assert_eq!(cli.version(), Install::Found { path: cli.bin.clone(), version: Version(0, 9, 0), compat: Compat::Ok });
        assert_eq!(cli.status(), Status::SignedIn);
        set_mode(&dir, "signed_out");
        assert_eq!(cli.status(), Status::SignedOut);
        set_mode(&dir, "keyring");
        assert!(matches!(cli.status(), Status::KeyringUnavailable(m) if m.contains("secrets")));
        set_mode(&dir, "offline");
        assert!(matches!(cli.status(), Status::Offline(_)));
        std::fs::write(dir.path().join("version"), "0.8.1").unwrap();
        assert!(matches!(cli.version(), Install::Found { compat: Compat::TooOld, .. }));
        set_mode(&dir, "crash");
        assert_eq!(cli.version(), Install::Broken { path: cli.bin.clone(), problem: Problem::NeedsBaseline });
        let missing = Cli { bin: dir.path().join("nope"), ..cli };
        assert!(matches!(missing.status(), Status::Other(m) if m.contains("not installed")));
        assert_eq!(missing.list("/my-files"), Err(Problem::NotInstalled));
    }

    #[test]
    fn fake_cli_transfers() {
        let (dir, cli) = fake("ok");
        let local = dir.path().join("a.zip");
        std::fs::write(&local, b"zipdata").unwrap();
        assert_eq!(cli.info("/my-files/kast-backups").unwrap(), None);
        cli.create_folder("/my-files", "kast-backups").unwrap();
        cli.create_folder("/my-files", "kast-backups").unwrap();
        let s = cli.upload(std::slice::from_ref(&local), "/my-files/kast-backups", &mut |_| {}).unwrap();
        assert_eq!((s.transferred_items, s.failed_items, s.skipped_items), (1, 0, 0));
        // The same content again: skipped by the CLI
        assert_eq!(cli.upload(std::slice::from_ref(&local), "/my-files/kast-backups", &mut |_| {}).unwrap().skipped_items, 1);
        let n = cli.list("/my-files/kast-backups").unwrap();
        assert_eq!(n.iter().map(|n| (n.name.as_str(), n.size)).collect::<Vec<_>>(), [("a.zip", Some(7))]);
        assert_eq!(cli.info("/my-files/kast-backups/a.zip").unwrap().unwrap().size, Some(7));
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        cli.download(&["/my-files/kast-backups/a.zip".into()], &out, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(out.join("a.zip")).unwrap(), b"zipdata");
        cli.trash(&["/my-files/kast-backups/a.zip".into()]).unwrap();
        assert!(cli.list("/my-files/kast-backups").unwrap().is_empty());
        assert!(matches!(cli.trash(&["/my-files/nope".into()]), Err(Problem::NotFound(_))));
        set_mode(&dir, "upload_fail");
        let s = cli.upload(&[local], "/my-files/kast-backups", &mut |_| {}).unwrap();
        assert_eq!(s.failed_items, 1);
        set_mode(&dir, "signed_out");
        assert_eq!(cli.list("/my-files"), Err(Problem::SignedOut));
        cli.logout().unwrap();
        // Every call had stdin on /dev/null (the fake refuses a terminal) and asked for JSON
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.lines().filter(|l| l.starts_with("filesystem")).all(|l| l.contains("--json")));
    }

    #[test]
    fn fake_cli_stderr_is_scrubbed() {
        let (_dir, cli) = fake("ok");
        let mut lines = Vec::new();
        let mut cmd = args_login();
        cmd.truncate(3);
        let o = cli.run(&cmd, Duration::from_secs(20), &mut |l| lines.extend(l.map(String::from))).unwrap();
        assert!(o.ok());
        assert!(!o.stderr.contains("SECRETKEY") && o.stderr.contains(URL_HIDDEN));
        assert!(lines.iter().all(|l| !l.contains("SECRETKEY")));
        // stdout keeps it for parsing; only parse_sign_in reveals it
        let url = o.stdout.lines().find_map(parse_sign_in).unwrap().unwrap();
        assert!(url.reveal().ends_with("#key=SECRETKEY"));
    }

    #[test]
    fn fake_cli_sign_in_flow() {
        let (dir, cli) = fake("signed_out");
        let mut login = cli.start_login().unwrap();
        let url = login.url(Duration::from_secs(10)).unwrap();
        assert!(url.reveal().starts_with("https://account.proton.me/"));
        login.finish(Duration::from_secs(20)).unwrap();
        assert_eq!(cli.status(), Status::SignedIn);
        // A failed sign-in carries the CLI's message, never the URL
        set_mode(&dir, "login_fail");
        let mut login = cli.start_login().unwrap();
        login.url(Duration::from_secs(10)).unwrap();
        let err = login.finish(Duration::from_secs(20)).unwrap_err();
        assert_eq!(err, Problem::Other("Authentication timed out".into()));
        // Canceling kills it
        std::fs::write(dir.path().join("login_delay"), "30").unwrap();
        let mut login = cli.start_login().unwrap();
        login.url(Duration::from_secs(10)).unwrap();
        let t = Instant::now();
        drop(login);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fake_cli_timeout_kills_the_group() {
        let (dir, cli) = fake("ok");
        std::fs::write(dir.path().join("login_delay"), "30").unwrap();
        let t = Instant::now();
        let o = cli.run(&args_login(), Duration::from_millis(500), &mut |_| {}).unwrap();
        assert_eq!(o.exit, Exit::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn commands_run_as_the_user() {
        let l = cli(true, None).launch(&args_status());
        assert_eq!(l.program, "sudo");
        assert_eq!(&l.args[..3], &strs(&["-u", "p3ter", "env"]));
        assert!(l.args.contains(&"HOME=/home/p3ter".into()) && l.args.contains(&"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus".into()));
        assert!(l.args.ends_with(&strs(&["/home/p3ter/.local/bin/proton-drive", "filesystem", "info", "/my-files", "--json"])));
        assert!(!l.args.iter().any(|a| a.starts_with("PROTON_DRIVE_CREDENTIALS_STORE")));

        let l = cli(false, None).launch(&args_status());
        assert_eq!(l.program, "/home/p3ter/.local/bin/proton-drive");
        assert_eq!(l.args, strs(&["filesystem", "info", "/my-files", "--json"]));
    }

    #[test]
    fn pass_store_is_passed_on_every_call() {
        let l = cli(true, Some("pass")).launch(&args_logout());
        assert!(l.args.contains(&"PROTON_DRIVE_CREDENTIALS_STORE=pass".into()));
        let l = cli(false, Some("pass")).launch(&args_logout());
        assert!(l.env.contains(&("PROTON_DRIVE_CREDENTIALS_STORE".into(), "pass".into())));
        // Never the unsafe file store, whatever the settings say
        let l = cli(false, Some("unsafe_file")).launch(&args_logout());
        assert!(!l.env.iter().any(|(k, _)| k == "PROTON_DRIVE_CREDENTIALS_STORE"));
    }

    #[test]
    fn transfers_always_set_json_and_both_strategies() {
        let up = args_upload(&[PathBuf::from("/s/a.zip"), PathBuf::from("/s/a.zip.manifest.json")], "/my-files/kast-backups/host");
        let down = args_download(&["/my-files/kast-backups/host/a.zip".into()], Path::new("/home/p3ter/.cache/kast/x"));
        for v in [&up, &down] {
            assert!(v.contains(&"--json".into()));
            let pos = |f: &str| v.iter().position(|a| a == f).unwrap_or_else(|| panic!("{f} missing"));
            assert!(!v[pos("--file-conflict-strategy") + 1].starts_with("--"));
            assert!(!v[pos("--folder-conflict-strategy") + 1].starts_with("--"));
        }
        assert_eq!(up[2..5], strs(&["/s/a.zip", "/s/a.zip.manifest.json", "/my-files/kast-backups/host"]));
        assert!(!up.contains(&"replace".into()), "uploads never replace");
        assert!(up.contains(&"--skip-thumbnails".into()));
        assert_eq!(down[down.iter().position(|a| a == "--file-conflict-strategy").unwrap() + 1], "rename");
        assert_eq!(args_trash(&["/my-files/x".into()]), strs(&["filesystem", "trash", "/my-files/x", "--json"]));
    }

    #[test]
    fn versions() {
        let out = "Proton Drive CLI proton-drive-cli@0.9.0\nProton Drive SDK @protontech/drive-sdk@0.14.2\nYou are running the latest version.\n";
        assert_eq!(parse_version(out), Some(Version(0, 9, 0)));
        // What the real 0.9.0 prints
        let real = "Proton Drive CLI cli-drive@0.9.0+75612299\nProton Drive SDK js@0.22.2+75612299\nYou are running the latest version.\n";
        assert_eq!(parse_version(real), Some(Version(0, 9, 0)));
        assert_eq!(parse_version("Proton Drive SDK 1.2.3\nProton Drive CLI 0.10.1"), Some(Version(0, 10, 1)));
        assert_eq!(parse_version("nothing here"), None);
        assert_eq!(compat(Version(0, 9, 0)), Compat::Ok);
        assert_eq!(compat(Version(0, 12, 3)), Compat::Ok);
        assert_eq!(compat(Version(0, 8, 9)), Compat::TooOld);
        assert_eq!(compat(Version(1, 0, 0)), Compat::NewerMajor);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
    }

    #[test]
    fn where_to_look() {
        let c = candidates("/usr/local/sbin:/usr/bin:relative", "/home/p", None);
        let s: Vec<String> = c.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(s, ["/usr/local/sbin/proton-drive", "/usr/bin/proton-drive", "/home/p/.local/bin/proton-drive", "/usr/local/bin/proton-drive"]);
        assert_eq!(candidates("/usr/bin", "/home/p", Some("/tmp/fake")), vec![PathBuf::from("/tmp/fake")]);
        assert_eq!(cpu_build("processor : 0\nflags : fpu sse avx avx2 bmi2\n"), "linux/x64");
        assert_eq!(cpu_build("flags : fpu sse avx\nbugs : avx2\n"), "linux/x64-baseline");
    }

    fn c(code: i32, stderr: &str) -> Problem {
        classify(&Exit::Code(code), stderr)
    }

    #[test]
    fn classification() {
        assert_eq!(c(1, "You need to login first\n"), Problem::SignedOut);
        assert!(matches!(
            c(1, "Failed to load session from secrets (ensure you have secrets available, read the README for more information): Cannot autolaunch D-Bus without X11 $DISPLAY"),
            Problem::Keyring(m) if m.starts_with("Failed to load session")
        ));
        // The real 0.9.0's words without a keyring, and without pass
        assert!(matches!(
            c(1, "Failed to load session from pass (ensure pass is installed and gpg-agent can decrypt): Executable not found in $PATH: \"pass\""),
            Problem::Keyring(_)
        ));
        assert!(matches!(c(1, "The name org.freedesktop.secrets was not provided by any .service files"), Problem::Keyring(_)));
        assert_eq!(c(1, "Error: 401 Unauthorized"), Problem::SignedOut);
        assert_eq!(c(1, "Invalid refresh token"), Problem::SignedOut);
        assert!(matches!(c(1, "RateLimitedError: Too many server requests, please try again later"), Problem::RateLimited(_)));
        assert!(matches!(c(1, "ConnectionError: fetch failed"), Problem::Offline(_)));
        assert!(matches!(c(1, "TypeError: getaddrinfo ENOTFOUND drive-api.proton.me"), Problem::Offline(_)));
        assert!(matches!(c(1, "Node not found: kast-backups"), Problem::NotFound(_)));
        assert!(matches!(c(1, "A file or folder with that name already exists"), Problem::Exists(_)));
        assert!(matches!(c(1, "ValidationError: Insufficient storage quota"), Problem::QuotaFull(_)));
        assert!(matches!(c(1, "This feature is not available on your plan"), Problem::NotAllowed(_)));
        assert_eq!(classify(&Exit::Signal(4), ""), Problem::NeedsBaseline);
        assert_eq!(c(132, "sudo: unable to execute: Illegal instruction"), Problem::NeedsBaseline);
        assert_eq!(classify(&Exit::TimedOut, "You need to login first"), Problem::TimedOut);
        assert_eq!(c(1, "Opening it\n  Something odd  \n    at foo (x.ts:1)\n"), Problem::Other("Something odd".into()));
        assert_eq!(c(2, ""), Problem::Other("proton-drive exited with code 2".into()));
    }

    #[test]
    fn status_from_problems() {
        assert_eq!(Status::from_problem(Problem::SignedOut), Status::SignedOut);
        assert_eq!(Status::from_problem(Problem::Keyring("x".into())), Status::KeyringUnavailable("x".into()));
        assert_eq!(Status::from_problem(Problem::NeedsBaseline).key(), "other");
    }

    #[test]
    fn summaries() {
        let ok = "{\"transferredItems\":2,\"transferredBytes\":1234,\"skippedItems\":0,\"failedItems\":0,\"failures\":[]}\n";
        assert_eq!(parse_summary(ok), Some(Summary { transferred_items: 2, transferred_bytes: 1234, ..Default::default() }));
        let skipped = "noise\n{\"transferredItems\":1,\"transferredBytes\":10,\"skippedItems\":1,\"failedItems\":0,\"failures\":[]}";
        assert_eq!(parse_summary(skipped).unwrap().skipped_items, 1);
        let failed = r#"{"transferredItems":0,"transferredBytes":0,"skippedItems":0,"failedItems":1,"failures":[{"name":"a.zip","error":"ValidationError: Insufficient storage quota"}]}"#;
        let s = parse_summary(failed).unwrap();
        assert_eq!(s.failures, vec![Failure { name: "a.zip".into(), node_uid: None, error: "ValidationError: Insufficient storage quota".into() }]);
        assert_eq!(parse_summary("garbage\n{not json}\n"), None);
        assert_eq!(parse_summary("{\"something\":1}"), None);
        assert_eq!(parse_summary(""), None);
    }

    #[test]
    fn transfer_outcomes() {
        let out = |code: i32, stdout: &str, stderr: &str| Ok(Output { exit: Exit::Code(code), stdout: stdout.into(), stderr: stderr.into() });
        let failed = r#"{"transferredItems":0,"skippedItems":0,"failedItems":1,"failures":[]}"#;
        assert_eq!(transfer(out(1, failed, "1 item(s) failed to upload")).unwrap().failed_items, 1);
        assert_eq!(transfer(out(1, "", "You need to login first")), Err(Problem::SignedOut));
        assert!(matches!(transfer(out(0, "garbage", "")), Err(Problem::Other(m)) if m.contains("no transfer summary")));
    }

    #[test]
    fn listings_are_read_defensively() {
        let list = r#"[
{"uid":"v~1","name":{"ok":true,"value":"host-a"},"type":"folder","creationTime":"2026-10-01T10:00:00.000Z"},
{"uid":"v~2","name":{"ok":true,"value":"all_backup_2026-10-01_100000.zip"},"type":"file","activeRevision":{"claimedSize":2048,"claimedModificationTime":"2026-10-01T10:00:00.000Z","storageSize":2100}},
{"uid":"v~3","name":{"ok":false,"error":{"name":"x","error":"bad"}},"type":"file"},
{"nodeUid":"v~4","name":"renamed.zip","nodeType":"File","size":7,"modifiedTime":"2026-10-02T00:00:00Z"},
{"uid":"v~5","name":{"ok":true,"value":"y"},"type":"album"},
42
]
"#;
        let n = parse_list(list).unwrap();
        assert_eq!(n.len(), 4);
        assert_eq!(n[0], Node { name: "host-a".into(), uid: "v~1".into(), kind: Kind::Folder, size: None, modified: Some("2026-10-01T10:00:00.000Z".into()) });
        assert_eq!((n[1].size, n[1].kind), (Some(2048), Kind::File));
        assert_eq!(
            n[2],
            Node { name: "renamed.zip".into(), uid: "v~4".into(), kind: Kind::File, size: Some(7), modified: Some("2026-10-02T00:00:00Z".into()) }
        );
        assert_eq!(n[3].kind, Kind::Other);
        assert_eq!(parse_list("[\n]\n").unwrap(), vec![]);
        assert!(parse_list("{\"a\":1}").is_err());
        assert!(parse_list("You need to login first").is_err());
        let info =
            parse_info("{\"uid\":\"u\",\"name\":{\"ok\":true,\"value\":\"a.zip\"},\"type\":\"file\",\"activeRevision\":{\"storageSize\":99}}\n").unwrap();
        assert_eq!(info.size, Some(99));
    }

    #[test]
    fn sign_in_url_extraction_and_validation() {
        let line = r#"{"signInUrl":"https://account.proton.me/login/sso?x=1#key=c2VjcmV0"}"#;
        let url = parse_sign_in(line).unwrap().unwrap();
        assert_eq!(url.reveal(), "https://account.proton.me/login/sso?x=1#key=c2VjcmV0");
        assert_eq!(format!("{url} {url:?}"), format!("{URL_HIDDEN} {URL_HIDDEN}"));
        assert!(parse_sign_in("Opening browser…").is_none());
        assert!(parse_sign_in("{\"other\":1}").is_none());
        for bad in [
            "http://account.proton.me/x",
            "https://proton.me.evil.com/x",
            "https://evil.com/proton.me",
            "https://user@proton.me/x",
            "https://proton.me:444/x",
            "https://evilproton.me/x",
        ] {
            let r = parse_sign_in(&format!("{{\"signInUrl\":\"{bad}\"}}")).unwrap();
            let err = r.unwrap_err();
            assert!(!err.contains(bad), "the error must not contain the URL");
        }
        assert!(valid_sign_in_url("https://proton.me/a") && valid_sign_in_url("https://drive.proton.me/#k"));
    }

    #[test]
    fn sign_in_url_is_scrubbed_from_logs() {
        let secret = "https://account.proton.me/sso/#selector=abc&key=c2VjcmV0";
        let text = format!("{{\"signInUrl\":\"{secret}\"}}\nOpen following URL manually: {secret}\nfetch https://drive-api.proton.me/core/v4/x failed\n");
        let s = scrub(&text);
        assert!(!s.contains("c2VjcmV0") && !s.contains("selector"));
        assert_eq!(s.matches(URL_HIDDEN).count(), 3);
        assert!(s.contains("\"signInUrl\":\"<sign-in URL hidden>\""));
        // Escaped JSON and odd casing too
        assert!(!scrub(r#"{"signInUrl": "https:\/\/account.proton.me\/x#k"}"#).contains("#k"));
        assert!(!scrub("HTTPS://Account.Proton.ME/x#k").contains("#k"));
        // The download page and unrelated URLs stay
        assert_eq!(scrub(&format!("see {DOWNLOAD_PAGE}")), format!("see {DOWNLOAD_PAGE}"));
        assert_eq!(scrub("https://example.com/a#b"), "https://example.com/a#b");
        // And a failing login's output, classified, never carries it either
        let p = classify(&Exit::Code(1), &format!("Authentication timed out\n{secret}"));
        assert!(!format!("{p} {p:?}").contains("c2VjcmV0"));
    }

    #[test]
    fn remote_paths() {
        assert_eq!(join("/my-files/kast-backups/", &["host", "/a.zip"]), "/my-files/kast-backups/host/a.zip");
    }
}
