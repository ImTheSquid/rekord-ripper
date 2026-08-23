use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use rekord_ripper::acquire;
use rekord_ripper::analysis::{self, CopyOpts};
use rekord_ripper::config::{Config, Credentials};
use rekord_ripper::db::{self, MasterDb, SafetyOpts};
use rekord_ripper::dump;
use rekord_ripper::paths;
use rekord_ripper::tui;

#[derive(Parser)]
#[command(name = "rekord-ripper", version, about = "Rekordbox analysis utility")]
struct Cli {
    /// Bypass the "rekordbox is running" hard refuse on any mutating command.
    #[arg(
        long = "i-know-rekordbox-is-open-and-may-corrupt-my-data",
        global = true
    )]
    bypass_rekordbox_check: bool,

    /// Path to config.toml. Overrides REKORD_RIPPER_CONFIG and the default
    /// location.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Dump analysis state for tracks. With no query, lists every track.
    ///
    /// A numeric query is matched against djmdContent.ID; anything else is
    /// matched as a substring against Title and Artist name.
    Dump {
        /// Track ID, or substring of title/artist. Omit to dump everything.
        query: Option<String>,
        /// Maximum number of tracks to print. Defaults to 10 when searching by
        /// substring; unlimited when listing all (no query).
        #[arg(short, long)]
        limit: Option<u32>,
    },

    /// Copy analysis (cues, beat grid, BPM, key, mixer params) from one track
    /// onto one or more destinations.
    Cp {
        /// Source track ID — the analyzed track to read from.
        src: String,
        /// Destination track IDs — receive a copy of `src`'s analysis.
        #[arg(required = true)]
        dst: Vec<String>,
        /// Overwrite cues on destinations that already have some.
        #[arg(long)]
        replace: bool,
        /// After copying, set bit 7 of djmdContent.Analysed on each destination
        /// so rekordbox won't re-analyze and lose your copied state.
        #[arg(long)]
        lock: bool,
        /// Print the plan without applying it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Interactive two-column TUI. Source on the left, destinations on the
    /// right, with search bars, multi-select, fuzzy-match-from-source toggle,
    /// and an auto-mode filter for unanalyzed destinations.
    Tui,

    /// Batch-match unanalyzed (or unlocked) tracks to a similar analyzed source
    /// by normalized title + artist + duration, then copy. Default = dry-run;
    /// pass --apply to write.
    Auto {
        /// Maximum number of matched plans to consider.
        #[arg(short, long)]
        limit: Option<u32>,
        /// Actually apply matched plans. Without this, prints proposals only.
        #[arg(long)]
        apply: bool,
        /// Overwrite cues on destinations that already have some.
        #[arg(long)]
        replace: bool,
        /// Set the lock bit on destinations after copy.
        #[arg(long)]
        lock: bool,
        /// Tolerance on track length difference (in integer seconds).
        #[arg(long, default_value_t = 1)]
        duration_tol_secs: i64,
        /// Allow destinations that already have cues (still gated by --replace).
        #[arg(long)]
        include_cued: bool,
    },

    /// Search every enabled backend at once and print one comparable table of
    /// offers, so you can see what a track costs and in which formats before
    /// buying it.
    ///
    /// Prices in different currencies are grouped, never converted — there is no
    /// exchange-rate source here, so a single "cheapest" across currencies would
    /// be a guess.
    Shop {
        /// Free-text query. Omit when using --track-id.
        query: Vec<String>,
        /// Build the query from a track already in your rekordbox library.
        #[arg(long, value_name = "ID", conflicts_with = "query")]
        track_id: Option<String>,
        /// Restrict to these backends. Repeatable; defaults to all enabled.
        #[arg(long, value_name = "BACKEND")]
        backend: Vec<acquire::BackendId>,
        /// Raw hits requested per backend.
        #[arg(long)]
        limit: Option<usize>,
        /// Top offers per backend to price-probe. 0 skips probing.
        #[arg(long, value_name = "N")]
        enrich: Option<usize>,
        /// Primary sort axis.
        #[arg(long, value_enum, default_value_t)]
        sort: acquire::shop::SortMode,
        /// Show only offers priced in this ISO currency code (e.g. GBP).
        #[arg(long, value_name = "CODE")]
        currency: Option<String>,
        /// Hide offers with no lossless format.
        #[arg(long)]
        lossless_only: bool,
        /// Machine-readable output, for scripting.
        #[arg(long)]
        json: bool,
        /// Treat any backend failure as fatal instead of degrading to a partial
        /// table.
        #[arg(long)]
        strict: bool,
    },

    /// Report which acquisition backends are enabled, whether their credentials
    /// are configured, and whether the external tools they need are installed.
    ///
    /// Makes no network calls and never opens master.db — this is the command
    /// you run to find out why something else is failing.
    Backends,

    /// Show where config.toml lives, or write a commented starter file.
    Config {
        /// Write a starter config.toml, with every value at its default.
        #[arg(long)]
        init: bool,
        /// Overwrite an existing config.toml.
        #[arg(long, requires = "init")]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let safety = SafetyOpts {
        bypass_rekordbox_check: cli.bypass_rekordbox_check,
    };
    let config_path = paths::config_path(cli.config.as_deref())?;

    // Open master.db only for commands that actually read it, so a missing or
    // broken rekordbox install cannot stop you running `backends` to find out why.
    let mut db = match needs_database(&cli.cmd) {
        true => Some(MasterDb::open()?),
        false => None,
    };

    match cli.cmd {
        Cmd::Backends => {
            let cfg = Config::load(&config_path)?;
            let creds = Credentials::load(&paths::credentials_path()?)?;
            acquire::report::run(&cfg, &creds, &config_path)?
        }
        Cmd::Config { init, force } => run_config(&config_path, init, force)?,
        Cmd::Shop {
            query,
            track_id,
            backend,
            limit,
            enrich,
            sort,
            currency,
            lossless_only,
            json,
            strict,
        } => {
            let cfg = Config::load(&config_path)?;
            let creds = Credentials::load(&paths::credentials_path()?)?;
            run_shop(
                db.as_ref(),
                &cfg,
                &creds,
                ShopArgs {
                    query: query.join(" "),
                    track_id,
                    backend,
                    limit,
                    enrich,
                    sort,
                    currency,
                    lossless_only,
                    json,
                    strict,
                },
            )?
        }
        Cmd::Dump { query, limit } => {
            dump::run(db.as_ref().expect("dump needs the db"), query.as_deref(), limit)?
        }
        Cmd::Tui => tui::run(db.take().expect("tui needs the db"), safety)?,
        Cmd::Cp {
            src,
            dst,
            replace,
            lock,
            dry_run,
        } => run_cp(
            db.as_mut().expect("cp needs the db"),
            &src,
            &dst,
            CopyOpts { replace, lock },
            dry_run,
            safety,
        )?,
        Cmd::Auto {
            limit,
            apply,
            replace,
            lock,
            duration_tol_secs,
            include_cued,
        } => run_auto(
            db.as_mut().expect("auto needs the db"),
            AutoArgs {
                limit,
                apply,
                opts: CopyOpts { replace, lock },
                duration_tol_secs,
                include_cued,
                safety,
            },
        )?,
    }
    Ok(())
}

/// True when this command reads `master.db`.
fn needs_database(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Backends | Cmd::Config { .. } => false,
        // Only needed to seed the query from an existing track.
        Cmd::Shop { track_id, .. } => track_id.is_some(),
        Cmd::Dump { .. } | Cmd::Tui | Cmd::Cp { .. } | Cmd::Auto { .. } => true,
    }
}

struct ShopArgs {
    query: String,
    track_id: Option<String>,
    backend: Vec<acquire::BackendId>,
    limit: Option<usize>,
    enrich: Option<usize>,
    sort: acquire::shop::SortMode,
    currency: Option<String>,
    lossless_only: bool,
    json: bool,
    strict: bool,
}

fn run_shop(
    db: Option<&MasterDb>,
    cfg: &Config,
    creds: &Credentials,
    args: ShopArgs,
) -> Result<()> {
    use acquire::shop;

    let limit = args.limit.unwrap_or(cfg.search.limit);

    // Seed from a local track when asked, so you don't retype what rekordbox
    // already knows.
    let query = match (&args.track_id, args.query.trim()) {
        (Some(id), _) => {
            let t = analysis::load_track(db.expect("--track-id requires the db"), id)?;
            let title = t
                .title
                .clone()
                .ok_or_else(|| anyhow::anyhow!("track {id} has no title to search for"))?;
            eprintln!(
                "searching for: {} — {}",
                t.artist.as_deref().unwrap_or("?"),
                title
            );
            acquire::SearchQuery {
                title,
                artist: t.artist.clone(),
                duration_secs: t.length,
                limit,
                ..Default::default()
            }
        }
        (None, "") => anyhow::bail!("give me something to search for, or pass --track-id"),
        (None, text) => acquire::SearchQuery::from_text(text, limit),
    };

    // A price threshold across currencies is not computable here, so refuse it
    // rather than silently comparing incomparable numbers.
    if let Some(c) = &args.currency {
        if c.trim().len() != 3 {
            anyhow::bail!("--currency takes a 3-letter ISO code, e.g. GBP");
        }
    }

    let reg = acquire::Registry::from_config(cfg, creds);
    if reg.is_empty() {
        anyhow::bail!("no backends enabled — check {}", "config.toml");
    }

    let opts = shop::SearchOpts {
        timeout: std::time::Duration::from_secs(cfg.search.timeout_secs.max(1)),
        enrich_top_n: args.enrich.unwrap_or(cfg.search.enrich_top_n),
        only: args.backend,
        lossless_only: args.lossless_only,
        currency: args.currency,
        sort: args.sort,
    };

    let outcome = shop::search_all(&reg, &query, &opts);

    if args.json {
        println!("{}", shop_json(&outcome)?);
    } else {
        print!("{}", acquire::render::table(&outcome));
    }

    if args.strict {
        if let Some(first) = outcome.failures().next() {
            anyhow::bail!(
                "{} failed and --strict was given: {}",
                first.backend,
                first.error.as_ref().expect("failures() filters on error")
            );
        }
    }
    if outcome.total_failure() {
        anyhow::bail!("every backend failed — see the errors above");
    }
    Ok(())
}

/// Machine-readable outcome, for scripting and tests.
fn shop_json(out: &acquire::shop::SearchOutcome) -> Result<String> {
    let offers: Vec<serde_json::Value> = out
        .offers
        .iter()
        .map(|r| {
            serde_json::json!({
                "row": r.row,
                "match_score": r.match_score,
                "backend": r.offer.backend().as_str(),
                "item_ref": r.offer.item_ref.to_string(),
                "kind": r.offer.kind.to_string(),
                "artist": r.offer.artist,
                "title": r.offer.title,
                "album": r.offer.album,
                "url": r.offer.url,
                "duration_secs": r.offer.duration_secs,
                "cost_class": r.offer.cost_class().to_string(),
                "price": acquire::render::price_cell(&r.offer),
                "formats": r.offer.formats.as_ref().map(|fs| {
                    fs.iter().map(|f| f.to_string()).collect::<Vec<_>>()
                }),
                "has_lossless": r.offer.has_lossless(),
                "ownership": acquire::render::ownership_cell(&r.offer),
                "requires_purchase": r.offer.requires_purchase(),
                "enrich_error": r.offer.enrich_error,
            })
        })
        .collect();

    let backends: Vec<serde_json::Value> = out
        .per_backend
        .iter()
        .map(|r| {
            serde_json::json!({
                "backend": r.backend.as_str(),
                "elapsed_ms": r.elapsed.as_millis(),
                "raw_hits": r.raw_hits,
                "enriched": r.enriched,
                "enrich_errors": r.enrich_errors,
                "error": r.error.as_ref().map(|e| e.to_string()),
            })
        })
        .collect();

    let cheapest: Vec<serde_json::Value> = acquire::shop::cheapest_per_currency(&out.offers)
        .into_iter()
        .map(|(p, b)| {
            serde_json::json!({
                "amount_minor": p.amount_minor,
                "currency": p.currency,
                "backend": b.as_str(),
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "offers": offers,
        "backends": backends,
        "cheapest_per_currency": cheapest,
        // Stated in the output itself so a consumer cannot mistake the list
        // above for a single ranked-by-price answer.
        "currency_note": "prices are in each seller's own currency and are never converted",
    }))?)
}

fn run_config(path: &std::path::Path, init: bool, force: bool) -> Result<()> {
    if !init {
        println!("{}", path.display());
        if !path.exists() {
            eprintln!("(does not exist — run `rekord-ripper config --init` to create it)");
        }
        return Ok(());
    }

    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists — pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = Config::default().to_toml()?;
    std::fs::write(path, &body)?;
    println!("wrote {}", path.display());
    eprintln!(
        "note: the Bandcamp identity cookie goes in {} (mode 600), not here.",
        paths::credentials_path()?.display()
    );
    Ok(())
}

struct AutoArgs {
    limit: Option<u32>,
    apply: bool,
    opts: CopyOpts,
    duration_tol_secs: i64,
    include_cued: bool,
    safety: SafetyOpts,
}

fn run_auto(db: &mut MasterDb, args: AutoArgs) -> Result<()> {
    let matches = analysis::find_auto_matches(
        db,
        analysis::AutoFilter {
            duration_tol_secs: args.duration_tol_secs,
            include_cued: args.include_cued,
            limit: args.limit,
        },
    )?;

    if matches.is_empty() {
        println!("No matches found.");
        return Ok(());
    }

    let mut plans = Vec::new();
    let mut failed = Vec::new();
    for m in &matches {
        match analysis::build_plan(db, &m.src_id, &m.dst_id, &args.opts) {
            Ok(plan) => plans.push(plan),
            Err(e) => failed.push((m.clone(), e)),
        }
    }

    for plan in &plans {
        println!("{}", plan.render());
    }
    for (m, e) in &failed {
        eprintln!(
            "skip {} ← {}: {e}",
            m.dst_id, m.src_id
        );
    }

    if !args.apply {
        eprintln!(
            "{} matched, {} eligible, {} failed validation. Dry-run; pass --apply to write.",
            matches.len(),
            plans.len(),
            failed.len()
        );
        return Ok(());
    }

    db::safety_preflight(args.safety)?;

    for (i, plan) in plans.iter().enumerate() {
        let backup = analysis::apply_plan(db, plan)?;
        if i == 0 {
            eprintln!("backed up to: {}", backup.display());
        }
        eprintln!("applied: {} → {}", plan.src.id, plan.dst.id);
    }
    Ok(())
}

fn run_cp(
    db: &mut MasterDb,
    src: &str,
    dsts: &[String],
    opts: CopyOpts,
    dry_run: bool,
    safety: SafetyOpts,
) -> Result<()> {
    // Build every plan up front; abort the batch if any fails validation.
    let plans = dsts
        .iter()
        .map(|dst| analysis::build_plan(db, src, dst, &opts))
        .collect::<Result<Vec<_>>>()?;

    for plan in &plans {
        println!("{}", plan.render());
    }

    if dry_run {
        eprintln!("dry-run: no changes applied.");
        return Ok(());
    }

    db::safety_preflight(safety)?;

    for (i, plan) in plans.iter().enumerate() {
        let backup = analysis::apply_plan(db, plan)?;
        if i == 0 {
            eprintln!("backed up to: {}", backup.display());
        }
        eprintln!("applied: {} → {}", plan.src.id, plan.dst.id);
    }
    Ok(())
}
