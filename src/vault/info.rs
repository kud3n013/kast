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

pub fn cmd_info(file: Option<&Path>) -> Result<()> {
    let Some(zip) = file else {
        log_error("Please specify a zip archive to inspect.");
        println!("Usage: arch-setup info <archive.zip>");
        return Err(reported());
    };
    if !zip.is_file() {
        log_error(&format!("Archive file not found: {}", zip.display()));
        return Err(reported());
    }
    let Some(raw) = read_zip_manifest(zip) else {
        log_error(&format!("No 'backup_manifest.json' found inside {}", zip.display()));
        return Err(reported());
    };
    let m: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    println!("{C_BOLD}=== Backup Archive Information ==={C_RESET}");
    println!("  Archive File:     {} ({})", zip.display(), format_bytes(zip));
    println!("  Created At:       {}", jq_str(&m, &["created_at"], "Unknown"));
    println!("  Source Host:      {} ({})", jq_str(&m, &["hostname"], "Unknown"), jq_str(&m, &["user"], "Unknown"));
    println!("  Backup Target:    {}", jq_str(&m, &["target"], "Unknown"));
    let media = m.get("include_media").map_or("Unknown".to_string(), |v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    println!("  Media Included:   {media}");
    println!();
    println!("{C_BOLD}Included Applications & Folders:{C_RESET}");

    let Some(apps) = m.get("apps").and_then(Value::as_object) else { return Ok(()) };
    for key in apps.keys() {
        let name: String = key.to_uppercase().chars().filter(|c| *c != ' ' && *c != '-').collect();
        println!("  {C_CYAN}• {name}{C_RESET}");
        let field = |f: &str, d: &str| jq_str(&m, &["apps", key, f], d);
        let source = |label: &str, f: &str| {
            let v = field(f, "");
            if !v.is_empty() {
                println!("      {label}{v}");
            }
        };
        match name.as_str() {
            "BEEPER" | "ZALO" | "ANTIGRAVITY" => {
                let default = match name.as_str() {
                    "BEEPER" => "BeeperTexts",
                    "ZALO" => "ZaloData",
                    _ => "Antigravity",
                };
                println!("      Folder:      ~/.config/{}", field("folder", default));
                source("Source:      ", "config_path");
            }
            "VESKTOP" | "ZEN" | "OBSIDIAN" | "FCITX5" | "HYPR" => {
                println!("      Folder:      ~/.config/{}", key);
                source("Source:      ", "config_path");
            }
            "HOWDY" => {
                let user = field("user", "");
                let file = field("model_file", "");
                println!("      Models Dir:  /etc/howdy/models");
                println!("      User Model:  {}", if file.is_empty() { format!("{user}.dat") } else { file });
            }
            "KDE" => {
                println!("      Config:      ~/.config (kdeglobals, kwinrc, Plasma applets, shortcuts)");
                println!("      Share:       ~/.local/share (plasma, kwin, icons, wallpapers)");
                source("Source cfg:  ", "config_path");
                source("Source data: ", "share_path");
            }
            _ => {}
        }
    }
    Ok(())
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
