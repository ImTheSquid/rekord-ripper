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

    // Commands that need neither the database nor rekordbox installed are
    // handled first, so a broken rekordbox install cannot stop you diagnosing it.
    match &cli.cmd {
        Cmd::Backends => {
            let cfg = Config::load(&config_path)?;
            let creds = Credentials::load(&paths::credentials_path()?)?;
            return acquire::report::run(&cfg, &creds, &config_path);
        }
        Cmd::Config { init, force } => return run_config(&config_path, *init, *force),
        _ => {}
    }

    let mut db = MasterDb::open()?;
    match cli.cmd {
        Cmd::Backends | Cmd::Config { .. } => unreachable!("handled above"),
        Cmd::Dump { query, limit } => dump::run(&db, query.as_deref(), limit)?,
        Cmd::Tui => tui::run(db, safety)?,
        Cmd::Cp {
            src,
            dst,
            replace,
            lock,
            dry_run,
        } => run_cp(
            &mut db,
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
            &mut db,
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
