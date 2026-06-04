use anyhow::Result;
use clap::{Parser, Subcommand};

use rekord_ripper::db::MasterDb;
use rekord_ripper::dump;

#[derive(Parser)]
#[command(name = "rekord-ripper", version, about = "Rekordbox analysis utility")]
struct Cli {
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db = MasterDb::open()?;
    match cli.cmd {
        Cmd::Dump { query, limit } => dump::run(&db, query.as_deref(), limit)?,
    }
    Ok(())
}
