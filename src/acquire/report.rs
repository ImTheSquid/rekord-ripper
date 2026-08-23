//! `backends` — what is enabled, what is authenticated, what is installed.
//!
//! Deliberately makes no network calls and never opens `master.db`: this is the
//! command you run to find out why something else isn't working, so it must not
//! be able to fail for the same reasons.

use anyhow::Result;
use owo_colors::OwoColorize;

use super::types::{Capabilities, CredentialState};
use super::Registry;
use crate::config::{Config, Credentials};
use crate::proc;

/// External binaries a backend may need, with the flag that proves they run.
const TOOLS: &[(&str, &str, &str)] = &[
    ("yt-dlp", "--version", "soundcloud search and rip"),
    ("ffmpeg", "-version", "audio decode for fingerprinting"),
    ("ffprobe", "-version", "precise duration for the speed pre-filter"),
];

pub fn run(cfg: &Config, creds: &Credentials, config_path: &std::path::Path) -> Result<()> {
    let reg = Registry::from_config(cfg, creds);

    println!("{}", "config".bold().cyan());
    let exists = if config_path.exists() {
        "".to_string()
    } else {
        format!(" {}", "(not created — using defaults)".dimmed())
    };
    println!("  {}{}", config_path.display(), exists);
    println!("  downloads: {}", cfg.download_dir()?.display());
    match super::format_preference(cfg) {
        Ok(prefs) => println!(
            "  formats:   {}",
            prefs
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(" → ")
        ),
        Err(e) => println!("  formats:   {}", e.to_string().red()),
    }

    println!();
    println!("{}", "backends".bold().cyan());
    if reg.is_empty() {
        println!("  {}", "none enabled".dimmed());
    }
    for b in reg.iter() {
        let caps = b.capabilities();
        println!("  {} {}", b.id().to_string().bold(), caps_summary(&caps).dimmed());
        match b.credentials() {
            CredentialState::NotRequired => {
                println!("    auth: {}", "not required".green())
            }
            CredentialState::Present { hint } => {
                println!("    auth: {} ({hint})", "configured".green())
            }
            CredentialState::Missing { how_to_fix } => {
                println!("    auth: {} — {how_to_fix}", "missing".yellow())
            }
            CredentialState::Malformed { detail } => {
                println!("    auth: {} — {detail}", "malformed".red())
            }
        }
    }

    println!();
    println!("{}", "external tools".bold().cyan());
    let mut missing = Vec::new();
    for (tool, flag, why) in TOOLS {
        // Honour a configured yt-dlp path rather than assuming it is on PATH.
        let path = if *tool == "yt-dlp" {
            cfg.soundcloud.yt_dlp_path.as_str()
        } else {
            tool
        };
        if proc::tool_available(path, flag) {
            println!("  {} {path} {}", "ok  ".green(), format!("— {why}").dimmed());
        } else {
            println!("  {} {path} {}", "MISS".red(), format!("— {why}").dimmed());
            missing.push(*tool);
        }
    }
    if !missing.is_empty() {
        eprintln!();
        eprintln!(
            "{} missing: {}. Install with: brew install {}",
            "warning:".yellow(),
            missing.join(", "),
            missing.join(" ")
        );
    }

    Ok(())
}

fn caps_summary(c: &Capabilities) -> String {
    let mut bits = Vec::new();
    if c.search {
        bits.push("search");
    }
    if c.price_quotes {
        bits.push("prices");
    }
    if c.ownership_check {
        bits.push("ownership");
    }
    if c.fetch {
        bits.push("fetch");
    }
    if c.lossless_capable {
        bits.push("lossless");
    }
    if c.requires_purchase {
        bits.push("purchase-gated");
    }
    if bits.is_empty() {
        "(no capabilities)".to_string()
    } else {
        format!("[{}]", bits.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_summary_lists_only_what_is_supported() {
        let s = caps_summary(&Capabilities {
            search: true,
            fetch: true,
            lossless_capable: true,
            ..Default::default()
        });
        assert_eq!(s, "[search, fetch, lossless]");
    }

    #[test]
    fn caps_summary_says_so_when_there_is_nothing() {
        assert_eq!(caps_summary(&Capabilities::default()), "(no capabilities)");
    }

    #[test]
    fn report_runs_without_a_config_file_or_a_database() {
        // This is the diagnose-everything-else command, so it must work in the
        // situation where nothing is set up yet.
        let missing = std::path::Path::new("/nonexistent/rekord-ripper/config.toml");
        run(&Config::default(), &Credentials::default(), missing).unwrap();
    }
}
