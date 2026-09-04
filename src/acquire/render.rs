//! Rendering the offer table.
//!
//! Kept free of ratatui so it stays testable, in the same spirit as the note at
//! the top of `tui/diff.rs`. Styling follows `dump.rs` and `Plan::render`.
//!
//! The currency rules are load-bearing, not cosmetic: a price is always printed
//! with its ISO code, and any "cheapest" line is per-currency and carries the
//! reason it cannot be a single number.

use owo_colors::OwoColorize;

use super::shop::{RankedOffer, SearchOutcome, cheapest_per_currency};
use super::types::*;

/// Truncate to `max` display columns, ending in `…` when shortened.
fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// How a price renders in the table.
pub fn price_cell(o: &Offer) -> String {
    match &o.pricing {
        // Never "free" and never "0.00" — we simply have not looked.
        Pricing::Unprobed => "?".into(),
        Pricing::Free => "free".into(),
        Pricing::NameYourPrice { minimum: None } => "name your price".into(),
        Pricing::NameYourPrice { minimum: Some(p) } => format!("{p}+"),
        Pricing::Flat(p) => p.to_string(),
        Pricing::PerFormat(offers) => match offers
            .iter()
            .filter_map(|f| f.price.as_ref())
            .min_by_key(|p| p.amount_minor)
        {
            Some(p) => format!("from {p}"),
            None => "?".into(),
        },
        Pricing::Unavailable { reason } => match reason {
            Some(r) => format!("n/a ({})", clip(r, 22)),
            None => "n/a".into(),
        },
    }
}

/// Available formats, best first, or `?` when unprobed.
pub fn format_cell(o: &Offer) -> String {
    match &o.formats {
        None => "?".into(),
        Some(fs) if fs.is_empty() => "none".into(),
        Some(fs) => {
            let mut sorted: Vec<&AudioFormat> = fs.iter().collect();
            sorted.sort_by_key(|f| std::cmp::Reverse(f.quality_rank()));
            sorted
                .iter()
                .take(3)
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

pub fn ownership_cell(o: &Offer) -> &'static str {
    match o.ownership {
        Ownership::Unknown => "?",
        Ownership::NotApplicable => "n/a",
        Ownership::No => "no",
        Ownership::Yes { .. } => "yes",
    }
}

/// The full table, plus the per-backend and currency footers.
pub fn table(out: &SearchOutcome) -> String {
    let mut s = String::new();

    if out.offers.is_empty() {
        s.push_str(&format!("{}\n", "no offers found.".dimmed()));
    } else {
        s.push_str(&format!(
            "{:>3}  {:<11} {:<44} {:<20} {:<17} {:<4} {}\n",
            "#".bold(),
            "backend".bold(),
            "artist / title".bold(),
            "formats".bold(),
            "price".bold(),
            "own".bold(),
            "match".bold()
        ));
        for r in &out.offers {
            s.push_str(&row(r));
        }
    }

    s.push('\n');
    s.push_str(&backend_footer(out));
    s.push_str(&currency_footer(&out.offers));
    s
}

fn row(r: &RankedOffer) -> String {
    let o = &r.offer;
    let who = format!("{} — {}", o.artist, o.title);
    let album = match (&o.album, o.kind) {
        (Some(a), _) => format!("  {}", clip(&format!("alb: {a}"), 42).dimmed()),
        (None, ItemKind::Album) => format!("  {}", "(album)".dimmed()),
        _ => String::new(),
    };

    let mut line = format!(
        "{:>3}  {:<11} {:<44} {:<20} {:<17} {:<4} {:>3}\n",
        r.row,
        o.backend().to_string(),
        clip(&who, 44),
        format_cell(o),
        price_cell(o),
        ownership_cell(o),
        r.match_score
    );
    if !album.is_empty() {
        line.push_str(&format!("     {}\n", album.trim_start()));
    }
    // A partial row must look partial rather than quietly wrong.
    if let Some(e) = &o.enrich_error {
        line.push_str(&format!("     {} {}\n", "!".yellow(), clip(e, 92).dimmed()));
    }
    line
}

fn backend_footer(out: &SearchOutcome) -> String {
    let mut s = String::new();
    for r in &out.per_backend {
        match &r.error {
            Some(e) if e.is_silently_skippable() => {}
            Some(e) => s.push_str(&format!("{} {}: {}\n", "degraded:".yellow(), r.backend, e)),
            None => {}
        }
    }
    let timings: Vec<String> = out
        .per_backend
        .iter()
        .map(|r| {
            format!(
                "{} {}ms/{} hits",
                r.backend,
                r.elapsed.as_millis(),
                r.raw_hits
            )
        })
        .collect();
    if !timings.is_empty() {
        s.push_str(&format!("{}\n", timings.join("   ").dimmed()));
    }
    s
}

/// The cheapest offer per currency.
fn currency_footer(offers: &[RankedOffer]) -> String {
    let cheap = cheapest_per_currency(offers);
    if cheap.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = cheap.iter().map(|(p, b)| format!("{p} ({b})")).collect();
    format!("cheapest per currency: {}\n", parts.join("   "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::error::BackendError;
    use crate::acquire::shop::{BackendReport, RankedOffer, SearchOutcome};
    use std::time::Duration;

    fn ranked(o: Offer, row: usize) -> RankedOffer {
        RankedOffer {
            offer: o,
            row,
            match_score: 90,
        }
    }

    fn offer() -> Offer {
        Offer::new(
            ItemRef::new(BackendId::Bandcamp, "a:1"),
            ItemKind::Album,
            "Burial",
            "Untrue",
            "https://burial.bandcamp.com/album/untrue",
        )
    }

    #[test]
    fn an_unprobed_price_renders_as_unknown_never_as_free() {
        // The single most important rendering rule: do not imply a price we have
        // not looked up.
        let cell = price_cell(&offer());
        assert_eq!(cell, "?");
        assert!(!cell.contains("free"));
        assert!(!cell.contains('0'));
    }

    #[test]
    fn prices_always_carry_their_currency_code() {
        let mut o = offer();
        o.pricing = Pricing::Flat(Price::new(850, "GBP"));
        assert_eq!(price_cell(&o), "8.50 GBP");

        o.pricing = Pricing::NameYourPrice {
            minimum: Some(Price::new(400, "GBP")),
        };
        assert_eq!(price_cell(&o), "4.00 GBP+", "the + marks it as a minimum");
    }

    #[test]
    fn name_your_price_with_no_floor_says_so() {
        let mut o = offer();
        o.pricing = Pricing::NameYourPrice { minimum: None };
        assert_eq!(price_cell(&o), "name your price");
    }

    #[test]
    fn an_unavailable_offer_says_why() {
        let mut o = offer();
        o.pricing = Pricing::Unavailable {
            reason: Some("no digital download offered".into()),
        };
        assert!(price_cell(&o).starts_with("n/a ("));
    }

    #[test]
    fn per_format_pricing_shows_the_lowest_as_a_from_price() {
        let mut o = offer();
        o.pricing = Pricing::PerFormat(vec![
            FormatOffer {
                format: AudioFormat::Flac,
                price: Some(Price::new(1200, "GBP")),
            },
            FormatOffer {
                format: AudioFormat::Mp3(Some(320)),
                price: Some(Price::new(700, "GBP")),
            },
        ]);
        assert_eq!(price_cell(&o), "from 7.00 GBP");
    }

    #[test]
    fn formats_render_best_first_and_unprobed_as_unknown() {
        let mut o = offer();
        assert_eq!(format_cell(&o), "?");
        o.formats = Some(vec![AudioFormat::Mp3(Some(320)), AudioFormat::Flac]);
        assert!(
            format_cell(&o).starts_with("FLAC"),
            "got {}",
            format_cell(&o)
        );
        o.formats = Some(vec![]);
        assert_eq!(format_cell(&o), "none", "probed but nothing usable");
    }

    #[test]
    fn ownership_distinguishes_unknown_from_no() {
        let mut o = offer();
        assert_eq!(ownership_cell(&o), "?");
        o.ownership = Ownership::No;
        assert_eq!(ownership_cell(&o), "no");
        o.ownership = Ownership::Yes {
            redownloadable: true,
        };
        assert_eq!(ownership_cell(&o), "yes");
        o.ownership = Ownership::NotApplicable;
        assert_eq!(ownership_cell(&o), "n/a");
    }

    #[test]
    fn mixed_currencies_are_never_declared_a_single_cheapest() {
        let mut gbp = offer();
        gbp.pricing = Pricing::Flat(Price::new(400, "GBP"));
        let mut usd = offer();
        usd.item_ref = ItemRef::new(BackendId::Bandcamp, "a:2");
        usd.pricing = Pricing::Flat(Price::new(129, "USD"));

        let out = SearchOutcome {
            offers: vec![ranked(gbp, 1), ranked(usd, 2)],
            per_backend: vec![],
        };
        let t = table(&out);
        assert!(t.contains("4.00 GBP"), "{t}");
        assert!(
            t.contains("1.29 USD"),
            "each currency gets its own entry rather than one overall winner:\n{t}"
        );
    }

    #[test]
    fn a_single_currency_table_still_reports_the_cheapest() {
        let mut a = offer();
        a.pricing = Pricing::Flat(Price::new(400, "GBP"));
        let out = SearchOutcome {
            offers: vec![ranked(a, 1)],
            per_backend: vec![],
        };
        let t = table(&out);
        assert!(t.contains("cheapest per currency"), "{t}");
        assert!(t.contains("4.00 GBP"), "{t}");
    }

    #[test]
    fn a_failed_backend_is_shown_as_degraded_not_hidden() {
        let out = SearchOutcome {
            offers: vec![ranked(offer(), 1)],
            per_backend: vec![BackendReport {
                backend: BackendId::SoundCloud,
                elapsed: Duration::from_millis(120),
                raw_hits: 0,
                enriched: 0,
                error: Some(BackendError::Network {
                    backend: BackendId::SoundCloud,
                    detail: "dns failure".into(),
                }),
                enrich_errors: 0,
            }],
        };
        let t = table(&out);
        assert!(t.contains("degraded:"), "{t}");
        assert!(t.contains("dns failure"), "{t}");
        // The surviving offer is still shown.
        assert!(t.contains("Untrue"), "{t}");
    }

    #[test]
    fn an_unsupported_backend_is_not_reported_as_degraded() {
        // "can't do that" is not a failure worth a warning line.
        let out = SearchOutcome {
            offers: vec![],
            per_backend: vec![BackendReport {
                backend: BackendId::SoundCloud,
                elapsed: Duration::from_millis(1),
                raw_hits: 0,
                enriched: 0,
                error: Some(BackendError::unsupported(BackendId::SoundCloud, "search")),
                enrich_errors: 0,
            }],
        };
        assert!(!table(&out).contains("degraded:"));
    }

    #[test]
    fn a_per_offer_enrich_failure_is_visible_on_its_row() {
        let mut o = offer();
        o.enrich_error = Some("item page 404".into());
        let out = SearchOutcome {
            offers: vec![ranked(o, 1)],
            per_backend: vec![],
        };
        assert!(table(&out).contains("item page 404"));
    }

    #[test]
    fn an_empty_table_says_so_rather_than_printing_a_bare_header() {
        let out = SearchOutcome {
            offers: vec![],
            per_backend: vec![],
        };
        assert!(table(&out).contains("no offers found"));
    }

    #[test]
    fn clip_shortens_with_an_ellipsis_and_respects_char_boundaries() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        // Must not panic on multi-byte input.
        assert_eq!(clip("ünïcödé-title", 6).chars().count(), 6);
    }
}
