use anyhow::Result;
use clap::{Parser, Subcommand};

use rekord_ripper::analysis::{self, CopyOpts};
use rekord_ripper::db::{self, MasterDb, SafetyOpts};
use rekord_ripper::dump;

#[derive(Parser)]
#[command(name = "rekord-ripper", version, about = "Rekordbox analysis utility")]
struct Cli {
    /// Bypass the "rekordbox is running" hard refuse on any mutating command.
    #[arg(
        long = "i-know-rekordbox-is-open-and-may-corrupt-my-data",
        global = true
    )]
    bypass_rekordbox_check: bool,

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut db = MasterDb::open()?;
    let safety = SafetyOpts {
        bypass_rekordbox_check: cli.bypass_rekordbox_check,
    };
    match cli.cmd {
        Cmd::Dump { query, limit } => dump::run(&db, query.as_deref(), limit)?,
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

    for plan in &plans {
        analysis::apply_plan(db, plan)?;
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

    for plan in &plans {
        analysis::apply_plan(db, plan)?;
        eprintln!("applied: {} → {}", plan.src.id, plan.dst.id);
    }
    Ok(())
}
