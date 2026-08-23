//! The acquisition commands: buy, rip, fetch, pending.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use owo_colors::OwoColorize;

use super::pick::{self, Choice, Selector};
use super::shop::{self, SearchOpts, SearchOutcome};
use super::types::*;
use super::Registry;
use crate::config::{Config, Credentials};
use crate::db::{self, MasterDb, SafetyOpts};
use crate::pending::{PendingStore, State};
use crate::{analysis, proc, transfer};

/// Search, let the user choose, and open the purchase page in a browser.
///
/// Nothing about payment is automated — Bandcamp's checkout is a card flow in
/// their own UI, and there is no API for it. What this does is get you to the
/// right page and remember what the purchase was for, so `fetch` can collect it.
pub struct BuyArgs {
    pub query: String,
    pub track_id: Option<String>,
    pub selector: Selector,
    pub print_url: bool,
    pub limit: Option<usize>,
    pub enrich: Option<usize>,
    pub yes: bool,
}

pub fn buy(
    db: Option<&MasterDb>,
    cfg: &Config,
    creds: &Credentials,
    args: BuyArgs,
) -> Result<()> {
    let reg = Registry::from_config(cfg, creds);
    let query = build_query(db, cfg, &args.query, args.track_id.as_deref(), args.limit)?;
    let opts = SearchOpts {
        timeout: Duration::from_secs(cfg.search.timeout_secs.max(1)),
        enrich_top_n: args.enrich.unwrap_or(cfg.search.enrich_top_n),
        ..Default::default()
    };

    let outcome = shop::search_all(&reg, &query, &opts);
    print!("{}", super::render::table(&outcome));
    if outcome.offers.is_empty() {
        bail!("nothing to buy");
    }

    let chosen = choose(&outcome, &args.selector)?;
    let offer = &chosen.offer;

    let backend = reg
        .get(offer.backend())
        .ok_or_else(|| anyhow!("{} is not enabled", offer.backend()))?;

    match backend.purchase(&offer.item_ref)? {
        PurchaseFlow::NotRequired => {
            println!(
                "{} — nothing to buy. Use `rekord-ripper rip {}` instead.",
                offer.title,
                offer.item_ref
            );
        }
        PurchaseFlow::AlreadyOwned => {
            println!(
                "you already own {} — run `rekord-ripper fetch --offer {}`",
                offer.title, offer.item_ref
            );
        }
        PurchaseFlow::OpenInBrowser { url, note } => {
            println!();
            println!("{}  {} — {}", "buying:".bold(), offer.artist, offer.title);
            println!("{}   {}", "price:".bold(), super::render::price_cell(offer));
            println!("{}     {url}", "url:".bold());
            if let Some(n) = note {
                println!("{}    {n}", "note:".bold());
            }
            if args.print_url {
                return Ok(());
            }
            if !args.yes && !confirm("open this in your browser?")? {
                println!("cancelled.");
                return Ok(());
            }
            proc::open_url(&url)?;
            println!();
            println!(
                "pay in the browser, then run: {}",
                format!("rekord-ripper fetch --offer {}", offer.item_ref).bold()
            );
        }
    }
    Ok(())
}

/// Download something free, or something already owned.
pub struct FetchArgs {
    /// A backend URL, or an item ref.
    pub target: Option<String>,
    pub offer: Option<ItemRef>,
    pub out: Option<std::path::PathBuf>,
    /// Queue the result against this track for an analysis transfer.
    pub src_track_id: Option<String>,
    pub format_pref: Option<Vec<AudioFormat>>,
    pub overwrite: bool,
    pub lock: bool,
}

pub fn fetch(
    db: Option<&MasterDb>,
    cfg: &Config,
    creds: &Credentials,
    args: FetchArgs,
) -> Result<()> {
    let reg = Registry::from_config(cfg, creds);

    let item = match (&args.offer, &args.target) {
        (Some(r), _) => r.clone(),
        (None, Some(t)) => match t.parse::<ItemRef>() {
            Ok(r) => r,
            // Not a ref, so try it as a URL and let a backend claim it.
            Err(_) => reg
                .claim_url(t)
                .map(|(_, r)| r)
                .ok_or_else(|| anyhow!("no backend recognises '{t}'"))?,
        },
        (None, None) => bail!("give me a URL or an --offer ref"),
    };

    let backend = reg
        .get(item.backend)
        .ok_or_else(|| anyhow!("{} is not enabled", item.backend))?;

    let dest = match &args.out {
        Some(p) => p.clone(),
        None => cfg.download_dir()?,
    };
    let format_pref = match args.format_pref {
        Some(p) => p,
        None => super::format_preference(cfg)?,
    };

    println!("fetching {item} ...");
    let files = backend.fetch(
        &item,
        &FetchOpts {
            dest_dir: dest.clone(),
            format_pref,
            retention: Retention::Keep,
            overwrite: args.overwrite,
            deadline: Instant::now() + Duration::from_secs(1800),
        },
    )?;

    for f in &files {
        println!(
            "{} {} ({}, {:.1} MB)",
            "got".green(),
            f.path.display(),
            f.format,
            f.bytes as f64 / 1_048_576.0
        );
        if !f.format.is_lossless() {
            // Be plain about it rather than letting a downgrade look like a win.
            println!(
                "     {}",
                "note: this is a lossy transcode, which may be worse than what you already have"
                    .yellow()
            );
        }
    }

    // Queue the transfer if a source track was named.
    if let Some(src_id) = &args.src_track_id {
        let db = db.ok_or_else(|| anyhow!("--src-track-id needs the rekordbox database"))?;
        let src = analysis::load_track(db, src_id)?;
        let store = PendingStore::open()?;
        for f in &files {
            let id = store.add(
                &src,
                &f.path,
                Some(item.backend.as_str()),
                // Rekordbox auto-analyses on import and leaves cues behind, which
                // build_plan would otherwise refuse to overwrite.
                true,
                args.lock,
                cfg.pending.ttl_days,
            )?;
            println!(
                "queued transfer #{id}: {} → this file, pending import",
                src.title.as_deref().unwrap_or(src_id)
            );
        }
    }

    println!();
    println!(
        "{} drag {} into rekordbox to import, then run {}",
        "next:".bold(),
        dest.display(),
        "rekord-ripper pending apply".bold()
    );
    println!(
        "      {}",
        "(rekordbox has no watch-folder feature, so this step is manual)".dimmed()
    );
    Ok(())
}

/// List, inspect, and act on queued pairings.
pub enum PendingAction {
    List,
    Apply { dry_run: bool },
    Clear { id: i64 },
}

pub fn pending(
    db: &mut MasterDb,
    cfg: &Config,
    safety: SafetyOpts,
    action: PendingAction,
) -> Result<()> {
    let store = PendingStore::open()?;

    // Retire anything that can no longer make progress, and say so — silently
    // dropping work the user is waiting on would be worse than noisy.
    for (id, state, why) in store.sweep(db)? {
        eprintln!("{} #{id} → {state}: {why}", "swept:".yellow());
    }

    match action {
        PendingAction::Clear { id } => {
            store.remove(id)?;
            println!("removed #{id}");
            Ok(())
        }
        PendingAction::List => {
            let all = store.all()?;
            if all.is_empty() {
                println!("{}", "nothing pending.".dimmed());
                return Ok(());
            }
            println!(
                "{:>4}  {:<16} {:<28} {}",
                "#".bold(),
                "state".bold(),
                "source".bold(),
                "acquired file".bold()
            );
            for e in &all {
                let src = format!(
                    "{} — {}",
                    e.src_artist.as_deref().unwrap_or("?"),
                    e.src_title.as_deref().unwrap_or("?")
                );
                println!(
                    "{:>4}  {:<16} {:<28} {}",
                    e.id,
                    e.state.to_string(),
                    clip(&src, 28),
                    e.acquired_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
                if let Some(v) = &e.verdict {
                    println!("      {}", clip(v, 100).dimmed());
                }
            }
            Ok(())
        }
        PendingAction::Apply { dry_run } => apply_pending(db, &store, cfg, safety, dry_run),
    }
}

fn apply_pending(
    db: &mut MasterDb,
    store: &PendingStore,
    cfg: &Config,
    safety: SafetyOpts,
    dry_run: bool,
) -> Result<()> {
    let waiting = store.in_state(State::AwaitingImport)?;
    if waiting.is_empty() {
        println!("{}", "nothing awaiting import.".dimmed());
    }

    let mut ready = Vec::new();
    for entry in waiting {
        match transfer::process(db, store, &entry, cfg)? {
            transfer::Processed::NotImported => {
                println!(
                    "{} {} — not in rekordbox yet",
                    "waiting:".dimmed(),
                    entry
                        .acquired_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
            }
            transfer::Processed::Rejected(why) => {
                eprintln!("{} #{}: {why}", "fp REJECT".red(), entry.id);
            }
            transfer::Processed::Ready { plan, verdict, .. } => {
                println!("{}", transfer::report(&plan, &verdict));
                ready.push((entry, plan));
            }
        }
    }

    if ready.is_empty() {
        return Ok(());
    }
    if dry_run {
        eprintln!(
            "{} ready. Dry-run; pass --apply to write.",
            ready.len()
        );
        return Ok(());
    }

    // Same preflight and backup discipline as `cp` and `auto`.
    db::safety_preflight(safety)?;
    for (i, (entry, plan)) in ready.iter().enumerate() {
        let backup = analysis::apply_plan(db, plan)?;
        if i == 0 {
            eprintln!("backed up to: {}", backup.display());
        }
        transfer::mark_applied(store, entry)?;
        eprintln!("applied: {} → {}", plan.src.id, plan.dst.id);
    }
    Ok(())
}

/// Build a search query from free text or from a local track.
fn build_query(
    db: Option<&MasterDb>,
    cfg: &Config,
    text: &str,
    track_id: Option<&str>,
    limit: Option<usize>,
) -> Result<SearchQuery> {
    let limit = limit.unwrap_or(cfg.search.limit);
    if let Some(id) = track_id {
        let db = db.ok_or_else(|| anyhow!("--track-id needs the rekordbox database"))?;
        let t = analysis::load_track(db, id)?;
        let title = t
            .title
            .clone()
            .ok_or_else(|| anyhow!("track {id} has no title to search for"))?;
        return Ok(SearchQuery {
            title,
            artist: t.artist.clone(),
            duration_secs: t.length,
            limit,
            ..Default::default()
        });
    }
    if text.trim().is_empty() {
        bail!("give me something to search for, or pass --track-id");
    }
    Ok(SearchQuery::from_text(text, limit))
}

/// Resolve a selector, prompting when none was given.
fn choose<'a>(out: &'a SearchOutcome, sel: &Selector) -> Result<&'a shop::RankedOffer> {
    if !sel.is_empty() {
        return pick::resolve(out, sel);
    }
    match pick::prompt(out)? {
        Some(Choice::Row(n)) => out
            .by_row(n)
            .ok_or_else(|| anyhow!("there is no row {n}")),
        Some(Choice::Open(n)) => {
            let r = out.by_row(n).ok_or_else(|| anyhow!("there is no row {n}"))?;
            proc::open_url(&r.offer.url)?;
            bail!("opened row {n} in your browser; run the command again to buy it")
        }
        Some(Choice::Quit) | None => bail!("cancelled"),
    }
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Never assume yes for something that opens a payment page.
        bail!("not a terminal — pass --yes to skip this confirmation");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_text_becomes_a_query() {
        let cfg = Config::default();
        let q = build_query(None, &cfg, "burial untrue", None, Some(4)).unwrap();
        assert_eq!(q.title, "burial untrue");
        assert_eq!(q.limit, 4);
    }

    #[test]
    fn an_empty_query_is_refused_rather_than_searching_for_nothing() {
        let cfg = Config::default();
        assert!(build_query(None, &cfg, "   ", None, None).is_err());
    }

    #[test]
    fn a_track_id_without_a_database_says_so() {
        let cfg = Config::default();
        let err = build_query(None, &cfg, "", Some("123"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs the rekordbox database"), "got {err}");
    }

    #[test]
    fn the_query_limit_falls_back_to_config() {
        let mut cfg = Config::default();
        cfg.search.limit = 11;
        assert_eq!(build_query(None, &cfg, "x", None, None).unwrap().limit, 11);
    }

    #[test]
    fn clip_shortens_without_splitting_a_character() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefgh", 4), "abc…");
        assert_eq!(clip("ünïcödé", 4).chars().count(), 4);
    }
}
