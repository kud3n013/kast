//! Packages archrepo publishes that `packages.toml` doesn't mention.
//!
//! archrepo's signed pacman database lists exactly the packages that built and were
//! published. kast downloads it (with its signature), checks the signature against the key
//! it ships, and adds the packages the file leaves out. Everything here is best effort: any
//! failure means nothing is added and the list stays as the file gave it.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};

use super::{GROUPS, Parsed, Pkg, Reason, TEXT_MAX, check_pkg, default_group_name};
use crate::setup::{REPO_KEY, REPO_KEY_FPR, REPO_URL};

/// Where the database is published; names the host in error messages.
pub const HOST: &str = "github.com";
/// Download caps: the database is a few KiB per dozen packages, the signature ~120 bytes.
pub const DB_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const SIG_MAX_BYTES: usize = 16 * 1024;
/// What the database may expand to, how many packages it may list, and one `desc`'s size.
const UNPACKED_MAX: u64 = 32 * 1024 * 1024;
const ENTRIES_MAX: usize = 5000;
const DESC_MAX: u64 = 256 * 1024;

pub fn db_url() -> String {
    format!("{REPO_URL}/archrepo.db")
}

pub fn sig_url() -> String {
    format!("{REPO_URL}/archrepo.db.sig")
}

/// One package from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub description: Option<String>,
    /// Dependency names, without version operators
    pub depends: Vec<String>,
}

/// Downloads `url` to at most `max` bytes.
pub type Fetch<'a> = &'a (dyn Fn(&str, usize) -> Result<Vec<u8>, Reason> + Sync);
/// Checks `signature` over `data`.
pub type Verify<'a> = &'a (dyn Fn(&[u8], &[u8]) -> Result<(), String> + Sync);

fn why(r: Reason) -> String {
    match r {
        Reason::Network(d) => format!("could not reach {HOST} ({d})"),
        Reason::Tls(d) => format!("TLS error reaching {HOST} ({d})"),
        Reason::TooLarge => "the database is too large".into(),
        other => other.to_string(),
    }
}

/// Download, verify and read archrepo's database. The error says why nothing was found.
pub fn fetch(fetch: Fetch, verify: Verify) -> Result<Vec<Found>, String> {
    let (db, sig) = std::thread::scope(|s| {
        let sig = s.spawn(|| fetch(&sig_url(), SIG_MAX_BYTES));
        let db = fetch(&db_url(), DB_MAX_BYTES);
        (db, sig.join().unwrap_or(Err(Reason::Download("download thread panicked".into()))))
    });
    let db = db.map_err(why)?;
    let sig = sig.map_err(|r| format!("signature: {}", why(r)))?;
    verify(&db, &sig).map_err(|e| format!("signature check failed: {e}"))?;
    read_db(&db)
}

/// `gpgv` over a temporary keyring holding only archrepo's key, so the user's keyrings are
/// neither read nor changed. Accepts only a good signature by that key's fingerprint.
pub fn verify_gpg(db: &[u8], sig: &[u8]) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| format!("no temporary directory ({e})"))?;
    let (key, ring, db_file, sig_file) =
        (dir.path().join("key.asc"), dir.path().join("ring.gpg"), dir.path().join("archrepo.db"), dir.path().join("archrepo.db.sig"));
    for (path, bytes) in [(&key, REPO_KEY.as_bytes()), (&db_file, db), (&sig_file, sig)] {
        std::fs::write(path, bytes).map_err(|e| format!("could not write {} ({e})", path.display()))?;
    }
    let run = |cmd: &str, args: &[&std::ffi::OsStr]| {
        Command::new(cmd).args(args).stdin(Stdio::null()).output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{cmd} not found"),
            _ => format!("could not run {cmd} ({e})"),
        })
    };
    // gpg --dearmor needs no agent and no homedir content; gpgv never starts an agent
    let armored = std::fs::File::open(&key).map_err(|e| e.to_string())?;
    let dearmor = Command::new("gpg")
        .args(["--batch", "--no-tty", "--dearmor", "--output"])
        .arg(&ring)
        .stdin(armored)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("could not run gpg ({e})"))?;
    if !dearmor.success() {
        return Err("could not prepare the archrepo key".into());
    }
    let out = run("gpgv", &["--keyring".as_ref(), ring.as_os_str(), "--status-fd".as_ref(), "1".as_ref(), sig_file.as_os_str(), db_file.as_os_str()])?;
    let status = String::from_utf8_lossy(&out.stdout);
    let good = status
        .lines()
        .filter_map(|l| l.strip_prefix("[GNUPG:] VALIDSIG "))
        .any(|l| l.split_whitespace().last().is_some_and(|f| f.eq_ignore_ascii_case(REPO_KEY_FPR)));
    if good { Ok(()) } else { Err("not signed by the archrepo key".into()) }
}

/// The packages in a gzip-compressed pacman database (a tar of `<name>-<ver>/desc`).
pub fn read_db(db: &[u8]) -> Result<Vec<Found>, String> {
    if db.len() < 2 || db[..2] != [0x1f, 0x8b] {
        return Err("the database isn't gzip-compressed".into());
    }
    let unpacked = flate2::read::GzDecoder::new(db).take(UNPACKED_MAX);
    let mut found = Vec::new();
    let mut entries = 0;
    for entry in tar::Archive::new(unpacked).entries().map_err(|e| format!("unreadable database ({e})"))? {
        let mut entry = entry.map_err(|e| format!("unreadable database ({e})"))?;
        entries += 1;
        if entries > ENTRIES_MAX {
            return Err("the database lists too many entries".into());
        }
        let is_desc = entry.path().is_ok_and(|p| p.file_name().is_some_and(|f| f == "desc") && p.components().count() == 2);
        if !is_desc {
            continue;
        }
        let mut text = String::new();
        if entry.by_ref().take(DESC_MAX).read_to_string(&mut text).is_err() {
            continue;
        }
        if let Some(f) = parse_desc(&text) {
            found.push(f);
        }
    }
    if found.is_empty() {
        return Err("the database lists no packages".into());
    }
    Ok(found)
}

/// `%NAME%`, `%DESC%` and `%DEPENDS%` from one `desc` file.
pub fn parse_desc(text: &str) -> Option<Found> {
    let section = |key: &str| -> Vec<&str> {
        let mut lines = text.lines().skip_while(|l| l.trim() != key).skip(1);
        let mut out = Vec::new();
        for l in lines.by_ref() {
            if l.trim().is_empty() {
                break;
            }
            out.push(l.trim());
        }
        out
    };
    let name = section("%NAME%").first()?.to_string();
    let description = section("%DESC%").first().map(|d| d.chars().filter(|c| !c.is_control()).take(TEXT_MAX).collect::<String>()).filter(|d| !d.is_empty());
    let depends = section("%DEPENDS%")
        .into_iter()
        // "zotero>=7" names `zotero`
        .map(|d| d.split(['<', '>', '=']).next().unwrap_or("").trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    Some(Found { name, description, depends })
}

/// Add `found` to `list`: what `packages.toml` doesn't list and doesn't exclude. A package
/// depending on a listed one becomes an add-on of it, any other goes to the kud3n group.
/// Returns how many were added; what was skipped is in `list.warnings`.
pub fn merge(list: &mut Parsed, mut found: Vec<Found>) -> usize {
    if !list.archrepo.auto {
        return 0;
    }
    let bare = |n: &str| n.split_once('/').map_or(n.to_string(), |(_, b)| b.to_string());
    // Names the file already accounts for, including what it names as a fallback or such
    let mut taken: HashSet<String> = HashSet::new();
    for p in list.packages() {
        taken.insert(p.bare().to_string());
        taken.extend([&p.fallback, &p.replaces, &p.requires].into_iter().flatten().map(|n| bare(n)));
    }
    let excluded: HashSet<String> = list.archrepo.exclude.iter().map(|n| bare(n)).collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));

    // Parents are looked up in the file's own list (kud3n's group first), never among
    // the packages added here
    let parents: Vec<(&str, &str)> = {
        let mut v: Vec<(&str, &str)> = list.groups.iter().filter(|g| g.id == "kud3n").flat_map(|g| &g.packages).map(|p| (p.bare(), p.name.as_str())).collect();
        v.extend(list.groups.iter().filter(|g| g.id != "kud3n").flat_map(|g| &g.packages).map(|p| (p.bare(), p.name.as_str())));
        v
    };

    let mut added: Vec<(&'static str, Pkg)> = Vec::new();
    let mut warnings = Vec::new();
    for f in found {
        if taken.contains(&f.name) || excluded.contains(&f.name) {
            continue;
        }
        let parent = f.depends.iter().filter_map(|d| parents.iter().position(|(b, _)| b == d)).min().map(|i| parents[i].1.to_string());
        let pkg = Pkg {
            name: format!("archrepo/{}", f.name),
            description: f.description,
            fallback: None,
            replaces: None,
            requires: parent.clone(),
            discovered: true,
        };
        // The same rules a name from the file has to meet; one bad package never fails the list
        if f.name.contains('/') {
            warnings.push(format!("archrepo: skipped {:?}: not a package name", f.name.chars().take(40).collect::<String>()));
            continue;
        }
        if let Err(e) = check_pkg(&pkg) {
            warnings.push(format!("archrepo: skipped {}: {e}", f.name.chars().take(40).collect::<String>()));
            continue;
        }
        taken.insert(f.name);
        added.push((if parent.is_some() { "addons" } else { "kud3n" }, pkg));
    }

    let count = added.len();
    for (id, pkg) in added {
        let at = match list.groups.iter().position(|g| g.id == id) {
            Some(i) => i,
            None => {
                // The file has no such group: make it, in install order
                let rank = |g: &str| GROUPS.iter().position(|k| *k == g);
                let i = list.groups.iter().position(|g| rank(&g.id) > rank(id)).unwrap_or(list.groups.len());
                list.groups.insert(i, super::Group { id: id.to_string(), name: default_group_name(id).to_string(), packages: Vec::new() });
                i
            }
        };
        list.groups[at].packages.push(pkg);
    }
    list.warnings.extend(warnings);
    count
}

#[cfg(test)]
mod tests {
    use super::super::{PkgList, Source, add_discovered, builtin, parse};
    use super::*;

    /// A real database and its signature, as archrepo published them.
    const DB: &[u8] = include_bytes!("../testdata/archrepo.db");
    const SIG: &[u8] = include_bytes!("../testdata/archrepo.db.sig");

    fn found(name: &str, depends: &[&str]) -> Found {
        Found { name: name.into(), description: Some(format!("The {name} package")), depends: depends.iter().map(|d| d.to_string()).collect() }
    }

    /// A gzip-compressed database holding these `desc` bodies.
    fn db_of(descs: &[(&str, String)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (dir, body) in descs {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, format!("{dir}/desc"), body.as_bytes()).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
        gz.finish().unwrap()
    }

    fn names(l: &Parsed, group: &str) -> Vec<String> {
        l.groups.iter().find(|g| g.id == group).map(|g| g.packages.iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
    }

    #[test]
    fn the_published_database_is_read() {
        let found = read_db(DB).unwrap();
        let get = |n: &str| found.iter().find(|f| f.name == n).unwrap_or_else(|| panic!("{n} in the database"));
        assert!(found.len() > 20);
        assert_eq!(get("sine").description.as_deref().map(|d| d.is_empty()), Some(false));
        assert!(get("millennium").depends.contains(&"steam".to_string()));
        assert!(get("zotero-ocr").depends.iter().any(|d| d == "zotero"), "{:?}", get("zotero-ocr").depends);
    }

    #[test]
    fn desc_fields_are_read_and_versions_stripped() {
        let f = parse_desc("%NAME%\nfoo\n\n%DESC%\nA foo\u{7}\n\n%DEPENDS%\nzotero>=7\nglibc\nlibfoo.so=1-64\n\n%OPTDEPENDS%\nbar: nice\n").unwrap();
        assert_eq!(f.name, "foo");
        assert_eq!(f.description.as_deref(), Some("A foo"), "control characters dropped");
        assert_eq!(f.depends, ["zotero", "glibc", "libfoo.so"]);
        assert!(parse_desc("%DESC%\nno name\n").is_none());
    }

    #[test]
    fn databases_that_are_not_gzip_or_empty_are_refused() {
        assert!(read_db(b"plain text").unwrap_err().contains("gzip"));
        assert!(read_db(&db_of(&[])).unwrap_err().contains("no packages"));
        assert!(read_db(&db_of(&[("x-1", "no sections".into())])).unwrap_err().contains("no packages"));
    }

    #[test]
    fn a_new_package_goes_to_kud3n_and_one_with_a_listed_dependency_is_an_add_on() {
        let mut l = builtin();
        let before = l.count();
        let n = merge(
            &mut l,
            vec![
                found("zotero-newplugin", &["zotero", "glibc"]),
                found("brandnew", &["glibc"]),
                found("another", &[]),
                // Both git (official) and howdy-next (kud3n) are listed: kud3n's wins
                found("howdy-theme", &["git", "howdy-next"]),
            ],
        );
        assert_eq!(n, 4);
        assert_eq!(l.count(), before + 4);
        // After what the file lists, sorted by name among the new ones
        let kud3n = names(&l, "kud3n");
        assert_eq!(&kud3n[kud3n.len() - 2..], ["archrepo/another", "archrepo/brandnew"]);
        let addons = l.groups.iter().find(|g| g.id == "addons").unwrap();
        let new: Vec<_> = addons.packages.iter().filter(|p| p.discovered).map(|p| (p.name.as_str(), p.requires.as_deref(), p.description.as_deref())).collect();
        assert_eq!(
            new,
            [
                ("archrepo/howdy-theme", Some("howdy-next"), Some("The howdy-theme package")),
                ("archrepo/zotero-newplugin", Some("zotero"), Some("The zotero-newplugin package"))
            ]
        );
        assert_eq!(addons.packages.iter().filter(|p| !p.discovered).count(), 7, "the file's own entries come first and are kept");
        assert!(addons.packages[..7].iter().all(|p| !p.discovered));
        assert_eq!(l.discovered(), 4);
    }

    #[test]
    fn a_dependency_on_a_prefixed_entry_keeps_the_entry_name() {
        let mut l = builtin();
        merge(&mut l, vec![found("freedownloadmanager-theme", &["freedownloadmanager"])]);
        let p = l.packages().find(|p| p.discovered).unwrap();
        assert_eq!(p.requires.as_deref(), Some("archrepo/freedownloadmanager"));
        // The result is a list `parse` accepts: every requires is in the list
        let again = l.packages().all(|p| p.requires.as_ref().is_none_or(|r| l.contains(r)));
        assert!(again);
    }

    #[test]
    fn listed_names_are_not_added_again_in_any_spelling() {
        let mut l = builtin();
        let before = l.count();
        let n = merge(
            &mut l,
            vec![
                found("sine", &[]),                // listed as archrepo/sine
                found("zotero", &[]),              // listed bare
                found("vesktop", &[]),             // a fallback
                found("sine-bin", &[]),            // a `replaces`
                found("freedownloadmanager", &[]), // listed with the prefix
                found("fresh", &[]),
                found("fresh", &[]), // twice in the database
            ],
        );
        assert_eq!(n, 1);
        assert_eq!(l.count(), before + 1);
    }

    #[test]
    fn exclude_keeps_packages_out_but_not_ones_the_file_lists() {
        let text = format!("{}\n[archrepo]\nexclude = [\"nope\", \"archrepo/also-nope\", \"zotero\"]\n", crate::pkglist::BUILTIN_TEXT);
        let mut l = parse(&text).unwrap();
        assert!(l.archrepo.auto);
        let n = merge(&mut l, vec![found("nope", &[]), found("also-nope", &[]), found("fine", &[])]);
        assert_eq!(n, 1);
        assert!(l.contains("archrepo/fine") && !l.contains("archrepo/nope"));
        assert!(l.contains("zotero"), "an explicit entry wins over exclude");
    }

    #[test]
    fn auto_false_adds_nothing() {
        let text = format!("{}\n[archrepo]\nauto = false\n", crate::pkglist::BUILTIN_TEXT);
        let mut pl = PkgList { source: Source::Online { fetched: "04:39".into() }, list: parse(&text).unwrap(), discovery_error: None };
        let before = pl.list.clone();
        add_discovered(&mut pl, Ok(vec![found("fine", &[])]));
        assert_eq!(pl.list, before);
        assert_eq!(pl.discovery_error, None);
    }

    #[test]
    fn the_archrepo_table_is_checked_and_optional() {
        assert!(parse(crate::pkglist::BUILTIN_TEXT).unwrap().archrepo.auto);
        let bad = format!("{}\n[archrepo]\nexclude = [\"Bad Name\"]\n", crate::pkglist::BUILTIN_TEXT);
        assert!(parse(&bad).unwrap_err().to_string().contains("[archrepo] exclude"));
    }

    #[test]
    fn a_name_that_breaks_a_rule_is_dropped_with_a_warning() {
        let mut l = builtin();
        let before = l.count();
        let n = merge(
            &mut l,
            vec![
                found("Upper_Case", &[]),
                found("-leading-dash", &[]),
                found("has space", &[]),
                found("a/b", &[]),
                found("good", &[]),
                found(&"x".repeat(150), &[]),
            ],
        );
        assert_eq!(n, 1);
        assert_eq!(l.count(), before + 1);
        assert_eq!(l.warnings.len(), 5, "{:?}", l.warnings);
        assert!(l.warnings.iter().all(|w| w.starts_with("archrepo: skipped ")));
        assert!(l.contains("archrepo/good"));
    }

    #[test]
    fn a_missing_group_is_made_in_install_order() {
        let mut l = parse("schema = 1\n[[group]]\nid = \"addons\"\npackages = [{ name = \"p\", requires = \"q\" }, { name = \"q\" }]\n[[group]]\nid = \"official\"\npackages = [{ name = \"base\" }]\n").unwrap();
        merge(&mut l, vec![found("fresh", &[])]);
        assert_eq!(l.groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), ["official", "kud3n", "addons"]);
        assert_eq!(l.groups[1].name, "kud3n's repository");
    }

    #[test]
    fn discovered_packages_never_come_from_the_aur() {
        let mut l = builtin();
        merge(&mut l, vec![found("fresh", &[])]);
        // setup builds from the AUR only for the built-in list's names
        assert!(!builtin().allows_aur("archrepo/fresh"));
        assert!(!builtin().allows_aur("fresh"));
        assert_eq!(l.packages().find(|p| p.discovered).unwrap().name, "archrepo/fresh", "installed from archrepo, whatever other repos carry");
    }

    #[test]
    fn a_file_cannot_mark_its_own_packages_as_discovered() {
        let l = parse("schema = 1\n[[group]]\nid = \"official\"\npackages = [{ name = \"a\", discovered = true }]\n").unwrap();
        assert_eq!(l.discovered(), 0);
    }

    #[test]
    fn download_and_signature_failures_leave_the_list_alone() {
        let good = |url: &str, _max: usize| Ok(if url.ends_with(".sig") { SIG.to_vec() } else { DB.to_vec() });
        let accept = |_: &[u8], _: &[u8]| Ok(());
        assert!(fetch(&good, &accept).unwrap().len() > 20);

        let offline = |_: &str, _: usize| Err(Reason::Network("timeout".into()));
        assert_eq!(fetch(&offline, &accept).unwrap_err(), "could not reach github.com (timeout)");
        let no_sig = |url: &str, _: usize| if url.ends_with(".sig") { Err(Reason::Download("HTTP 404".into())) } else { Ok(DB.to_vec()) };
        assert_eq!(fetch(&no_sig, &accept).unwrap_err(), "signature: download failed (HTTP 404)");
        let reject = |_: &[u8], _: &[u8]| Err("not signed by the archrepo key".to_string());
        let err = fetch(&good, &reject).unwrap_err();
        assert_eq!(err, "signature check failed: not signed by the archrepo key");

        // Nothing is merged, and the reason is kept for the source line
        let base = PkgList { source: Source::Online { fetched: "04:39".into() }, list: builtin(), discovery_error: None };
        let mut pl = base.clone();
        add_discovered(&mut pl, Err(err));
        assert_eq!(pl.list, base.list);
        assert_eq!(pl.discovery_error.as_deref(), Some("signature check failed: not signed by the archrepo key"));
        assert!(pl.source_line().ends_with(" · 36 packages · archrepo: signature check failed: not signed by the archrepo key"));
        // A bad database (signed, but unreadable) is the same
        let junk = |url: &str, _: usize| Ok(if url.ends_with(".sig") { SIG.to_vec() } else { b"junk".to_vec() });
        assert!(fetch(&junk, &accept).is_err());
    }

    #[test]
    fn the_source_line_counts_what_was_added() {
        let mut pl = PkgList { source: Source::Online { fetched: "04:39".into() }, list: builtin(), discovery_error: None };
        assert!(pl.source_line().ends_with(" · 36 packages"));
        add_discovered(&mut pl, Ok(vec![found("a", &[]), found("b", &[])]));
        assert_eq!(pl.source_line(), "Online · kud3n013/kast@main · fetched 04:39 · 36 packages + 2 from archrepo");
        assert!(pl.diff().is_empty(), "discovered packages aren't edits of the file");
    }

    #[test]
    fn the_list_marks_what_was_found_in_the_database() {
        let mut pl = PkgList { source: Source::Online { fetched: "04:39".into() }, list: builtin(), discovery_error: None };
        add_discovered(&mut pl, Ok(vec![found("fresh", &[]), found("zotero-plugin", &["zotero"])]));
        let text = crate::pkglist::list_text(&pl, None);
        assert!(text.contains("36 packages + 2 from archrepo"));
        let line =
            |name: &str| text.lines().find(|l| l.contains(name)).unwrap_or_else(|| panic!("{name} in {text}")).split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(line("archrepo/fresh"), "archrepo/fresh from archrepo, not in packages.toml");
        assert_eq!(line("archrepo/zotero-plugin"), "archrepo/zotero-plugin needs zotero · from archrepo, not in packages.toml");
        assert!(!text.contains("+ archrepo/fresh"), "not shown as an edit of the file");
        let j = crate::pkglist::list_json(&pl, None);
        assert_eq!(j["discovered"], 2);
        assert_eq!(j["groups"][2]["packages"].as_array().unwrap().last().unwrap()["discovered"], true);
    }

    fn have(cmd: &str) -> bool {
        Command::new(cmd).arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
    }

    #[test]
    fn the_real_signature_checks_with_gpgv() {
        if !have("gpg") || !have("gpgv") {
            eprintln!("gpg/gpgv not installed: skipped");
            return;
        }
        assert_eq!(verify_gpg(DB, SIG), Ok(()));
        let mut tampered = DB.to_vec();
        tampered[100] ^= 0xff;
        assert!(verify_gpg(&tampered, SIG).is_err());
        assert!(verify_gpg(DB, b"not a signature").is_err());
        assert!(verify_gpg(DB, &[]).is_err());
    }
}
