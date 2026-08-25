//! Choosing an offer.
//!
//! Plain stdin rather than ratatui: the TUI event loop is a single-threaded
//! blocking poll with no worker thread, so a multi-second fan-out called from a
//! key handler would freeze the screen. The offer table is a one-shot static
//! comparison that prints perfectly well, and this keeps the whole thing
//! testable by feeding it a string.

use std::io::{BufRead, Write};

use anyhow::{Result, bail};

use super::shop::{RankedOffer, SearchOutcome};
use super::types::{BackendId, ItemRef};

/// How the user identified an offer without being prompted.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    /// Fully explicit and stable across runs — the form scripts should use.
    pub offer: Option<ItemRef>,
    /// Top-ranked offer from this backend.
    pub from: Option<BackendId>,
    /// Printed row number. Convenient for a human's follow-up command, wrong for
    /// anything automated: row numbers move with network results.
    pub row: Option<usize>,
}

impl Selector {
    pub fn is_empty(&self) -> bool {
        self.offer.is_none() && self.from.is_none() && self.row.is_none()
    }
}

/// What a typed line asked for.
#[derive(Debug, PartialEq)]
pub enum Choice {
    Row(usize),
    Open(usize),
    Quit,
}

/// Interpret one line of input against a table of `max` rows.
///
/// `""` (a bare return) is `Quit`, deliberately: an empty line must never be
/// read as "yes, buy the first one".
pub fn parse_choice(line: &str, max: usize) -> Result<Choice> {
    let s = line.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("q") || s.eq_ignore_ascii_case("quit") {
        return Ok(Choice::Quit);
    }
    // `o3` / `o 3` opens row 3 in a browser instead of selecting it.
    if let Some(rest) = s.strip_prefix(['o', 'O']) {
        let n = rest.trim();
        let n = if n.is_empty() { "1" } else { n };
        let row = n.parse::<usize>().ok().filter(|r| *r >= 1 && *r <= max);
        return match row {
            Some(r) => Ok(Choice::Open(r)),
            None => bail!("'{s}' is not a row between 1 and {max}"),
        };
    }
    match s.parse::<usize>() {
        Ok(r) if r >= 1 && r <= max => Ok(Choice::Row(r)),
        _ => bail!("'{s}' is not a row between 1 and {max}"),
    }
}

/// Resolve a non-interactive selector against an outcome.
pub fn resolve<'a>(out: &'a SearchOutcome, sel: &Selector) -> Result<&'a RankedOffer> {
    if let Some(item) = &sel.offer {
        return out
            .by_ref(item)
            .ok_or_else(|| anyhow::anyhow!("{item} is not in these results"));
    }
    if let Some(backend) = sel.from {
        return out
            .top_from(backend)
            .ok_or_else(|| anyhow::anyhow!("{backend} returned no offers"));
    }
    if let Some(row) = sel.row {
        return out
            .by_row(row)
            .ok_or_else(|| anyhow::anyhow!("there is no row {row}"));
    }
    bail!("nothing selected")
}

/// Prompt for a row. Requires a terminal.
///
/// Refuses rather than auto-picking when stdin is not a TTY: silently choosing
/// row 1 in a script would spend money without anyone looking.
pub fn prompt(out: &SearchOutcome) -> Result<Option<Choice>> {
    if out.offers.is_empty() {
        return Ok(None);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!(
            "not a terminal — pass --offer <ref> (stable), --from <backend>, or --pick <n> \
             to choose without a prompt"
        );
    }
    let max = out.offers.len();
    print!("pick a row [1-{max}], o<n> to open in a browser, q to quit: ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(Some(parse_choice(&line, max)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::shop::RankedOffer;
    use crate::acquire::types::*;

    fn outcome() -> SearchOutcome {
        let mk = |backend, n: usize| RankedOffer {
            offer: Offer::new(
                ItemRef::new(backend, format!("t:{n}")),
                ItemKind::Track,
                "A",
                format!("T{n}"),
                "https://example.test/x",
            ),
            row: n,
            match_score: 50,
        };
        SearchOutcome {
            offers: vec![mk(BackendId::Bandcamp, 1), mk(BackendId::SoundCloud, 2)],
            per_backend: vec![],
        }
    }

    #[test]
    fn parses_a_row_number() {
        assert_eq!(parse_choice("2", 3).unwrap(), Choice::Row(2));
        assert_eq!(parse_choice(" 1 \n", 3).unwrap(), Choice::Row(1));
    }

    #[test]
    fn an_empty_line_quits_rather_than_buying_the_first_row() {
        // A bare return must never be read as consent to spend money.
        assert_eq!(parse_choice("", 3).unwrap(), Choice::Quit);
        assert_eq!(parse_choice("\n", 3).unwrap(), Choice::Quit);
        assert_eq!(parse_choice("q", 3).unwrap(), Choice::Quit);
        assert_eq!(parse_choice("QUIT", 3).unwrap(), Choice::Quit);
    }

    #[test]
    fn parses_the_open_in_browser_form() {
        assert_eq!(parse_choice("o2", 3).unwrap(), Choice::Open(2));
        assert_eq!(parse_choice("o 2", 3).unwrap(), Choice::Open(2));
        assert_eq!(parse_choice("o", 3).unwrap(), Choice::Open(1));
    }

    #[test]
    fn rejects_out_of_range_and_nonsense_input() {
        for bad in ["0", "4", "-1", "abc", "o9", "1.5"] {
            assert!(parse_choice(bad, 3).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn an_explicit_ref_resolves_and_is_preferred_over_other_selectors() {
        let out = outcome();
        let sel = Selector {
            offer: Some(ItemRef::new(BackendId::SoundCloud, "t:2")),
            // These would pick row 1; the explicit ref must win.
            from: Some(BackendId::Bandcamp),
            row: Some(1),
        };
        assert_eq!(resolve(&out, &sel).unwrap().row, 2);
    }

    #[test]
    fn from_backend_picks_that_backends_best_row() {
        let out = outcome();
        let sel = Selector {
            from: Some(BackendId::SoundCloud),
            ..Default::default()
        };
        assert_eq!(
            resolve(&out, &sel).unwrap().offer.backend(),
            BackendId::SoundCloud
        );
    }

    #[test]
    fn row_selection_resolves_by_printed_number() {
        let out = outcome();
        let sel = Selector {
            row: Some(2),
            ..Default::default()
        };
        assert_eq!(resolve(&out, &sel).unwrap().row, 2);
    }

    #[test]
    fn unresolvable_selectors_name_what_was_missing() {
        let out = outcome();
        let err = resolve(
            &out,
            &Selector {
                offer: Some(ItemRef::new(BackendId::Bandcamp, "t:999")),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not in these results"), "got {err}");

        let err = resolve(&out, &Selector::default()).unwrap_err().to_string();
        assert!(err.contains("nothing selected"), "got {err}");
    }

    #[test]
    fn an_empty_selector_is_recognised_as_needing_a_prompt() {
        assert!(Selector::default().is_empty());
        assert!(
            !Selector {
                row: Some(1),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn prompting_with_no_offers_returns_nothing_rather_than_erroring() {
        let empty = SearchOutcome {
            offers: vec![],
            per_backend: vec![],
        };
        assert!(prompt(&empty).unwrap().is_none());
    }
}
