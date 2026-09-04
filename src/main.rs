use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use rekord_ripper::acquire;
use rekord_ripper::analysis::{self, CopyOpts};
use rekord_ripper::config::{Config, Credentials};
use rekord_ripper::db::{self, MasterDb, SafetyOpts};
use rekord_ripper::dump;
use rekord_ripper::paths;
use rekord_ripper::query;
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
    /// A numeric query is an exact djmdContent.ID. Anything else is a search
    /// box: words are ANDed against title and artist, "quoted words" must be
    /// adjacent, -word excludes, OR alternates.
    ///
    /// p:name (or p:"a name") restricts to a playlist or a folder of them.
    /// is:name matches a keyword: local, cloud or stream for where the audio
    /// lives; present or missing for whether a local file is really on this
    /// machine; lossy or lossless, and mp3 m4a flac aiff wav, for what it is;
    /// cues and locked for what the track already carries.
    ///
    /// bpm: and len: take a number, a comparison or a span — bpm:128,
    /// bpm:>=128, bpm:120-130, len:>6m, len:3m-6m, len:4:30. A bare number
    /// covers the precision you typed, so bpm:128 matches 128.02; a comparison
    /// or a span means exactly the number written.
    ///
    /// An excluded term starts with a hyphen, which the flag parser claims
    /// first, so put those after a `--`:
    ///   rekord-ripper dump --limit 5 -- p:"jack night" is:stream -remix
    Dump {
        /// Track ID, or a search. Omit to dump everything.
        query: Vec<String>,
        /// Maximum number of tracks to print. Defaults to 10 when searching;
        /// unlimited when listing all (no query).
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
        /// Repeatable, to shop for several tracks in one run.
        #[arg(long, value_name = "ID", conflicts_with = "query")]
        track_id: Vec<String>,
        /// Shop for every library track matching a search, in the same language
        /// the TUI's `/` box takes. Combines with --track-id.
        ///
        /// e.g. --match 'p:"jn next" is:stream'
        #[arg(long = "match", value_name = "QUERY", conflicts_with = "query")]
        match_query: Option<String>,
        /// Most tracks --match may queue before it refuses. Each one is a
        /// fan-out across every backend, so this is a guard against shopping
        /// half the library by accident.
        #[arg(long, value_name = "N", default_value_t = 25)]
        match_max: usize,
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

    /// Pick an offer and open its purchase page in your browser.
    ///
    /// Payment happens in the browser — there is no API for Bandcamp checkout and
    /// nothing here automates it. This gets you to the right page and tells you
    /// what to run afterwards.
    Buy {
        /// Search terms.
        query: Vec<String>,
        /// Build the query from a track already in your library.
        #[arg(long, value_name = "ID", conflicts_with = "query")]
        track_id: Option<String>,
        /// Buy this exact offer, skipping the search. The scriptable form.
        #[arg(long, value_name = "REF")]
        offer: Option<acquire::ItemRef>,
        /// Auto-pick the top-ranked offer from this backend.
        #[arg(long, value_name = "BACKEND")]
        from: Option<acquire::BackendId>,
        /// Auto-pick this printed row. NOT stable between runs — use --offer in
        /// scripts.
        #[arg(long, value_name = "N")]
        pick: Option<usize>,
        /// Print the purchase URL instead of launching a browser.
        #[arg(long)]
        print_url: bool,
        /// Raw hits requested per backend.
        #[arg(long)]
        limit: Option<usize>,
        /// Top offers per backend to price-probe.
        #[arg(long, value_name = "N")]
        enrich: Option<usize>,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Download a free track, or one you already own, and optionally queue its
    /// analysis transfer.
    Fetch {
        /// A backend URL or an item ref.
        target: Option<String>,
        /// The offer to fetch, instead of a URL.
        #[arg(long, value_name = "REF")]
        offer: Option<acquire::ItemRef>,
        /// Where to put it. Defaults to the configured download directory.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Queue an analysis transfer from this track onto the downloaded file,
        /// to run once rekordbox has imported it.
        #[arg(long, value_name = "ID")]
        src_track_id: Option<String>,
        /// Override the configured format preference for this run.
        #[arg(long, value_name = "LIST", value_delimiter = ',')]
        format: Option<Vec<acquire::AudioFormat>>,
        /// Overwrite an existing file of the same name.
        #[arg(long)]
        overwrite: bool,
        /// Set the lock bit on the destination after the transfer.
        #[arg(long)]
        lock: bool,
    },

    /// Inspect and apply queued analysis transfers.
    Pending {
        /// Just list what is queued, without checking imports or fingerprinting.
        #[arg(long, conflicts_with_all = ["apply", "clear"])]
        list: bool,
        /// Apply the transfers whose files have been imported and which pass the
        /// fingerprint gate. Without this, prints proposals only.
        #[arg(long)]
        apply: bool,
        /// Create the rekordbox rows for downloads that have not been imported,
        /// instead of waiting for them to be dragged in. The queue already
        /// knows each file's source track, so this needs no other argument.
        ///
        /// Needs `insert_content_rows = true` under [import] in your config.
        #[arg(long, conflicts_with_all = ["list", "clear"])]
        import: bool,
        /// Skip the confirmation in front of --import's row creation.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Forget a queued transfer.
        #[arg(long, value_name = "ID")]
        clear: Option<i64>,
    },

    /// Create rekordbox track rows for audio files, so you don't have to drag
    /// them in by hand.
    ///
    /// This is the one command that adds tracks to your library, so it is
    /// default-dry-run, needs `insert_content_rows = true` in config, and prints
    /// every value it would write before asking. With --src-track-id it also
    /// performs the fingerprint-gated analysis transfer in the same run.
    Import {
        /// Audio files to add.
        files: Vec<PathBuf>,
        /// Override the track title. Only valid with a single file.
        #[arg(long)]
        title: Option<String>,
        /// Override the artist. Only valid with a single file.
        #[arg(long)]
        artist: Option<String>,
        /// Copy this track's analysis onto the imported file, gated on a
        /// fingerprint match. Only valid with a single file.
        #[arg(long, value_name = "ID")]
        src_track_id: Option<String>,
        /// Set the lock bit on the imported track after the transfer.
        #[arg(long)]
        lock: bool,
        /// Actually write to master.db. Without this, prints what it would do.
        #[arg(long)]
        apply: bool,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Remove a row this tool inserted, by marking it deleted the way
        /// rekordbox does so the removal syncs.
        #[arg(long, value_name = "ID", conflicts_with_all = ["files", "src_track_id"])]
        undo: Option<String>,
    },

    /// Re-derive `djmdContent.FileType` from the audio itself and correct rows
    /// that disagree.
    ///
    /// A wrong value is not cosmetic: rekordbox reads FileType 0 as "Unknown
    /// Format" and refuses to play the track, however good the file is. Every
    /// local row is probed; anything unreadable or moved is skipped rather than
    /// guessed at. Default = dry-run.
    Repair {
        /// Actually write to master.db. Without this, prints what it would do.
        #[arg(long)]
        apply: bool,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Only correct rows rekordbox cannot play, leaving mislabelled but
        /// working rows alone.
        #[arg(long)]
        unplayable_only: bool,
    },

    /// Compare two audio files by fingerprint and print the raw numbers.
    ///
    /// This is the calibration tool. The accept thresholds shipped in config are
    /// uncalibrated guesses chosen to fail closed; run this over pairs you know
    /// are the same track and pairs you know are not, then set the thresholds
    /// from the gap between them.
    Fp {
        /// First audio file.
        a: PathBuf,
        /// Second audio file.
        b: PathBuf,
        /// Seconds of audio to fingerprint from the start of each file. Both
        /// sides always use the same window.
        #[arg(long, value_name = "N")]
        secs: Option<u32>,
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
        Cmd::Repair {
            apply,
            yes,
            unplayable_only,
        } => run_repair(
            db.as_mut().expect("repair needs the db"),
            safety,
            apply,
            yes,
            unplayable_only,
        )?,
        Cmd::Import {
            files,
            title,
            artist,
            src_track_id,
            lock,
            apply,
            yes,
            undo,
        } => {
            let cfg = Config::load(&config_path)?;
            run_import(
                db.as_mut().expect("import needs the db"),
                &cfg,
                safety,
                ImportArgs {
                    files,
                    title,
                    artist,
                    src_track_id,
                    lock,
                    apply,
                    yes,
                    undo,
                },
            )?
        }
        Cmd::Fp { a, b, secs } => {
            let cfg = Config::load(&config_path)?;
            run_fp(&a, &b, secs.unwrap_or(cfg.fingerprint.window_secs), &cfg)?
        }
        Cmd::Buy {
            query,
            track_id,
            offer,
            from,
            pick,
            print_url,
            limit,
            enrich,
            yes,
        } => {
            let cfg = Config::load(&config_path)?;
            let creds = Credentials::load(&paths::credentials_path()?)?;
            acquire::cmd::buy(
                db.as_ref(),
                &cfg,
                &creds,
                acquire::cmd::BuyArgs {
                    query: query.join(" "),
                    track_id,
                    selector: acquire::pick::Selector {
                        offer,
                        from,
                        row: pick,
                    },
                    print_url,
                    limit,
                    enrich,
                    yes,
                },
            )?
        }
        Cmd::Fetch {
            target,
            offer,
            out,
            src_track_id,
            format,
            overwrite,
            lock,
        } => {
            let cfg = Config::load(&config_path)?;
            let creds = Credentials::load(&paths::credentials_path()?)?;
            acquire::cmd::fetch(
                db.as_ref(),
                &cfg,
                &creds,
                acquire::cmd::FetchArgs {
                    target,
                    offer,
                    out,
                    src_track_id,
                    format_pref: format,
                    overwrite,
                    lock,
                },
            )?
        }
        Cmd::Pending {
            list,
            apply,
            import,
            yes,
            clear,
        } => {
            let cfg = Config::load(&config_path)?;
            let action = match (clear, list) {
                (Some(id), _) => acquire::cmd::PendingAction::Clear { id },
                (None, true) => acquire::cmd::PendingAction::List,
                (None, false) => acquire::cmd::PendingAction::Apply {
                    dry_run: !apply,
                    import,
                    yes,
                },
            };
            acquire::cmd::pending(
                db.as_mut().expect("pending needs the db"),
                &cfg,
                safety,
                action,
            )?
        }
        Cmd::Shop {
            query,
            track_id,
            match_query,
            match_max,
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
                    match_query,
                    match_max,
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
            let query = query::join_argv(&query);
            dump::run(
                db.as_ref().expect("dump needs the db"),
                Some(query.trim()).filter(|q| !q.is_empty()),
                limit,
            )?
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

struct ImportArgs {
    files: Vec<PathBuf>,
    title: Option<String>,
    artist: Option<String>,
    src_track_id: Option<String>,
    lock: bool,
    apply: bool,
    yes: bool,
    undo: Option<String>,
}

fn run_import(db: &mut MasterDb, cfg: &Config, safety: SafetyOpts, args: ImportArgs) -> Result<()> {
    use owo_colors::OwoColorize;
    use rekord_ripper::{audio, import};

    if let Some(id) = &args.undo {
        return undo_import(db, cfg, safety, id, args.apply, args.yes);
    }
    if args.files.is_empty() {
        anyhow::bail!("give me at least one audio file to import");
    }
    let single = args.files.len() == 1;
    if !single && (args.title.is_some() || args.artist.is_some() || args.src_track_id.is_some()) {
        anyhow::bail!("--title, --artist and --src-track-id only make sense with one file");
    }

    // Gate 1: the config opt-in. Checked before anything is probed so a user who
    // has not opted in gets told, not a wall of output.
    if args.apply && !cfg.import.insert_content_rows {
        anyhow::bail!(
            "creating rekordbox rows is off. Set `insert_content_rows = true` under \
             [import] in {} first — see `rekord-ripper config`.",
            paths::config_path(None)?.display()
        );
    }

    let mut planned = Vec::new();
    for path in &args.files {
        let info = audio::probe(path)?;
        let new = import::plan_insert(
            db,
            path,
            &info,
            args.title.as_deref(),
            args.artist.as_deref(),
        )?;
        println!("{}", import::render(&new));
        println!(
            "  {:<18} {}",
            "AnalysisDataPath",
            format!("{} (after the transfer)", import::anlz_path_for(&new)).dimmed()
        );
        println!();
        planned.push(new);
    }
    // Several files by the same new artist mint one artist row between them,
    // not one each — planning happens before any of it is inserted.
    import::dedupe_lookups(&mut planned);

    if !args.apply {
        eprintln!(
            "{} plan for {} file(s). Dry-run; pass --apply to write.",
            "ok:".green(),
            planned.len()
        );
        return Ok(());
    }

    // Gate 2: an explicit yes, having seen the rows.
    if !args.yes && !confirm_stdin(&format!("insert {} row(s) into master.db?", planned.len()))? {
        println!("cancelled.");
        return Ok(());
    }

    // Gate 3: the same refusal and backup discipline as cp and auto.
    db::safety_preflight(safety)?;
    let backup = db.backup()?;
    eprintln!("backed up to: {}", backup.display());

    for new in &planned {
        let mut note = rekord_ripper::import::insert(db, new)?;
        note.backup = Some(backup.to_string_lossy().into_owned());
        let note_path = note.write_beside(&backup)?;
        eprintln!("{} track {} — {}", "inserted:".green(), new.id, new.title);
        eprintln!(
            "  undo with: {}",
            format!("rekord-ripper import --undo {} --apply", new.id).bold()
        );
        eprintln!("  {}", format!("note: {}", note_path.display()).dimmed());
    }

    // The payoff: the row exists now, so the transfer can run immediately
    // instead of waiting for a manual drag.
    if let (Some(src_id), Some(new)) = (&args.src_track_id, planned.first()) {
        transfer_onto_import(db, cfg, safety, src_id, new, args.lock)?;
    }
    Ok(())
}

fn run_repair(
    db: &mut MasterDb,
    safety: SafetyOpts,
    apply: bool,
    yes: bool,
    unplayable_only: bool,
) -> Result<()> {
    use owo_colors::OwoColorize;
    use rekord_ripper::import::{self, file_type_name};

    eprintln!("probing every local track …");
    let mut fixes = import::scan_file_types(db)?;
    if unplayable_only {
        fixes.retain(|f| f.unplayable);
    }
    if fixes.is_empty() {
        eprintln!("{} every FileType matches its file.", "ok:".green());
        return Ok(());
    }

    let broken = fixes.iter().filter(|f| f.unplayable).count();
    for fix in &fixes {
        let arrow = format!(
            "{} -> {}",
            file_type_name(fix.current),
            file_type_name(Some(fix.correct))
        );
        let name = std::path::Path::new(&fix.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| fix.path.clone());
        if fix.unplayable {
            println!("  {} {:<28} {}", "unplayable".red(), arrow, name);
        } else {
            println!("  {} {:<28} {}", "mislabelled".yellow(), arrow, name);
        }
    }
    println!();
    eprintln!(
        "{} row(s) to correct, {broken} of them unplayable today.",
        fixes.len()
    );

    if !apply {
        eprintln!("Dry-run; pass --apply to write.");
        return Ok(());
    }
    if !yes && !confirm_stdin(&format!("correct {} row(s) in master.db?", fixes.len()))? {
        println!("cancelled.");
        return Ok(());
    }

    db::safety_preflight(safety)?;
    let backup = db.backup()?;
    eprintln!("backed up to: {}", backup.display());
    let n = import::apply_file_type_fixes(db, &fixes)?;
    eprintln!("{} corrected {n} row(s).", "ok:".green());
    eprintln!("rekordbox must be restarted to re-read them.");
    Ok(())
}

/// Fingerprint-gate and apply an analysis transfer onto a freshly imported row.
fn transfer_onto_import(
    db: &mut MasterDb,
    cfg: &Config,
    safety: SafetyOpts,
    src_id: &str,
    new: &rekord_ripper::import::NewContent,
    lock: bool,
) -> Result<()> {
    use owo_colors::OwoColorize;
    use rekord_ripper::transfer;

    let src = analysis::load_track(db, src_id)?;
    let dst_path = std::path::Path::new(&new.folder_path);

    eprintln!();
    eprintln!("checking the fingerprint before transferring …");
    // BPM is NULL on a fresh row, so only duration evidence is available.
    let outcome = transfer::gate(&src, dst_path, Some(new.length), None, cfg)?;
    if !outcome.verdict.is_accept() {
        eprintln!("{} {}", "fp REJECT".red(), outcome.verdict.summary());
        eprintln!(
            "the track was imported, but no analysis was copied. \
             Transfer by hand with `cp` if you disagree."
        );
        // Non-zero: the import succeeded, the transfer did not.
        std::process::exit(2);
    }

    let plan = analysis::build_plan(
        db,
        src_id,
        &new.id,
        &CopyOpts {
            // A brand-new row has no cues, so there is nothing to replace.
            replace: false,
            lock,
        },
    )?;
    println!("{}", transfer::report(&plan, &outcome.verdict));

    db::safety_preflight(safety)?;
    let backup = analysis::apply_plan(db, &plan)?;
    eprintln!("backed up to: {}", backup.display());
    eprintln!("applied: {} → {}", plan.src.id, plan.dst.id);
    Ok(())
}

fn undo_import(
    db: &mut MasterDb,
    _cfg: &Config,
    safety: SafetyOpts,
    id: &str,
    apply: bool,
    yes: bool,
) -> Result<()> {
    use owo_colors::OwoColorize;

    let track = analysis::load_track(db, id)?;
    println!(
        "would mark track {id} deleted: {} — {}",
        track.artist.as_deref().unwrap_or("?"),
        track.title.as_deref().unwrap_or("?")
    );
    println!(
        "  {}",
        "sets rb_local_deleted = 1 with a fresh USN, so the removal syncs to your \
         other devices rather than leaving them a row for a file they lack."
            .dimmed()
    );
    if !apply {
        eprintln!("dry-run; pass --apply to write.");
        return Ok(());
    }
    if !yes && !confirm_stdin(&format!("mark track {id} deleted?"))? {
        println!("cancelled.");
        return Ok(());
    }

    db::safety_preflight(safety)?;
    let backup = db.backup()?;
    eprintln!("backed up to: {}", backup.display());
    rekord_ripper::import::tombstone(db, id, Some(&track.uuid))?;
    eprintln!("{} track {id} marked deleted.", "ok:".green());
    Ok(())
}

fn confirm_stdin(question: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!("not a terminal — pass --yes to confirm non-interactively");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn run_fp(a: &std::path::Path, b: &std::path::Path, secs: u32, cfg: &Config) -> Result<()> {
    use rekord_ripper::fingerprint as fp;

    let t = fp::Thresholds {
        score_max: cfg.fingerprint.score_max,
        coverage_min: cfg.fingerprint.coverage_min,
        shift_items_max: cfg.fingerprint.shift_items_max,
        speed_ratio_tol: cfg.fingerprint.speed_ratio_tol,
    };

    eprintln!("fingerprinting {} ...", a.display());
    let fa = fp::fingerprint_file(a, secs)?;
    eprintln!("fingerprinting {} ...", b.display());
    let fb = fp::fingerprint_file(b, secs)?;

    // ffprobe durations are the cheap, independent speed check — much finer than
    // the whole-second Length rekordbox stores.
    let durations = match (fp::probe_duration_secs(a), fp::probe_duration_secs(b)) {
        (Ok(x), Ok(y)) => {
            println!("ffprobe    A {x:.3}s   B {y:.3}s   ratio {:.5}", x / y);
            Some((x, y))
        }
        _ => {
            eprintln!("warning: ffprobe gave no duration; the speed check is skipped");
            None
        }
    };

    print!("{}", fp::debug_report(&fa, &fb)?);

    let speed = fp::SpeedEvidence {
        durations,
        bpms: None,
    };
    let verdict = fp::compare(&fa, &fb, speed, &t)?;
    println!();
    println!(
        "thresholds score_max {:.2}  coverage_min {:.2}  shift_items_max {}  speed_tol {:.4}",
        t.score_max, t.coverage_min, t.shift_items_max, t.speed_ratio_tol
    );
    println!(
        "VERDICT    {}  {}",
        if verdict.is_accept() {
            "ACCEPT"
        } else {
            "REJECT"
        },
        verdict.summary()
    );
    if !verdict.is_accept() {
        // Non-zero so a calibration script can tell the outcomes apart.
        std::process::exit(2);
    }
    Ok(())
}

/// True when this command reads `master.db`.
fn needs_database(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Backends | Cmd::Config { .. } | Cmd::Fp { .. } => false,
        // Only needed to seed the query from an existing track.
        Cmd::Shop {
            track_id,
            match_query,
            ..
        } => !track_id.is_empty() || match_query.is_some(),
        Cmd::Buy { track_id, .. } => track_id.is_some(),
        // Only needed to queue a transfer against an existing track.
        Cmd::Fetch { src_track_id, .. } => src_track_id.is_some(),
        Cmd::Dump { .. }
        | Cmd::Tui
        | Cmd::Cp { .. }
        | Cmd::Auto { .. }
        | Cmd::Pending { .. }
        | Cmd::Import { .. }
        | Cmd::Repair { .. } => true,
    }
}

struct ShopArgs {
    query: String,
    track_id: Vec<String>,
    match_query: Option<String>,
    match_max: usize,
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

    // `--match` is just a way of naming track ids, so it resolves to some and
    // then takes the same path — one spec builder, one set of labels.
    let mut track_ids = args.track_id.clone();
    if let Some(q) = &args.match_query {
        let db = db.ok_or_else(|| anyhow::anyhow!("--match needs the rekordbox database"))?;
        let hits = rekord_ripper::select::hits(db, q, args.match_max)?;
        eprintln!("--match {q:?} selected {} track(s):", hits.len());
        for h in &hits {
            let artist = if h.artist.is_empty() { "?" } else { &h.artist };
            eprintln!("  {} — {}", artist, h.title);
        }
        track_ids.extend(hits.into_iter().map(|h| h.id));
    }

    // Seed from a local track when asked, so you don't retype what rekordbox
    // already knows.
    // One spec per thing to look for. A single search is one spec, so bulk needs
    // no separate code path here either.
    let mut specs: Vec<shop::QuerySpec> = Vec::new();
    if !track_ids.is_empty() {
        let db = db.ok_or_else(|| anyhow::anyhow!("--track-id needs the rekordbox database"))?;
        for id in &track_ids {
            let t = analysis::load_track(db, id)?;
            let Some(title) = t.title.clone().filter(|s| !s.trim().is_empty()) else {
                eprintln!("skipping track {id}: no title to search for");
                continue;
            };
            specs.push(shop::QuerySpec {
                label: format!("{} — {title}", t.artist.as_deref().unwrap_or("?")),
                src_id: Some(id.clone()),
                query: acquire::SearchQuery {
                    title,
                    artist: t.artist.clone(),
                    duration_secs: t.length,
                    limit,
                    ..Default::default()
                },
            });
        }
        if specs.is_empty() {
            anyhow::bail!("none of the given tracks had a title to search for");
        }
    } else if args.query.trim().is_empty() {
        anyhow::bail!("give me something to search for, or pass --track-id");
    } else {
        let text = args.query.trim();
        specs.push(shop::QuerySpec {
            label: text.to_string(),
            src_id: None,
            query: acquire::SearchQuery::from_text(text, limit),
        });
    }

    // A price threshold across currencies is not computable here, so refuse it
    // rather than silently comparing incomparable numbers.
    if let Some(c) = &args.currency
        && c.trim().len() != 3
    {
        anyhow::bail!("--currency takes a 3-letter ISO code, e.g. GBP");
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

    let multi = specs.len() > 1;
    let groups = shop::search_many(&reg, &specs, &opts, |done, total, label| {
        if label.is_empty() {
            return;
        }
        if total > 1 {
            eprintln!("[{}/{total}] {label}", done + 1);
        } else {
            eprintln!("searching for: {label}");
        }
    });

    if args.json {
        println!("{}", shop_json_groups(&groups)?);
    } else {
        for g in &groups {
            if multi {
                println!();
                println!("for: {}", g.label);
            }
            print!("{}", acquire::render::table(&g.outcome));
        }
    }

    if args.strict {
        for g in &groups {
            if let Some(first) = g.outcome.failures().next() {
                anyhow::bail!(
                    "{} failed and --strict was given: {}",
                    first.backend,
                    first.error.as_ref().expect("failures() filters on error")
                );
            }
        }
    }
    if !groups.is_empty() && groups.iter().all(|g| g.outcome.total_failure()) {
        anyhow::bail!("every backend failed — see the errors above");
    }
    Ok(())
}

/// Machine-readable grouped outcome, so a bulk run is scriptable.
fn shop_json_groups(groups: &[acquire::shop::GroupOutcome]) -> Result<String> {
    let mut out = Vec::new();
    for g in groups {
        let mut v: serde_json::Value = serde_json::from_str(&shop_json(&g.outcome)?)?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("label".into(), serde_json::json!(g.label));
            obj.insert("src_track_id".into(), serde_json::json!(g.src_id));
        }
        out.push(v);
    }
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "groups": out,
        "currency_note": "prices are in each seller's own currency and are never converted",
    }))?)
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
        "note: secrets go in {} (mode 600), not here — the Bandcamp identity cookie \
         and the slskd API key.",
        paths::credentials_path()?.display()
    );
    eprintln!(
        "note: soulseek needs [soulseek] url pointing at an slskd instance; it is never \
         started for you."
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
        eprintln!("skip {} ← {}: {e}", m.dst_id, m.src_id);
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
