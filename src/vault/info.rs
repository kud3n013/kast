use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::ctx::reported;
use crate::ui::{C_BOLD, C_CYAN, C_RESET};

use super::archive::read_zip_manifest;
use super::{format_bytes, log_error};

/// jq-style `.a.b // default`: missing, null or false fall back; non-strings print as JSON.
pub fn jq_str(v: &Value, path: &[&str], default: &str) -> String {
    let mut cur = v;
    for key in path {
        match cur.get(key) {
            Some(next) => cur = next,
            None => return default.into(),
        }
    }
    match cur {
        Value::Null | Value::Bool(false) => default.into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What a backup archive's manifest says, ready to print or show.
#[derive(Debug, Clone, PartialEq)]
pub struct InfoReport {
    pub file: String,
    pub size: String,
    pub created: String,
    pub host: String,
    pub user: String,
    pub target: String,
    pub media: String,
    /// Keys of `apps`, in manifest order
    pub apps: Vec<InfoApp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InfoApp {
    /// Manifest key (beeper, kde, ...)
    pub key: String,
    /// Display name (BEEPER, KDE, ...)
    pub name: String,
    /// (label padded to 13 columns, value)
    pub fields: Vec<(String, String)>,
}

/// Read the manifest of `zip`; the error is the message `kast info` prints.
pub fn read_info(zip: &Path) -> std::result::Result<InfoReport, String> {
    if !zip.is_file() {
        return Err(format!("Archive file not found: {}", zip.display()));
    }
    let Some(raw) = read_zip_manifest(zip) else {
        return Err(format!("No 'backup_manifest.json' found inside {}", zip.display()));
    };
    let m: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    Ok(info_from_manifest(zip.display().to_string(), format_bytes(zip), &m))
}

/// What a manifest says (from an archive, or a Proton Drive sidecar's copy of it).
pub fn info_from_manifest(file: String, size: String, m: &Value) -> InfoReport {
    let media = m.get("include_media").map_or("Unknown".to_string(), |v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let mut apps = Vec::new();
    for key in m.get("apps").and_then(Value::as_object).map(|a| a.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let name: String = key.to_uppercase().chars().filter(|c| *c != ' ' && *c != '-').collect();
        let field = |f: &str, d: &str| jq_str(m, &["apps", &key, f], d);
        let mut fields: Vec<(String, String)> = Vec::new();
        let source = |fields: &mut Vec<(String, String)>, label: &str, f: &str| {
            let v = field(f, "");
            if !v.is_empty() {
                fields.push((label.into(), v));
            }
        };
        match name.as_str() {
            "BEEPER" | "ZALO" | "ANTIGRAVITY" => {
                let default = match name.as_str() {
                    "BEEPER" => "BeeperTexts",
                    "ZALO" => "ZaloData",
                    _ => "Antigravity",
                };
                fields.push(("Folder:      ".into(), format!("~/.config/{}", field("folder", default))));
                source(&mut fields, "Source:      ", "config_path");
            }
            "VESKTOP" | "ZEN" | "OBSIDIAN" | "FCITX5" | "HYPR" | "FASTFETCH" => {
                fields.push(("Folder:      ".into(), format!("~/.config/{key}")));
                source(&mut fields, "Source:      ", "config_path");
            }
            "SCRIPTS" => {
                fields.push(("Folder:      ".into(), "~/.scripts".into()));
                source(&mut fields, "Source:      ", "config_path");
            }
            "ZOTERO" => {
                fields.push(("Library:     ".into(), "~/Zotero".into()));
                fields.push(("Profile:     ".into(), "~/.zotero".into()));
                source(&mut fields, "Source lib:  ", "config_path");
                source(&mut fields, "Source prof: ", "share_path");
            }
            "HELIUM" => {
                fields.push(("Folder:      ".into(), "~/.config/net.imput.helium".into()));
                source(&mut fields, "Source:      ", "config_path");
            }
            "FDM" | "FISH" => {
                let (cfg, share) = if name == "FDM" { ("Softdeluxe", "Softdeluxe/Free Download Manager") } else { ("fish", "fish") };
                fields.push(("Config:      ".into(), format!("~/.config/{cfg}")));
                fields.push(("Share:       ".into(), format!("~/.local/share/{share}")));
                source(&mut fields, "Source cfg:  ", "config_path");
                source(&mut fields, "Source data: ", "share_path");
            }
            "HOWDY" => {
                let user = field("user", "");
                let file = field("model_file", "");
                fields.push(("Models Dir:  ".into(), "/etc/howdy/models".into()));
                fields.push(("User Model:  ".into(), if file.is_empty() { format!("{user}.dat") } else { file }));
            }
            "KDE" => {
                fields.push(("Config:      ".into(), "~/.config (kdeglobals, kwinrc, Plasma applets, shortcuts)".into()));
                fields.push(("Share:       ".into(), "~/.local/share (plasma, kwin, icons, wallpapers)".into()));
                source(&mut fields, "Source cfg:  ", "config_path");
                source(&mut fields, "Source data: ", "share_path");
            }
            _ => {}
        }
        apps.push(InfoApp { key, name, fields });
    }
    InfoReport {
        file,
        size,
        created: jq_str(m, &["created_at"], "Unknown"),
        host: jq_str(m, &["hostname"], "Unknown"),
        user: jq_str(m, &["user"], "Unknown"),
        target: jq_str(m, &["target"], "Unknown"),
        media,
        apps,
    }
}

pub fn cmd_info(file: Option<&Path>) -> Result<()> {
    let Some(zip) = file else {
        log_error("Please specify a zip archive to inspect.");
        println!("Usage: kast info <archive.zip>");
        return Err(reported());
    };
    match read_info(zip) {
        Ok(r) => {
            print_info(&r);
            Ok(())
        }
        Err(msg) => {
            log_error(&msg);
            Err(reported())
        }
    }
}

fn print_info(r: &InfoReport) {
    println!("{C_BOLD}=== Backup Archive Information ==={C_RESET}");
    println!("  Archive File:     {} ({})", r.file, r.size);
    println!("  Created At:       {}", r.created);
    println!("  Source Host:      {} ({})", r.host, r.user);
    println!("  Backup Target:    {}", r.target);
    println!("  Media Included:   {}", r.media);
    println!();
    println!("{C_BOLD}Included Applications & Folders:{C_RESET}");
    for app in &r.apps {
        println!("  {C_CYAN}• {}{C_RESET}", app.name);
        for (label, value) in &app.fields {
            println!("      {label}{value}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jq_fallbacks() {
        let v: Value = serde_json::json!({"a": {"s": "x", "n": null, "f": false, "t": true, "o": 3}});
        assert_eq!(jq_str(&v, &["a", "s"], "d"), "x");
        assert_eq!(jq_str(&v, &["a", "n"], "d"), "d");
        assert_eq!(jq_str(&v, &["a", "f"], "d"), "d");
        assert_eq!(jq_str(&v, &["a", "t"], "d"), "true");
        assert_eq!(jq_str(&v, &["a", "o"], "d"), "3");
        assert_eq!(jq_str(&v, &["missing", "x"], "d"), "d");
    }
}
