//! Fan-out search and the ranked offer table.
//!
//! # Why threads, and why they cannot hang
//!
//! Sequential search costs the *sum* of every backend's latency: Bandcamp's
//! endpoint plus a `yt-dlp` process spawn is comfortably ten seconds for two
//! backends, and it grows linearly with every backend added. `std::thread::scope`
//! gives borrowed access to the registry with no `Arc`, no `'static` bound, and no
//! channel — the smallest possible threading footprint, and the first threads in
//! this crate.
//!
//! The catch: `scope` joins every thread and **cannot abandon one**. So a hung
//! backend would hang the table. The fix is to make hanging impossible — the HTTP
//! agent carries a global timeout and child processes go through
//! `proc::run_with_deadline` — *not* to detach `'static` threads and walk away
//! with `recv_timeout`, which leaks a thread that is still writing to the network
//! and to disk, and can leave a truncated file behind after the process exits.
//!
//! # Why price is never a sort key
//!
//! Bandcamp prices arrive in the seller's currency and there is no exchange-rate
//! source here. Ordering two prices in different currencies by amount would be
//! meaningless, so currency is a *grouping* key and amounts are only ever
//! compared within one currency.

use std::time::{Duration, Instant};

use super::error::BackendError;
use super::types::*;
use super::{AcquisitionBackend, Registry};
use crate::analysis::{artist_matches, normalize_title};

#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub timeout: Duration,
    /// Offers per backend to price-probe. 0 skips probing entirely.
    pub enrich_top_n: usize,
    /// Restrict to these backends. Empty means all enabled.
    pub only: Vec<BackendId>,
    pub lossless_only: bool,
    /// Show only offers priced in this ISO currency code.
    pub currency: Option<String>,
    pub sort: SortMode,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            enrich_top_n: 5,
            only: Vec::new(),
            lossless_only: false,
            currency: None,
            sort: SortMode::Quality,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SortMode {
    /// Lossless first. The point of the feature is getting a lossless file, and a
    /// cheaper MP3 is not a cheaper version of the same thing.
    #[default]
    Quality,
    /// Cheapest cost class first, for when price matters more than format.
    Cost,
}

/// One offer positioned in the table. Backends never build this — they emit
/// `Offer` and the orchestrator scores and sorts.
#[derive(Debug, Clone)]
pub struct RankedOffer {
    pub offer: Offer,
    /// 1-based printed row. Valid only for this invocation, which is why
    /// `--pick` is documented as human-only and `--offer <ref>` is the
    /// scriptable handle.
    pub row: usize,
    /// 0..=100 textual similarity to the local track.
    pub match_score: u8,
}

/// What one backend did.
#[derive(Debug)]
pub struct BackendReport {
    pub backend: BackendId,
    pub elapsed: Duration,
    pub raw_hits: usize,
    pub enriched: usize,
    /// Whole-backend failure. Offers from before it may still be present.
    pub error: Option<BackendError>,
    /// Per-offer enrich failures, already mirrored into `Offer::enrich_error`.
    pub enrich_errors: usize,
}

/// One thing to search for, with enough context to label its results.
///
/// Bulk and single search share this: a single search is just one spec, so there
/// is no second code path to keep in step.
#[derive(Debug, Clone)]
pub struct QuerySpec {
    /// What to show above the results, e.g. `Artist — Title`.
    pub label: String,
    /// The local track this was searched for, so a download can be paired to it.
    pub src_id: Option<String>,
    pub query: SearchQuery,
}

/// The results for one spec.
#[derive(Debug)]
pub struct GroupOutcome {
    pub label: String,
    pub src_id: Option<String>,
    pub outcome: SearchOutcome,
}

/// Search each spec in turn, reporting progress as it goes.
///
/// Sequential across specs on purpose: `search_all` already fans out across
/// backends inside one spec, and running several specs concurrently would
/// multiply requests per backend and invite a rate limit.
pub fn search_many(
    reg: &Registry,
    specs: &[QuerySpec],
    opts: &SearchOpts,
    mut on_progress: impl FnMut(usize, usize, &str),
) -> Vec<GroupOutcome> {
    let total = specs.len();
    let mut out = Vec::with_capacity(total);
    for (i, spec) in specs.iter().enumerate() {
        on_progress(i, total, &spec.label);
        out.push(GroupOutcome {
            label: spec.label.clone(),
            src_id: spec.src_id.clone(),
            outcome: search_all(reg, &spec.query, opts),
        });
    }
    on_progress(total, total, "");
    out
}

/// Deliberately not a `Result<Vec<Offer>>`.
///
/// `run_cp` aborting a whole batch when one plan fails validation is right for
/// local database work. It is wrong here: Bandcamp being down must not hide the
/// SoundCloud offers.
#[derive(Debug)]
pub struct SearchOutcome {
    pub offers: Vec<RankedOffer>,
    pub per_backend: Vec<BackendReport>,
}

impl SearchOutcome {
    pub fn is_empty(&self) -> bool {
        self.offers.is_empty()
    }

    /// Backends that failed outright, for the `degraded:` block.
    pub fn failures(&self) -> impl Iterator<Item = &BackendReport> {
        self.per_backend.iter().filter(|r| r.error.is_some())
    }

    /// True when every backend we asked failed.
    pub fn total_failure(&self) -> bool {
        !self.per_backend.is_empty() && self.per_backend.iter().all(|r| r.error.is_some())
    }

    pub fn by_ref(&self, item: &ItemRef) -> Option<&RankedOffer> {
        self.offers.iter().find(|r| &r.offer.item_ref == item)
    }

    pub fn by_row(&self, row: usize) -> Option<&RankedOffer> {
        self.offers.iter().find(|r| r.row == row)
    }

    /// Top-ranked offer from one backend, for `--from`.
    pub fn top_from(&self, backend: BackendId) -> Option<&RankedOffer> {
        self.offers.iter().find(|r| r.offer.backend() == backend)
    }
}

/// Search every eligible backend at once.
pub fn search_all(reg: &Registry, query: &SearchQuery, opts: &SearchOpts) -> SearchOutcome {
    let deadline = Instant::now() + opts.timeout;

    let selected: Vec<&dyn AcquisitionBackend> = reg
        .searchable()
        .filter(|b| opts.only.is_empty() || opts.only.contains(&b.id()))
        // A rip is never lossless, so --lossless-only can skip the backend
        // entirely rather than searching and discarding.
        .filter(|b| !opts.lossless_only || b.capabilities().lossless_capable)
        .collect();

    let results: Vec<(BackendReport, Vec<Offer>)> = std::thread::scope(|s| {
        let handles: Vec<_> = selected
            .iter()
            .map(|b| {
                let b = *b;
                s.spawn(move || run_one(b, query, opts, deadline))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                // A panic in one backend's parsing must not take the process
                // down or lose the other backends' results.
                h.join().unwrap_or_else(|_| {
                    (
                        BackendReport {
                            backend: BackendId::Bandcamp,
                            elapsed: Duration::ZERO,
                            raw_hits: 0,
                            enriched: 0,
                            error: Some(BackendError::Other(anyhow::anyhow!(
                                "backend panicked while searching"
                            ))),
                            enrich_errors: 0,
                        },
                        Vec::new(),
                    )
                })
            })
            .collect()
    });

    let mut per_backend = Vec::new();
    let mut offers = Vec::new();
    for (report, found) in results {
        per_backend.push(report);
        offers.extend(found);
    }

    offers.retain(|o| passes_filters(o, opts));
    let mut ranked = rank(offers, query, opts);
    for (i, r) in ranked.iter_mut().enumerate() {
        r.row = i + 1;
    }

    SearchOutcome {
        offers: ranked,
        per_backend,
    }
}

fn run_one(
    b: &dyn AcquisitionBackend,
    query: &SearchQuery,
    opts: &SearchOpts,
    deadline: Instant,
) -> (BackendReport, Vec<Offer>) {
    let started = Instant::now();
    let mut report = BackendReport {
        backend: b.id(),
        elapsed: Duration::ZERO,
        raw_hits: 0,
        enriched: 0,
        error: None,
        enrich_errors: 0,
    };

    let mut offers = match b.search(query) {
        Ok(o) => o,
        Err(e) => {
            report.error = Some(e);
            report.elapsed = started.elapsed();
            return (report, Vec::new());
        }
    };
    report.raw_hits = offers.len();

    // Probe only the top few by textual similarity. Enriching every hit would
    // mean an item-page fetch per result, which is the quickest way to a 429.
    if opts.enrich_top_n > 0 && !offers.is_empty() && Instant::now() < deadline {
        offers.sort_by_key(|o| std::cmp::Reverse(similarity(o, query)));
        let n = opts.enrich_top_n.min(offers.len());
        if let Err(e) = b.enrich(&mut offers[..n]) {
            // A whole-batch enrich failure still leaves the search hits usable.
            report.error = Some(e);
        } else {
            report.enriched = n;
        }
        report.enrich_errors = offers.iter().filter(|o| o.enrich_error.is_some()).count();
    }

    report.elapsed = started.elapsed();
    (report, offers)
}

fn passes_filters(o: &Offer, opts: &SearchOpts) -> bool {
    if opts.lossless_only && o.has_lossless() != Some(true) {
        return false;
    }
    if let Some(want) = &opts.currency {
        // Filtering to one currency is what *makes* prices comparable, so an
        // offer with no known price cannot satisfy it.
        let Some(price) = sortable_price(o) else {
            return false;
        };
        if !price.currency.eq_ignore_ascii_case(want) {
            return false;
        }
    }
    true
}

/// 0..=100 textual similarity between an offer and what was asked for.
pub fn similarity(o: &Offer, q: &SearchQuery) -> u8 {
    let want = normalize_title(&q.title);
    let got = normalize_title(&o.title);
    if want.is_empty() || got.is_empty() {
        return 0;
    }

    let mut score: u32 = if got == want {
        70
    } else if got.contains(&want) || want.contains(&got) {
        45
    } else {
        let overlap = word_overlap(&want, &got);
        (overlap * 40.0) as u32
    };

    if artist_matches(Some(&o.artist), q.artist.as_deref()) {
        score += 20;
    }
    // A duration within a couple of seconds is strong evidence it is the same
    // recording, not a remix that happens to share a title.
    if let (Some(a), Some(b)) = (o.duration_secs, q.duration_secs) {
        if (a - b).abs() <= 2 {
            score += 10;
        } else if (a - b).abs() <= 10 {
            score += 5;
        }
    }
    score.min(100) as u8
}

/// Fraction of `want`'s words that appear in `got`.
fn word_overlap(want: &str, got: &str) -> f64 {
    let got_words: Vec<&str> = got.split_whitespace().collect();
    let want_words: Vec<&str> = want.split_whitespace().collect();
    if want_words.is_empty() {
        return 0.0;
    }
    let hits = want_words.iter().filter(|w| got_words.contains(w)).count();
    hits as f64 / want_words.len() as f64
}

/// Sort key. Pure and total, so the table is deterministic and snapshot-testable.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    /// Quality and cost, in whichever order `SortMode` asked for.
    primary: u8,
    secondary: u8,
    /// Best available format's quality, inverted so better sorts first.
    format_rank: u16,
    /// Grouping only — a stable but non-semantic order across currencies.
    currency: String,
    /// Meaningful only *within* one currency.
    amount: i64,
    /// Match score, descending.
    neg_score: i16,
    backend: u8,
}

/// 0 lossless, 1 unprobed, 2 known-lossy.
///
/// Unprobed sits in the middle deliberately: it might be lossless, so burying it
/// under something known lossy would hide the better option.
fn quality_rank(o: &Offer) -> u8 {
    match o.has_lossless() {
        Some(true) => 0,
        None => 1,
        Some(false) => 2,
    }
}

/// The price used for ordering and display, if any is known.
fn sortable_price(o: &Offer) -> Option<Price> {
    match &o.pricing {
        Pricing::Flat(p) => Some(p.clone()),
        Pricing::NameYourPrice { minimum } => minimum.clone(),
        Pricing::PerFormat(_) => o.effective_price(&[]),
        _ => None,
    }
}

fn rank(offers: Vec<Offer>, query: &SearchQuery, opts: &SearchOpts) -> Vec<RankedOffer> {
    let mut ranked: Vec<RankedOffer> = offers
        .into_iter()
        .map(|offer| {
            let match_score = similarity(&offer, query);
            RankedOffer {
                offer,
                row: 0,
                match_score,
            }
        })
        .collect();

    ranked.sort_by_cached_key(|r| {
        let o = &r.offer;
        let quality = quality_rank(o);
        let cost = o.cost_class() as u8;
        let price = sortable_price(o);

        let (primary, secondary) = match opts.sort {
            SortMode::Quality => (quality, cost),
            SortMode::Cost => (cost, quality),
        };

        RankKey {
            primary,
            secondary,
            format_rank: o
                .formats
                .as_ref()
                .and_then(|fs| fs.iter().map(|f| u16::MAX - f.quality_rank()).min())
                .unwrap_or(u16::MAX),
            currency: price
                .as_ref()
                .map(|p| p.currency.clone())
                .unwrap_or_default(),
            amount: price.as_ref().map(|p| p.amount_minor).unwrap_or(0),
            neg_score: -(r.match_score as i16),
            backend: o.backend().sort_order(),
        }
    });
    ranked
}

/// The cheapest offer in each currency.
///
/// Plural by construction: naming a single winner across differing currencies
/// would require an exchange rate we do not have.
pub fn cheapest_per_currency(offers: &[RankedOffer]) -> Vec<(Price, BackendId)> {
    use std::collections::BTreeMap;
    let mut best: BTreeMap<String, (Price, BackendId)> = BTreeMap::new();
    for r in offers {
        let price = match &r.offer.pricing {
            Pricing::Flat(p) => Some(p.clone()),
            Pricing::NameYourPrice { minimum: Some(p) } => Some(p.clone()),
            _ => None,
        };
        let Some(price) = price.filter(|p| !p.is_zero()) else {
            continue;
        };
        let entry = best.entry(price.currency.clone());
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((price, r.offer.backend()));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if price.amount_minor < o.get().0.amount_minor {
                    o.insert((price, r.offer.backend()));
                }
            }
        }
    }
    best.into_values().collect()
}

/// True when the table holds prices in more than one currency, meaning any
/// "cheapest" claim has to be qualified.
pub fn has_mixed_currencies(offers: &[RankedOffer]) -> bool {
    cheapest_per_currency(offers).len() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(backend: BackendId, title: &str, artist: &str) -> Offer {
        Offer::new(
            ItemRef::new(backend, format!("t:{title}")),
            ItemKind::Track,
            artist,
            title,
            "https://example.test/x",
        )
    }

    fn ranked(offers: Vec<Offer>, sort: SortMode) -> Vec<RankedOffer> {
        let q = SearchQuery::from_text("anything", 10);
        let opts = SearchOpts {
            sort,
            ..Default::default()
        };
        let mut r = rank(offers, &q, &opts);
        for (i, x) in r.iter_mut().enumerate() {
            x.row = i + 1;
        }
        r
    }

    #[test]
    fn lossless_outranks_a_cheaper_lossy_offer() {
        // The point of the feature is a lossless file; a cheaper MP3 is not a
        // cheaper version of the same thing.
        let mut paid_flac = offer(BackendId::Bandcamp, "song", "a");
        paid_flac.formats = Some(vec![AudioFormat::Flac]);
        paid_flac.pricing = Pricing::Flat(Price::new(800, "GBP"));

        let mut free_mp3 = offer(BackendId::SoundCloud, "song", "a");
        free_mp3.formats = Some(vec![AudioFormat::Mp3(Some(128))]);
        free_mp3.pricing = Pricing::Free;

        let r = ranked(vec![free_mp3, paid_flac], SortMode::Quality);
        assert_eq!(r[0].offer.backend(), BackendId::Bandcamp);
    }

    #[test]
    fn cost_sort_puts_the_free_offer_first_instead() {
        let mut paid_flac = offer(BackendId::Bandcamp, "song", "a");
        paid_flac.formats = Some(vec![AudioFormat::Flac]);
        paid_flac.pricing = Pricing::Flat(Price::new(800, "GBP"));

        let mut free_mp3 = offer(BackendId::SoundCloud, "song", "a");
        free_mp3.formats = Some(vec![AudioFormat::Mp3(Some(128))]);
        free_mp3.pricing = Pricing::Free;

        let r = ranked(vec![paid_flac, free_mp3], SortMode::Cost);
        assert_eq!(r[0].offer.backend(), BackendId::SoundCloud);
    }

    #[test]
    fn an_already_owned_offer_outranks_everything_paid() {
        let mut owned = offer(BackendId::Bandcamp, "song", "a");
        owned.formats = Some(vec![AudioFormat::Flac]);
        owned.ownership = Ownership::Yes {
            redownloadable: true,
        };
        owned.pricing = Pricing::Flat(Price::new(900, "GBP"));

        let mut paid = offer(BackendId::Bandcamp, "song2", "a");
        paid.formats = Some(vec![AudioFormat::Flac]);
        paid.pricing = Pricing::Flat(Price::new(100, "GBP"));

        let r = ranked(vec![paid, owned], SortMode::Quality);
        assert_eq!(r[0].offer.cost_class(), CostClass::AlreadyOwned);
    }

    #[test]
    fn unprobed_offers_sort_between_lossless_and_known_lossy() {
        // An unprobed offer might be lossless, so burying it below a known
        // lossy one would hide the better option.
        let mut lossless = offer(BackendId::Bandcamp, "a", "x");
        lossless.formats = Some(vec![AudioFormat::Flac]);
        let unprobed = offer(BackendId::Bandcamp, "b", "x");
        let mut lossy = offer(BackendId::Bandcamp, "c", "x");
        lossy.formats = Some(vec![AudioFormat::Mp3(Some(128))]);

        let r = ranked(vec![lossy, unprobed, lossless], SortMode::Quality);
        assert_eq!(r[0].offer.title, "a");
        assert_eq!(r[1].offer.title, "b");
        assert_eq!(r[2].offer.title, "c");
    }

    #[test]
    fn amounts_are_only_compared_within_one_currency() {
        // 5 GBP vs 6 USD must not be ordered by amount — there is no rate here.
        // Currency groups them; within GBP the cheaper one wins.
        let mut gbp_hi = offer(BackendId::Bandcamp, "gbp-hi", "x");
        gbp_hi.formats = Some(vec![AudioFormat::Flac]);
        gbp_hi.pricing = Pricing::Flat(Price::new(900, "GBP"));
        let mut gbp_lo = offer(BackendId::Bandcamp, "gbp-lo", "x");
        gbp_lo.formats = Some(vec![AudioFormat::Flac]);
        gbp_lo.pricing = Pricing::Flat(Price::new(500, "GBP"));

        let r = ranked(vec![gbp_hi, gbp_lo], SortMode::Quality);
        assert_eq!(r[0].offer.title, "gbp-lo");
    }

    #[test]
    fn cheapest_is_reported_per_currency_never_as_one_winner() {
        let mut gbp = offer(BackendId::Bandcamp, "g", "x");
        gbp.pricing = Pricing::Flat(Price::new(400, "GBP"));
        let mut usd = offer(BackendId::Bandcamp, "u", "x");
        usd.pricing = Pricing::Flat(Price::new(129, "USD"));

        let r = ranked(vec![gbp, usd], SortMode::Quality);
        let cheap = cheapest_per_currency(&r);
        assert_eq!(cheap.len(), 2, "one entry per currency, not one overall");
        assert!(has_mixed_currencies(&r));
        let codes: Vec<&str> = cheap.iter().map(|(p, _)| p.currency.as_str()).collect();
        assert_eq!(codes, vec!["GBP", "USD"]);
    }

    #[test]
    fn free_and_unpriced_offers_are_not_reported_as_cheapest() {
        let mut free = offer(BackendId::SoundCloud, "f", "x");
        free.pricing = Pricing::Free;
        let unprobed = offer(BackendId::Bandcamp, "u", "x");
        let r = ranked(vec![free, unprobed], SortMode::Quality);
        assert!(cheapest_per_currency(&r).is_empty());
        assert!(!has_mixed_currencies(&r));
    }

    #[test]
    fn a_single_currency_table_is_not_flagged_as_mixed() {
        let mut a = offer(BackendId::Bandcamp, "a", "x");
        a.pricing = Pricing::Flat(Price::new(400, "GBP"));
        let mut b = offer(BackendId::Bandcamp, "b", "x");
        b.pricing = Pricing::Flat(Price::new(700, "GBP"));
        let r = ranked(vec![a, b], SortMode::Quality);
        assert!(!has_mixed_currencies(&r));
        assert_eq!(cheapest_per_currency(&r)[0].0.to_string(), "4.00 GBP");
    }

    #[test]
    fn rows_are_numbered_from_one_in_display_order() {
        let r = ranked(
            vec![
                offer(BackendId::Bandcamp, "a", "x"),
                offer(BackendId::Bandcamp, "b", "x"),
            ],
            SortMode::Quality,
        );
        assert_eq!(r[0].row, 1);
        assert_eq!(r[1].row, 2);
    }

    #[test]
    fn ties_break_on_backend_so_output_is_deterministic() {
        let a = offer(BackendId::SoundCloud, "same", "x");
        let b = offer(BackendId::Bandcamp, "same", "x");
        let r = ranked(vec![a, b], SortMode::Quality);
        assert_eq!(r[0].offer.backend(), BackendId::Bandcamp);
    }

    #[test]
    fn similarity_rewards_an_exact_title_then_artist_then_duration() {
        let q = SearchQuery {
            title: "Roygbiv".into(),
            artist: Some("Boards of Canada".into()),
            duration_secs: Some(150),
            limit: 5,
            ..Default::default()
        };
        let mut exact = offer(BackendId::Bandcamp, "Roygbiv", "Boards of Canada");
        exact.duration_secs = Some(150);
        let partial = offer(BackendId::Bandcamp, "Roygbiv (Remix)", "Someone Else");
        let unrelated = offer(BackendId::Bandcamp, "Completely Different", "Nobody");

        let se = similarity(&exact, &q);
        let sp = similarity(&partial, &q);
        let su = similarity(&unrelated, &q);
        assert!(se > sp, "exact {se} should beat partial {sp}");
        assert!(sp > su, "partial {sp} should beat unrelated {su}");
        assert_eq!(se, 100, "title + artist + duration is a full match");
    }

    #[test]
    fn similarity_ignores_bracketed_text_the_way_the_existing_matcher_does() {
        // normalize_title strips parenthesised text, so these normalize equal.
        let q = SearchQuery::from_text("Roygbiv", 5);
        assert!(similarity(&offer(BackendId::Bandcamp, "Roygbiv (Edit)", "a"), &q) >= 70);
    }

    #[test]
    fn similarity_is_zero_for_empty_input() {
        let q = SearchQuery::from_text("", 5);
        assert_eq!(similarity(&offer(BackendId::Bandcamp, "x", "y"), &q), 0);
        let q2 = SearchQuery::from_text("x", 5);
        assert_eq!(similarity(&offer(BackendId::Bandcamp, "", ""), &q2), 0);
    }

    #[test]
    fn lossless_only_drops_unprobed_and_lossy_offers() {
        let opts = SearchOpts {
            lossless_only: true,
            ..Default::default()
        };
        let mut flac = offer(BackendId::Bandcamp, "a", "x");
        flac.formats = Some(vec![AudioFormat::Flac]);
        let mut mp3 = offer(BackendId::Bandcamp, "b", "x");
        mp3.formats = Some(vec![AudioFormat::Mp3(Some(320))]);
        let unprobed = offer(BackendId::Bandcamp, "c", "x");

        assert!(passes_filters(&flac, &opts));
        assert!(!passes_filters(&mp3, &opts));
        assert!(
            !passes_filters(&unprobed, &opts),
            "unknown is not proof of lossless"
        );
    }

    #[test]
    fn currency_filter_keeps_only_matching_prices() {
        let opts = SearchOpts {
            currency: Some("gbp".into()),
            ..Default::default()
        };
        let mut gbp = offer(BackendId::Bandcamp, "a", "x");
        gbp.pricing = Pricing::Flat(Price::new(400, "GBP"));
        let mut usd = offer(BackendId::Bandcamp, "b", "x");
        usd.pricing = Pricing::Flat(Price::new(400, "USD"));
        let unpriced = offer(BackendId::Bandcamp, "c", "x");

        assert!(passes_filters(&gbp, &opts), "case-insensitive match");
        assert!(!passes_filters(&usd, &opts));
        assert!(
            !passes_filters(&unpriced, &opts),
            "an unknown price cannot be confirmed to be in this currency"
        );
    }

    #[test]
    fn outcome_lookups_work_by_ref_row_and_backend() {
        let r = ranked(
            vec![
                offer(BackendId::Bandcamp, "a", "x"),
                offer(BackendId::SoundCloud, "b", "x"),
            ],
            SortMode::Quality,
        );
        let out = SearchOutcome {
            offers: r,
            per_backend: Vec::new(),
        };
        assert_eq!(out.by_row(1).unwrap().row, 1);
        assert!(out.by_row(99).is_none());
        assert_eq!(
            out.top_from(BackendId::SoundCloud).unwrap().offer.backend(),
            BackendId::SoundCloud
        );
        let item = out.offers[0].offer.item_ref.clone();
        assert!(out.by_ref(&item).is_some());
    }

    #[test]
    fn total_failure_needs_every_backend_to_have_failed() {
        let failed = |id| BackendReport {
            backend: id,
            elapsed: Duration::ZERO,
            raw_hits: 0,
            enriched: 0,
            error: Some(BackendError::unsupported(id, "search")),
            enrich_errors: 0,
        };
        let ok = |id| BackendReport {
            backend: id,
            elapsed: Duration::ZERO,
            raw_hits: 1,
            enriched: 0,
            error: None,
            enrich_errors: 0,
        };

        let all_bad = SearchOutcome {
            offers: vec![],
            per_backend: vec![failed(BackendId::Bandcamp), failed(BackendId::SoundCloud)],
        };
        assert!(all_bad.total_failure());
        assert_eq!(all_bad.failures().count(), 2);

        // One backend down must degrade, not abort.
        let partial = SearchOutcome {
            offers: vec![],
            per_backend: vec![failed(BackendId::Bandcamp), ok(BackendId::SoundCloud)],
        };
        assert!(!partial.total_failure());
        assert_eq!(partial.failures().count(), 1);

        // No backends at all is not a "total failure" either.
        let none = SearchOutcome {
            offers: vec![],
            per_backend: vec![],
        };
        assert!(!none.total_failure());
    }
}
