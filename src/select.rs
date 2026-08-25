//! Pick tracks out of the library with the filter language.

use anyhow::{Result, bail};

use crate::db::MasterDb;
use crate::library::load_rows;
use crate::query::Query;

/// One matched track, with the little the callers need.
pub struct Hit {
    pub id: String,
    pub title: String,
    pub artist: String,
}

/// Track ids matching `query`, in library order.
///
/// Refuses an unbounded result rather than trimming to `max` quietly. Every
/// track here becomes a fan-out across every backend, so the difference between
/// 25 and 2500 is the difference between a minute and a rate limit — that is a
/// decision to hand back, not to make silently.
pub fn hits(db: &MasterDb, query: &str, max: usize) -> Result<Vec<Hit>> {
    let parsed = Query::parse(query);
    if parsed.is_empty() {
        bail!("--match needs a query that actually filters something");
    }

    let rows = load_rows(db)?;
    let matched: Vec<Hit> = rows
        .iter()
        .filter(|r| parsed.matches(r.fields()))
        .map(|r| Hit {
            id: r.id.clone(),
            title: r.title.clone(),
            artist: r.artist.clone(),
        })
        .collect();

    if matched.is_empty() {
        bail!("--match {query:?} matched no tracks");
    }
    if matched.len() > max {
        bail!(
            "--match {query:?} matched {} tracks, over the {max} cap. \
             Narrow it, or raise --match-max.",
            matched.len()
        );
    }
    Ok(matched)
}
