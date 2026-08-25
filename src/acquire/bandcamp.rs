//! The Bandcamp backend.
//!
//! Bandcamp has no usable public API for this: the official one is for labels and
//! fulfilment partners (sales and merch reports) and exposes neither catalogue
//! search nor downloads. So search uses the same undocumented endpoint their own
//! site calls, and pricing comes from the JSON blob their pages embed.
//!
//! Two things about their responses drive the code below:
//!
//! * **Failures arrive as HTTP 200** with `{"error":true,"error_message":...}`.
//!   Checking the status code alone reads a failure as success, so every response
//!   is inspected for that field first.
//! * **`data-cart.currency` is the *viewer's* currency, not the seller's.** On a
//!   GBP album it reads `USD` from a US IP. Using it would label a £8.50 price as
//!   dollars. The seller's currency comes from `packages[].currency`, falling back
//!   to the ISO code rendered next to the price — and if neither is present we
//!   report the currency as unknown rather than guessing.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use super::blob;
use super::error::{BackendError, Result};
use super::http;
use super::types::*;
use crate::config::{CredentialSource, Credentials, Secret};

const SEARCH_URL: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const COLLECTION_SUMMARY_URL: &str = "https://bandcamp.com/api/fan/2/collection_summary";
const COLLECTION_ITEMS_URL: &str = "https://bandcamp.com/api/fancollection/1/collection_items";

/// `current.download_pref` constants, named in the page blob as `FREE`/`PAID`.
const DOWNLOAD_PREF_FREE: i64 = 1;
const DOWNLOAD_PREF_PAID: i64 = 2;

/// The format menu Bandcamp offers for *any* digital download.
///
/// Not read from the item page: the format list only exists on the post-purchase
/// download page, so it cannot be known before buying. This is Bandcamp's
/// standard menu, used to populate the comparison table. The download page stays
/// authoritative at fetch time, and a fetch that finds fewer formats reports
/// `NoAcceptableFormat` naming what was actually there.
const DIGITAL_FORMATS: &[AudioFormat] = &[
    AudioFormat::Flac,
    AudioFormat::Aiff,
    AudioFormat::Wav,
    AudioFormat::Alac,
    AudioFormat::Mp3(Some(320)),
    AudioFormat::Mp3V0,
    AudioFormat::Aac(Some(256)),
    AudioFormat::Ogg,
];

pub struct Bandcamp {
    identity: Option<(Secret, CredentialSource)>,
    budget: Duration,
    /// Resolved once per process: the collection summary is a single request that
    /// answers ownership for everything, so it must not be refetched per offer.
    owned: OnceLock<OwnedSet>,
}

/// What the user owns.
///
/// Keyed by Bandcamp's own `"t<id>"` / `"a<id>"` lookup strings.
#[derive(Debug, Default)]
struct OwnedSet {
    /// `None` when we could not look at all, so "not owned" stays distinct from
    /// "unknown" — reporting an unchecked item as unowned would push the user
    /// towards buying something twice.
    keys: Option<std::collections::HashSet<String>>,
}

impl OwnedSet {
    fn contains(&self, kind: ItemKind, id: i64) -> Ownership {
        let Some(keys) = &self.keys else {
            return Ownership::Unknown;
        };
        let prefix = if kind == ItemKind::Track { 't' } else { 'a' };
        if keys.contains(&format!("{prefix}{id}")) {
            Ownership::Yes {
                redownloadable: true,
            }
        } else {
            Ownership::No
        }
    }
}

/// Parse `/api/fan/2/collection_summary`.
///
/// Returns `None` when the response says we are not logged in, so the caller can
/// report an expired session rather than an empty collection.
fn parse_collection_summary(body: &str) -> Result<std::collections::HashSet<String>> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        BackendError::parse(BackendId::Bandcamp, "collection_summary", e.to_string())
    })?;
    if v["error"].as_bool().unwrap_or(false) {
        let msg = v["error_message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        return Err(BackendError::AuthExpired {
            backend: BackendId::Bandcamp,
            detail: msg,
            reauth: Some("https://bandcamp.com/login".into()),
        });
    }
    let lookup = &v["collection_summary"]["tralbum_lookup"];
    let Some(map) = lookup.as_object() else {
        // A logged-in fan with an empty collection is legitimate.
        return Ok(std::collections::HashSet::new());
    };
    Ok(map.keys().cloned().collect())
}

/// One page of `/api/fancollection/1/collection_items`.
#[derive(Debug, Default)]
pub struct CollectionPage {
    /// `(kind, tralbum_id, sale_item_key)` per item.
    items: Vec<(ItemKind, i64, String)>,
    /// `sale_item_key` → redownload URL.
    redownload_urls: std::collections::HashMap<String, String>,
    pub more_available: bool,
    pub last_token: Option<String>,
}

impl CollectionPage {
    /// The redownload URL for a tralbum, joining `items` to `redownload_urls`.
    pub fn redownload_for(&self, kind: ItemKind, id: i64) -> Option<String> {
        self.items
            .iter()
            .find(|(k, i, _)| *k == kind && *i == id)
            .and_then(|(_, _, sale_key)| self.redownload_urls.get(sale_key))
            .cloned()
    }
}

/// Parse the collection-items response. Shape confirmed against the live API.
pub fn parse_collection_items(body: &str) -> Result<CollectionPage> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| BackendError::parse(BackendId::Bandcamp, "collection_items", e.to_string()))?;
    if v["error"].as_bool().unwrap_or(false) {
        return Err(BackendError::AuthExpired {
            backend: BackendId::Bandcamp,
            detail: v["error_message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string(),
            reauth: Some("https://bandcamp.com/login".into()),
        });
    }

    let mut page = CollectionPage {
        more_available: v["more_available"].as_bool().unwrap_or(false),
        last_token: v["last_token"].as_str().map(str::to_string),
        ..Default::default()
    };

    for item in v["items"].as_array().unwrap_or(&Vec::new()) {
        let kind = match item["tralbum_type"].as_str() {
            Some("t") => ItemKind::Track,
            Some("a") => ItemKind::Album,
            _ => continue,
        };
        let Some(id) = item["tralbum_id"].as_i64() else {
            continue;
        };
        // Bandcamp keys redownload_urls by sale-item, not tralbum, so the join
        // key is built from the sale item's own type and id.
        let Some(sale_id) = item["sale_item_id"].as_i64() else {
            continue;
        };
        let sale_type = item["sale_item_type"].as_str().unwrap_or("p");
        page.items.push((kind, id, format!("{sale_type}{sale_id}")));
    }

    if let Some(map) = v["redownload_urls"].as_object() {
        for (k, val) in map {
            if let Some(url) = val.as_str() {
                page.redownload_urls.insert(k.clone(), url.to_string());
            }
        }
    }
    Ok(page)
}

/// `fan_id` from a collection summary.
fn parse_fan_id(body: &str) -> Result<i64> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        BackendError::parse(BackendId::Bandcamp, "collection_summary", e.to_string())
    })?;
    if v["error"].as_bool().unwrap_or(false) {
        return Err(BackendError::AuthExpired {
            backend: BackendId::Bandcamp,
            detail: v["error_message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string(),
            reauth: Some("https://bandcamp.com/login".into()),
        });
    }
    v["fan_id"]
        .as_i64()
        .or_else(|| v["collection_summary"]["fan_id"].as_i64())
        .ok_or_else(|| BackendError::parse(BackendId::Bandcamp, "collection_summary", "no fan_id"))
}

/// Per-format download links from a `redownload_url` page.
///
/// Chain: the page's `<div id="pagedata" data-blob="…">` →
/// `download_items[0].downloads[<slug>]` → `{ url, size_mb }`.
pub fn parse_download_page(html: &str) -> Result<std::collections::HashMap<AudioFormat, String>> {
    let blob = blob::id_attr_json(html, "pagedata", "data-blob")
        .or_else(|_| blob::attr_json(html, "data-blob"))
        .map_err(|e| {
            BackendError::parse(BackendId::Bandcamp, "download pagedata", e.to_string())
        })?;

    let items = blob["download_items"].as_array().ok_or_else(|| {
        BackendError::parse(
            BackendId::Bandcamp,
            "download pagedata",
            "no download_items",
        )
    })?;
    let first = items.first().ok_or_else(|| {
        BackendError::parse(
            BackendId::Bandcamp,
            "download pagedata",
            "download_items was empty",
        )
    })?;
    let downloads = first["downloads"].as_object().ok_or_else(|| {
        BackendError::parse(BackendId::Bandcamp, "download pagedata", "no downloads map")
    })?;

    let mut out = std::collections::HashMap::new();
    for (slug, entry) in downloads {
        // An unrecognised slug is skipped rather than failing the whole page —
        // bandcamp adding a format must not break downloading the others.
        let Ok(format) = slug.parse::<AudioFormat>() else {
            continue;
        };
        if let Some(url) = entry["url"].as_str() {
            out.insert(format, url.to_string());
        }
    }
    if out.is_empty() {
        return Err(BackendError::parse(
            BackendId::Bandcamp,
            "download pagedata",
            "no usable download links",
        ));
    }
    Ok(out)
}

/// Longest we will wait for Bandcamp to prepare a download.
///
/// Measured: a file nobody has downloaded before returns an HTML interstitial for
/// **longer than two minutes**, then serves the audio instantly once ready. Two
/// minutes was not enough and produced a spurious timeout, so this is generous.
const READY_TIMEOUT: Duration = Duration::from_secs(600);

/// Longest gap between re-asks.
const READY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Fetch the audio, waiting out Bandcamp's "preparing your download" page.
///
/// Bandcamp prepares a purchased file **asynchronously**. The first request for a
/// download URL usually returns an HTML interstitial (the download page again,
/// with `ready: false`) rather than audio; a later request returns the file. So
/// this cannot be a single GET.
///
/// Two rules, both learned by writing a 229KB HTML page out as a `.flac`:
///
/// * A response is only written if it is actually audio. Content-Type is the
///   primary signal, and the leading bytes are checked as a backstop — an HTML
///   page saved with an audio extension is the worst possible outcome here,
///   because rekordbox will import it and analyse it into nonsense.
/// * The interstitial carries a fresh download URL, so each retry re-reads it
///   instead of hammering a URL that may have expired.
fn download_when_ready(
    agent: &ureq::Agent,
    first_url: &str,
    cookie: &str,
    format: AudioFormat,
    target: &std::path::Path,
) -> Result<u64> {
    let mut url = first_url.to_string();
    let mut delay = Duration::from_secs(2);
    let started = std::time::Instant::now();
    let mut attempt = 0u32;

    while started.elapsed() < READY_TIMEOUT {
        attempt += 1;
        let mut resp = agent
            .get(&url)
            .header("Cookie", cookie)
            .call()
            .map_err(|e| http::map_err(BackendId::Bandcamp, &url, e))?;

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if !content_type.starts_with("text/html") {
            let mut reader = resp.body_mut().as_reader();
            return super::fs::write_audio_atomically(target, &mut reader);
        }

        // An interstitial. Re-read it for a fresh link and wait.
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| http::map_err(BackendId::Bandcamp, &url, e))?;
        if attempt == 1 {
            eprintln!(
                "  bandcamp is preparing this download — a file nobody has fetched before can \
                 take several minutes. Waiting up to {}s.",
                READY_TIMEOUT.as_secs()
            );
        } else {
            eprintln!(
                "  still preparing ({}s elapsed, attempt {attempt})",
                started.elapsed().as_secs()
            );
        }
        match parse_download_page(&body) {
            Ok(links) => match links.get(&format) {
                Some(fresh) if *fresh != url => url = fresh.clone(),
                Some(_) => {}
                None => {
                    return Err(BackendError::parse(
                        BackendId::Bandcamp,
                        "download retry page",
                        format!("{format} is no longer offered"),
                    ));
                }
            },
            Err(e) => eprintln!("  (could not re-read the download page: {e})"),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(READY_MAX_BACKOFF);
    }

    Err(BackendError::Timeout {
        backend: BackendId::Bandcamp,
        op: "waiting for bandcamp to prepare the download — try again in a few minutes",
        elapsed: started.elapsed(),
    })
}

/// Artist and title from a download page, for naming the file.
fn item_artist(html: &str) -> Option<String> {
    blob::id_attr_json(html, "pagedata", "data-blob")
        .ok()?
        .pointer("/download_items/0/artist")?
        .as_str()
        .map(str::to_string)
}

fn item_title(html: &str) -> Option<String> {
    blob::id_attr_json(html, "pagedata", "data-blob")
        .ok()?
        .pointer("/download_items/0/title")?
        .as_str()
        .map(str::to_string)
}

/// Item keys are `<t|a>:<id>:<url>`.
///
/// The URL is carried in the key rather than looked up because Bandcamp gives no
/// way to turn a numeric id back into a page URL — so without it a saved ref
/// could not be opened in a browser later. The id stays the identity; the URL is
/// just along for the ride, and Bandcamp slug changes only affect the latter.
fn item_key(kind: ItemKind, id: i64, url: &str) -> String {
    let prefix = if kind == ItemKind::Track { "t" } else { "a" };
    format!("{prefix}:{id}:{url}")
}

/// Split an item key into its kind and numeric id.
fn parse_item_key(key: &str) -> Option<(ItemKind, i64)> {
    let mut parts = key.splitn(3, ':');
    let kind = match parts.next()? {
        "t" => ItemKind::Track,
        "a" => ItemKind::Album,
        _ => return None,
    };
    Some((kind, parts.next()?.parse().ok()?))
}

impl Bandcamp {
    pub fn new(creds: &Credentials, budget: Duration) -> Self {
        Self {
            identity: creds.bandcamp_identity().unwrap_or(None),
            budget,
            owned: OnceLock::new(),
        }
    }

    fn err_parse(&self, at: &'static str, detail: impl Into<String>) -> BackendError {
        BackendError::parse(BackendId::Bandcamp, at, detail)
    }

    /// The collection, fetched at most once per process.
    ///
    /// One request answers ownership for everything, which is exactly why
    /// `enrich` takes a slice: doing this per offer would multiply it by N.
    fn owned(&self) -> &OwnedSet {
        self.owned.get_or_init(|| {
            let Some((secret, _)) = &self.identity else {
                return OwnedSet::default();
            };
            let agent = http::agent(self.budget);
            let result = agent
                .get(COLLECTION_SUMMARY_URL)
                .header("Cookie", &format!("identity={}", secret.expose()))
                .call()
                .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_SUMMARY_URL, e))
                .and_then(|mut r| {
                    r.body_mut()
                        .read_to_string()
                        .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_SUMMARY_URL, e))
                })
                .and_then(|body| parse_collection_summary(&body));

            match result {
                Ok(keys) => OwnedSet { keys: Some(keys) },
                Err(e) => {
                    // Ownership stays Unknown rather than No: telling the user
                    // they don't own something we failed to check could have
                    // them buy it twice.
                    eprintln!("warning: could not read your bandcamp collection: {e}");
                    OwnedSet::default()
                }
            }
        })
    }

    /// The `redownload_url` for an owned item, or `None` if it is not owned.
    ///
    /// Walks the collection in pages: the API returns `redownload_urls` keyed by
    /// `"<p|t|a><sale_item_id>"`, alongside `items` that carry the tralbum id, so
    /// the two have to be joined.
    fn redownload_url(&self, secret: &Secret, kind: ItemKind, id: i64) -> Result<Option<String>> {
        let agent = http::agent(self.budget);
        let cookie = format!("identity={}", secret.expose());

        let fan_id = {
            let body = agent
                .get(COLLECTION_SUMMARY_URL)
                .header("Cookie", &cookie)
                .call()
                .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_SUMMARY_URL, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_SUMMARY_URL, e))?;
            parse_fan_id(&body)?
        };

        // A newer-than token pages backwards through the collection; this start
        // value is far enough in the future to begin at the most recent item.
        let mut token = String::from("9999999999::a::");
        for _ in 0..40 {
            let body = http::with_retries(3, || {
                agent
                    .post(COLLECTION_ITEMS_URL)
                    .header("Cookie", &cookie)
                    .send_json(serde_json::json!({
                        "fan_id": fan_id,
                        "older_than_token": token,
                        "count": 100,
                    }))
                    .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_ITEMS_URL, e))?
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| http::map_err(BackendId::Bandcamp, COLLECTION_ITEMS_URL, e))
            })?;

            let page = parse_collection_items(&body)?;
            if let Some(url) = page.redownload_for(kind, id) {
                return Ok(Some(url));
            }
            match (page.more_available, page.last_token) {
                (true, Some(next)) if next != token => token = next,
                _ => break,
            }
        }
        Ok(None)
    }

    /// Read one item page and fold its facts into the offer.
    fn enrich_one(&self, offer: &mut Offer) -> Result<()> {
        let agent = http::agent(self.budget);
        let url = offer.url.clone();
        let html = http::with_retries(2, || {
            agent
                .get(&url)
                .call()
                .map_err(|e| http::map_err(BackendId::Bandcamp, &url, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(BackendId::Bandcamp, &url, e))
        })?;

        let facts = parse_item_page(&html)?;
        let acquirable = !matches!(facts.pricing, Pricing::Unavailable { .. });
        offer.pricing = facts.pricing;
        if offer.duration_secs.is_none() {
            offer.duration_secs = facts.duration_secs;
        }
        // Bandcamp's format menu only exists on the post-purchase download page,
        // so this is their standard digital set rather than something read off
        // this page. The download page stays authoritative at fetch time.
        offer.formats = Some(if acquirable {
            DIGITAL_FORMATS.to_vec()
        } else {
            Vec::new()
        });
        Ok(())
    }
}

/// The search endpoint's response.
#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    auto: Option<AutoBlock>,
    #[serde(default)]
    error: bool,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AutoBlock {
    #[serde(default)]
    results: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    /// `t` = track, `a` = album, `b` = band, `f` = fan.
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<i64>,
    name: Option<String>,
    band_name: Option<String>,
    album_name: Option<String>,
    item_url_path: Option<String>,
    img: Option<String>,
}

/// Turn a search response into offers, dropping hits that aren't buyable items.
///
/// Split out from the request so it can be tested against a captured response.
fn parse_search(body: &str, limit: usize) -> Result<Vec<Offer>> {
    let resp: SearchResponse = serde_json::from_str(body)
        .map_err(|e| BackendError::parse(BackendId::Bandcamp, "search response", e.to_string()))?;

    // Bandcamp answers 200 on failure, so the body is the only signal.
    if resp.error {
        let msg = resp.error_message.unwrap_or_else(|| "unknown error".into());
        return Err(if msg.contains("logged in") {
            BackendError::AuthExpired {
                backend: BackendId::Bandcamp,
                detail: msg,
                reauth: Some("https://bandcamp.com/login".into()),
            }
        } else {
            BackendError::parse(BackendId::Bandcamp, "search response", msg)
        });
    }

    let hits = resp.auto.map(|a| a.results).unwrap_or_default();
    let mut offers = Vec::new();
    for h in hits {
        if offers.len() >= limit {
            break;
        }
        // Bands and fans are not acquirable, so they are not offers.
        let kind = match h.kind.as_deref() {
            Some("t") => ItemKind::Track,
            Some("a") => ItemKind::Album,
            _ => continue,
        };
        let (Some(id), Some(name), Some(url)) = (h.id, h.name, h.item_url_path) else {
            continue;
        };
        let mut offer = Offer::new(
            ItemRef::new(BackendId::Bandcamp, item_key(kind, id, &url)),
            kind,
            h.band_name.unwrap_or_default(),
            name,
            url,
        );
        offer.album = h.album_name;
        offer.artwork_url = h.img;
        offers.push(offer);
    }
    Ok(offers)
}

/// Pricing and duration read off an item page.
#[derive(Debug, PartialEq)]
pub struct ItemFacts {
    pub pricing: Pricing,
    pub duration_secs: Option<i64>,
    /// True when the page advertises a free download page.
    pub free_download: bool,
}

/// Parse an item page's `data-tralbum` blob.
///
/// The numeric price comes from the blob (authoritative); the currency comes from
/// `packages[].currency`, then the rendered ISO code, then nowhere — in which
/// case it stays unknown and the offer table shows it as such.
pub fn parse_item_page(html: &str) -> Result<ItemFacts> {
    let t = blob::attr_json(html, "data-tralbum")
        .map_err(|e| BackendError::parse(BackendId::Bandcamp, "data-tralbum", e.to_string()))?;

    let current = &t["current"];
    let download_pref = current["download_pref"].as_i64();
    let minimum = current["minimum_price"].as_f64();
    let set_price = current["set_price"].as_f64();
    // A JSON null here means "not a set price", i.e. name-your-price.
    let is_set_price = current["is_set_price"].as_bool().unwrap_or(false);
    let free_download = !t["freeDownloadPage"].is_null();

    let currency = seller_currency(&t, html);

    let pricing = match download_pref {
        Some(DOWNLOAD_PREF_FREE) => Pricing::Free,
        Some(DOWNLOAD_PREF_PAID) => {
            if is_set_price {
                match (set_price, &currency) {
                    (Some(p), Some(c)) => Pricing::Flat(Price::from_major(p, c)),
                    // A price we cannot label is still a price; carry it with an
                    // explicit unknown currency rather than inventing one.
                    (Some(p), None) => Pricing::Flat(Price::from_major(p, UNKNOWN_CURRENCY)),
                    (None, _) => Pricing::Unprobed,
                }
            } else {
                let minimum = minimum
                    .filter(|m| *m > 0.0)
                    .map(|m| Price::from_major(m, currency.as_deref().unwrap_or(UNKNOWN_CURRENCY)));
                Pricing::NameYourPrice { minimum }
            }
        }
        // No download preference at all means it is not offered as a download —
        // streaming-only, or a physical-media-only release.
        _ => Pricing::Unavailable {
            reason: Some("no digital download offered".into()),
        },
    };

    Ok(ItemFacts {
        pricing,
        duration_secs: total_duration_secs(&t),
        free_download,
    })
}

/// Marker for a price whose currency we could not establish. Rendered as-is, so
/// it is obvious in the table and can never be compared against a real code.
pub const UNKNOWN_CURRENCY: &str = "???";

/// The *seller's* currency.
///
/// Deliberately never `data-cart.currency`, which is the viewer's cart currency
/// and is wrong for any seller who doesn't price in it.
fn seller_currency(tralbum: &serde_json::Value, html: &str) -> Option<String> {
    if let Some(packages) = tralbum["packages"].as_array()
        && let Some(c) = packages
            .iter()
            .filter_map(|p| p["currency"].as_str())
            .find(|c| !c.trim().is_empty())
    {
        return Some(c.trim().to_ascii_uppercase());
    }
    // Fall back to the ISO code rendered beside the price.
    rendered_currency(html)
}

/// The ISO code Bandcamp prints next to a price, e.g.
/// `<span class="buyItemExtra secondaryText">GBP</span>`.
fn rendered_currency(html: &str) -> Option<String> {
    let marker = "buyItemExtra secondaryText\">";
    let at = html.find(marker)? + marker.len();
    let rest = html[at..].trim_start();
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    (code.len() == 3).then(|| code.to_ascii_uppercase())
}

/// Total playing time from `trackinfo`, rounded to whole seconds.
fn total_duration_secs(tralbum: &serde_json::Value) -> Option<i64> {
    let tracks = tralbum["trackinfo"].as_array()?;
    let total: f64 = tracks.iter().filter_map(|t| t["duration"].as_f64()).sum();
    (total > 0.0).then(|| total.round() as i64)
}

impl super::AcquisitionBackend for Bandcamp {
    fn id(&self) -> BackendId {
        BackendId::Bandcamp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            search: true,
            price_quotes: true,
            // Needs the identity cookie; without it ownership stays Unknown.
            ownership_check: self.identity.is_some(),
            requires_purchase: true,
            fetch: self.identity.is_some(),
            lossless_capable: true,
        }
    }

    fn credentials(&self) -> CredentialState {
        match &self.identity {
            Some((secret, source)) => CredentialState::Present {
                hint: format!("identity cookie, {} bytes, from {source}", secret.len()),
            },
            None => CredentialState::Missing {
                how_to_fix: "put your bandcamp `identity` cookie in credentials.toml, \
                             or set BANDCAMP_IDENTITY"
                    .into(),
            },
        }
    }

    fn claim_url(&self, url: &str) -> Option<ItemRef> {
        // Bandcamp URLs carry a mutable slug, not a stable id, so a claimed URL
        // keeps the URL as its key and the id is resolved when the page is read.
        if !url.contains(".bandcamp.com/") {
            return None;
        }
        let kind = if url.contains("/track/") {
            "url-t"
        } else if url.contains("/album/") {
            "url-a"
        } else {
            return None;
        };
        Some(ItemRef::new(BackendId::Bandcamp, format!("{kind}:{url}")))
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<Offer>> {
        let text = query.search_text();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let agent = http::agent(self.budget);
        let body = http::with_retries(3, || {
            agent
                .post(SEARCH_URL)
                .send_json(serde_json::json!({
                    "search_text": text,
                    "search_filter": "",
                    "full_page": false,
                }))
                .map_err(|e| http::map_err(BackendId::Bandcamp, SEARCH_URL, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(BackendId::Bandcamp, SEARCH_URL, e))
        })?;
        parse_search(&body, query.limit)
    }

    fn enrich(&self, offers: &mut [Offer]) -> Result<()> {
        // Ownership first: one request covers every offer, so it is nearly free
        // and it can make a paid offer turn out to cost nothing.
        let owned = self.owned();
        for offer in offers.iter_mut() {
            if let Some((kind, id)) = parse_item_key(&offer.item_ref.key) {
                offer.ownership = owned.contains(kind, id);
            }
        }

        // Pricing needs one page per offer, so it is the part the caller budgets.
        for offer in offers.iter_mut() {
            if let Err(e) = self.enrich_one(offer) {
                // An expired session is a whole-batch problem: every remaining
                // request would fail the same way, so stop rather than hammering.
                if matches!(e, BackendError::AuthExpired { .. }) {
                    return Err(e);
                }
                offer.enrich_error = Some(e.to_string());
            }
        }
        Ok(())
    }

    fn fetch(&self, item: &ItemRef, opts: &FetchOpts) -> Result<Vec<AcquiredFile>> {
        let Some((secret, _)) = &self.identity else {
            return Err(BackendError::NoCredentials {
                backend: BackendId::Bandcamp,
                how_to_fix: "put your bandcamp `identity` cookie in credentials.toml".into(),
            });
        };
        let (kind, id) = parse_item_key(&item.key)
            .ok_or_else(|| self.err_parse("item ref", "not a bandcamp item key"))?;

        // Only things you own can be downloaded, so say which it is rather than
        // reporting a generic failure.
        let redownload =
            self.redownload_url(secret, kind, id)?
                .ok_or_else(|| BackendError::NotOwned {
                    backend: BackendId::Bandcamp,
                    item: item.clone(),
                })?;

        let cookie = format!("identity={}", secret.expose());
        let agent = http::download_agent(
            opts.deadline
                .saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_secs(30)),
        );

        // The download page carries the per-format links, and is the
        // authoritative format list — the search-time list is only bandcamp's
        // standard menu.
        let page = http::with_retries(4, || {
            agent
                .get(&redownload)
                .header("Cookie", &cookie)
                .call()
                .map_err(|e| http::map_err(BackendId::Bandcamp, &redownload, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(BackendId::Bandcamp, &redownload, e))
        })?;

        let available = parse_download_page(&page)?;
        let chosen = opts
            .format_pref
            .iter()
            .find(|f| available.contains_key(f))
            .copied()
            .ok_or_else(|| BackendError::NoAcceptableFormat {
                available: available.keys().copied().collect(),
                wanted: opts.format_pref.clone(),
            })?;
        let url = available[&chosen].clone();

        let name = super::fs::track_filename(
            Some(&item_artist(&page).unwrap_or_default()),
            item_title(&page).as_deref(),
            chosen.extension(),
        );
        let target = super::fs::unique_path(&opts.dest_dir.join(name));

        let bytes = download_when_ready(&agent, &url, &cookie, chosen, &target)?;

        Ok(vec![AcquiredFile {
            path: target,
            format: chosen,
            bytes,
            retention: opts.retention,
            source: item.clone(),
            source_url: url,
            artist: item_artist(&page),
            title: item_title(&page),
            album: None,
            track_number: None,
        }])
    }

    fn purchase(&self, item: &ItemRef) -> Result<PurchaseFlow> {
        // The buy page is the item page — Bandcamp's checkout lives there. There
        // is no automated purchase path and this tool will not pretend otherwise.
        Ok(PurchaseFlow::OpenInBrowser {
            url: item_url(item).ok_or_else(|| self.err_parse("item ref", "no url in ref"))?,
            note: Some("pay in the browser, then run `rekord-ripper fetch`".into()),
        })
    }
}

/// Recover a browsable URL from an item ref.
///
/// Handles both key shapes: `<t|a>:<id>:<url>` from a search, and `url-<t|a>:<url>`
/// from a URL the user pasted (which has no id until the page is read).
pub fn item_url(item: &ItemRef) -> Option<String> {
    if let Some(rest) = item
        .key
        .strip_prefix("url-t:")
        .or_else(|| item.key.strip_prefix("url-a:"))
    {
        return Some(rest.to_string());
    }
    let mut parts = item.key.splitn(3, ':');
    let _kind = parts.next()?;
    let _id = parts.next()?;
    parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = r#"{"auto":{"results":[
        {"type":"a","id":856850876,"name":"Untrue","band_name":"Burial","album_name":null,
         "item_url_path":"https://burial.bandcamp.com/album/untrue","img":"https://f4.bcbits.com/img/a.jpg"},
        {"type":"t","id":2920211424,"name":"Burial","band_name":"JORDI GANCHITOS","album_name":"UNTRUE",
         "item_url_path":"https://jordiganchitos.bandcamp.com/track/burial","img":null},
        {"type":"b","id":999,"name":"Some Band","band_name":"Some Band","album_name":null,
         "item_url_path":"https://someband.bandcamp.com","img":null}
    ]}}"#;

    #[test]
    fn parses_a_real_search_response() {
        let offers = parse_search(SEARCH_FIXTURE, 10).unwrap();
        // The band hit is dropped: you cannot acquire a band.
        assert_eq!(offers.len(), 2);

        assert_eq!(offers[0].kind, ItemKind::Album);
        assert_eq!(offers[0].artist, "Burial");
        assert_eq!(offers[0].title, "Untrue");
        assert_eq!(
            parse_item_key(&offers[0].item_ref.key),
            Some((ItemKind::Album, 856850876))
        );

        assert_eq!(offers[1].kind, ItemKind::Track);
        assert_eq!(offers[1].album.as_deref(), Some("UNTRUE"));
        assert_eq!(
            parse_item_key(&offers[1].item_ref.key),
            Some((ItemKind::Track, 2920211424))
        );
    }

    #[test]
    fn an_item_ref_from_a_search_can_still_be_opened_in_a_browser() {
        // Bandcamp offers no id-to-url lookup, so the ref has to carry the url or
        // a saved ref could never be reopened.
        let o = &parse_search(SEARCH_FIXTURE, 1).unwrap()[0];
        assert_eq!(
            item_url(&o.item_ref).as_deref(),
            Some("https://burial.bandcamp.com/album/untrue")
        );
    }

    #[test]
    fn item_refs_round_trip_through_their_string_form() {
        let o = &parse_search(SEARCH_FIXTURE, 1).unwrap()[0];
        let s = o.item_ref.to_string();
        let back: ItemRef = s.parse().unwrap();
        assert_eq!(
            back, o.item_ref,
            "a ref printed for --offer must parse back"
        );
        assert_eq!(
            parse_item_key(&back.key),
            Some((ItemKind::Album, 856850876))
        );
    }

    #[test]
    fn item_keys_with_urls_containing_colons_still_parse() {
        // The url has its own "https:" colon, so naive splitting would break.
        let key = item_key(ItemKind::Track, 42, "https://x.bandcamp.com/track/y");
        assert_eq!(parse_item_key(&key), Some((ItemKind::Track, 42)));
        assert_eq!(
            item_url(&ItemRef::new(BackendId::Bandcamp, key)).as_deref(),
            Some("https://x.bandcamp.com/track/y")
        );
    }

    #[test]
    fn malformed_item_keys_are_rejected() {
        assert_eq!(parse_item_key("b:1:url"), None, "bands are not items");
        assert_eq!(parse_item_key("t:notanumber:url"), None);
        assert_eq!(parse_item_key("t"), None);
    }

    #[test]
    fn ownership_is_unknown_when_the_collection_could_not_be_read() {
        // Reporting "no" for an unchecked item could have the user buy it twice.
        let unknown = OwnedSet::default();
        assert_eq!(unknown.contains(ItemKind::Album, 1), Ownership::Unknown);
    }

    #[test]
    fn ownership_distinguishes_tracks_from_albums_with_the_same_id() {
        let owned = OwnedSet {
            keys: Some(["a856850876".to_string()].into_iter().collect()),
        };
        assert_eq!(
            owned.contains(ItemKind::Album, 856850876),
            Ownership::Yes {
                redownloadable: true
            }
        );
        // Same number, different kind — must not be treated as owned.
        assert_eq!(owned.contains(ItemKind::Track, 856850876), Ownership::No);
    }

    #[test]
    fn parses_a_collection_summary_into_owned_keys() {
        let body = r#"{"fan_id":123,"collection_summary":{"fan_id":123,
            "tralbum_lookup":{"a856850876":{"item_type":"a"},"t42":{"item_type":"t"}}}}"#;
        let keys = parse_collection_summary(body).unwrap();
        assert!(keys.contains("a856850876"));
        assert!(keys.contains("t42"));
    }

    #[test]
    fn a_logged_out_collection_summary_is_an_auth_error_not_an_empty_collection() {
        // Verified live: this endpoint answers HTTP 200 with this body.
        let body = r#"{"error":true,"error_message":"must be logged in"}"#;
        let err = parse_collection_summary(body).unwrap_err();
        assert!(
            matches!(err, BackendError::AuthExpired { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn joins_collection_items_to_their_redownload_urls() {
        // The join is the whole subtlety: redownload_urls is keyed by sale item,
        // items carry the tralbum id, and the two are different numbers.
        let body = r#"{"items":[
            {"tralbum_type":"a","tralbum_id":856850876,"sale_item_id":111,"sale_item_type":"p"},
            {"tralbum_type":"t","tralbum_id":42,"sale_item_id":222,"sale_item_type":"t"}],
          "redownload_urls":{"p111":"https://bandcamp.com/download?id=1",
                             "t222":"https://bandcamp.com/download?id=2"},
          "more_available":false,"last_token":null}"#;
        let page = parse_collection_items(body).unwrap();
        assert_eq!(
            page.redownload_for(ItemKind::Album, 856850876).as_deref(),
            Some("https://bandcamp.com/download?id=1")
        );
        assert_eq!(
            page.redownload_for(ItemKind::Track, 42).as_deref(),
            Some("https://bandcamp.com/download?id=2")
        );
        // Same id, wrong kind must not match.
        assert_eq!(page.redownload_for(ItemKind::Track, 856850876), None);
        assert_eq!(page.redownload_for(ItemKind::Album, 999), None);
    }

    #[test]
    fn paging_state_is_read_so_a_large_collection_can_be_walked() {
        let body = r#"{"items":[],"redownload_urls":{},
                       "more_available":true,"last_token":"1700000000::a::"}"#;
        let page = parse_collection_items(body).unwrap();
        assert!(page.more_available);
        assert_eq!(page.last_token.as_deref(), Some("1700000000::a::"));
    }

    #[test]
    fn an_empty_collection_page_is_valid() {
        // Verified live: this is what the endpoint returns unauthenticated.
        let body = r#"{"items":[],"more_available":false,"tracklists":{},
                       "redownload_urls":{},"item_lookup":{},"last_token":null}"#;
        let page = parse_collection_items(body).unwrap();
        assert!(!page.more_available);
        assert_eq!(page.redownload_for(ItemKind::Album, 1), None);
    }

    #[test]
    fn a_logged_out_collection_page_is_an_auth_error() {
        let err = parse_collection_items(r#"{"error":true,"error_message":"must be logged in"}"#)
            .unwrap_err();
        assert!(matches!(err, BackendError::AuthExpired { .. }));
    }

    #[test]
    fn reads_the_fan_id_from_either_shape() {
        assert_eq!(parse_fan_id(r#"{"fan_id":123}"#).unwrap(), 123);
        assert_eq!(
            parse_fan_id(r#"{"collection_summary":{"fan_id":456}}"#).unwrap(),
            456
        );
        assert!(parse_fan_id(r#"{"something_else":1}"#).is_err());
    }

    const DOWNLOAD_PAGE: &str = r#"<html><div id="pagedata" data-blob="{
        &quot;download_items&quot;:[{&quot;artist&quot;:&quot;Burial&quot;,
        &quot;title&quot;:&quot;Untrue&quot;,
        &quot;downloads&quot;:{
          &quot;flac&quot;:{&quot;url&quot;:&quot;https://popplers.bcbits.com/flac&quot;,&quot;size_mb&quot;:&quot;280MB&quot;},
          &quot;aiff-lossless&quot;:{&quot;url&quot;:&quot;https://popplers.bcbits.com/aiff&quot;},
          &quot;mp3-320&quot;:{&quot;url&quot;:&quot;https://popplers.bcbits.com/mp3&quot;},
          &quot;vorbis&quot;:{&quot;url&quot;:&quot;https://popplers.bcbits.com/ogg&quot;},
          &quot;some-new-format&quot;:{&quot;url&quot;:&quot;https://popplers.bcbits.com/new&quot;}}}]}"></div></html>"#;

    #[test]
    fn parses_the_per_format_download_links() {
        let links = parse_download_page(DOWNLOAD_PAGE).unwrap();
        assert_eq!(
            links[&AudioFormat::Flac],
            "https://popplers.bcbits.com/flac"
        );
        assert_eq!(
            links[&AudioFormat::Aiff],
            "https://popplers.bcbits.com/aiff"
        );
        assert!(links.contains_key(&AudioFormat::Mp3(Some(320))));
    }

    #[test]
    fn an_unknown_format_slug_does_not_break_the_other_links() {
        // Bandcamp adding a format must not stop you downloading FLAC.
        let links = parse_download_page(DOWNLOAD_PAGE).unwrap();
        assert!(links.contains_key(&AudioFormat::Flac));
        assert_eq!(
            links.len(),
            4,
            "the unrecognised slug is skipped, not fatal"
        );
    }

    #[test]
    fn the_download_page_is_authoritative_and_can_report_ogg() {
        // The search-time format list is bandcamp's standard menu; this page is
        // the truth, including formats rekordbox cannot use.
        let links = parse_download_page(DOWNLOAD_PAGE).unwrap();
        assert!(links.contains_key(&AudioFormat::Ogg));
        assert!(!AudioFormat::Ogg.usable_in_rekordbox());
    }

    #[test]
    fn format_preference_picks_flac_over_mp3_from_a_real_page() {
        let links = parse_download_page(DOWNLOAD_PAGE).unwrap();
        let pref = [
            AudioFormat::Flac,
            AudioFormat::Aiff,
            AudioFormat::Mp3(Some(320)),
        ];
        let chosen = pref.iter().find(|f| links.contains_key(f)).unwrap();
        assert_eq!(*chosen, AudioFormat::Flac);
    }

    #[test]
    fn a_page_with_no_usable_links_reports_a_shape_change() {
        let html = r#"<div id="pagedata" data-blob="{&quot;download_items&quot;:[{&quot;downloads&quot;:{}}]}"></div>"#;
        assert!(matches!(
            parse_download_page(html).unwrap_err(),
            BackendError::Parse { .. }
        ));
        let html2 = r#"<div id="pagedata" data-blob="{}"></div>"#;
        assert!(parse_download_page(html2).is_err());
        assert!(parse_download_page("<html>redesigned</html>").is_err());
    }

    #[test]
    fn reads_artist_and_title_for_naming_the_file() {
        assert_eq!(item_artist(DOWNLOAD_PAGE).as_deref(), Some("Burial"));
        assert_eq!(item_title(DOWNLOAD_PAGE).as_deref(), Some("Untrue"));
    }

    #[test]
    fn fetching_without_a_cookie_says_what_to_do_rather_than_failing_opaquely() {
        use super::super::AcquisitionBackend;
        let bc = Bandcamp::new(&Credentials::default(), Duration::from_secs(5));
        if bc.identity.is_some() {
            return; // developer has a real cookie configured
        }
        let err = bc
            .fetch(
                &ItemRef::new(BackendId::Bandcamp, "a:1:https://x/album/y"),
                &FetchOpts {
                    dest_dir: std::env::temp_dir(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: std::time::Instant::now() + Duration::from_secs(5),
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, BackendError::NoCredentials { .. }),
            "got {err:?}"
        );
        assert!(err.needs_user_action());
    }

    #[test]
    fn an_empty_collection_is_valid_not_an_error() {
        let keys =
            parse_collection_summary(r#"{"collection_summary":{"tralbum_lookup":{}}}"#).unwrap();
        assert!(keys.is_empty());
    }

    #[test]
    fn search_results_start_unprobed() {
        // Search must not guess at price, formats, or ownership.
        let o = &parse_search(SEARCH_FIXTURE, 10).unwrap()[0];
        assert!(matches!(o.pricing, Pricing::Unprobed));
        assert_eq!(o.formats, None);
        assert_eq!(o.ownership, Ownership::Unknown);
    }

    #[test]
    fn search_respects_the_limit() {
        assert_eq!(parse_search(SEARCH_FIXTURE, 1).unwrap().len(), 1);
        assert_eq!(parse_search(SEARCH_FIXTURE, 0).unwrap().len(), 0);
    }

    #[test]
    fn an_error_body_with_http_200_is_still_an_error() {
        // This is the trap: bandcamp answers 200 and puts the failure in the body.
        let err = parse_search(r#"{"error":true,"error_message":"bad function"}"#, 5).unwrap_err();
        assert!(matches!(err, BackendError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn a_logged_out_error_body_becomes_auth_expired() {
        // So the message can say "re-paste your cookie" instead of "parse error".
        let err =
            parse_search(r#"{"error":true,"error_message":"must be logged in"}"#, 5).unwrap_err();
        assert!(
            matches!(err, BackendError::AuthExpired { .. }),
            "got {err:?}"
        );
        assert!(err.needs_user_action());
    }

    #[test]
    fn an_empty_result_set_is_success_not_failure() {
        assert!(
            parse_search(r#"{"auto":{"results":[]}}"#, 5)
                .unwrap()
                .is_empty()
        );
        assert!(parse_search(r#"{}"#, 5).unwrap().is_empty());
    }

    #[test]
    fn malformed_hits_are_skipped_rather_than_failing_the_batch() {
        // One unusable row must not lose the others.
        let body = r#"{"auto":{"results":[
            {"type":"t","id":null,"name":"No Id","item_url_path":"https://x/track/y"},
            {"type":"t","id":1,"name":"Good","band_name":"B","item_url_path":"https://x/track/z"}
        ]}}"#;
        let offers = parse_search(body, 10).unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].title, "Good");
    }

    /// Shaped like the real Untrue page: GBP seller, viewer's cart in USD.
    const ALBUM_FIXTURE: &str = r#"<html>
      <div id="pagedata" data-cart="{&quot;currency&quot;:&quot;USD&quot;}"></div>
      <script data-tralbum="{&quot;item_type&quot;:&quot;album&quot;,&quot;freeDownloadPage&quot;:null,
        &quot;current&quot;:{&quot;download_pref&quot;:2,&quot;minimum_price&quot;:8.5,
        &quot;set_price&quot;:9.0,&quot;is_set_price&quot;:null},
        &quot;packages&quot;:[{&quot;currency&quot;:&quot;GBP&quot;,&quot;price&quot;:10.99}],
        &quot;trackinfo&quot;:[{&quot;duration&quot;:46.2133},{&quot;duration&quot;:100.0}]}"></script>
      <span class="buyItemExtra secondaryText">GBP</span>
      </html>"#;

    #[test]
    fn uses_the_seller_currency_not_the_viewers_cart_currency() {
        // The whole point: the cart says USD, the album sells in GBP. Labelling
        // £8.50 as dollars would make the comparison table lie.
        let facts = parse_item_page(ALBUM_FIXTURE).unwrap();
        match facts.pricing {
            Pricing::NameYourPrice {
                minimum: Some(price),
            } => {
                assert_eq!(price.currency, "GBP");
                assert_eq!(price.amount_minor, 850);
            }
            other => panic!("expected name-your-price with a minimum, got {other:?}"),
        }
    }

    #[test]
    fn a_null_is_set_price_means_name_your_price() {
        let facts = parse_item_page(ALBUM_FIXTURE).unwrap();
        assert!(matches!(facts.pricing, Pricing::NameYourPrice { .. }));
    }

    #[test]
    fn a_true_is_set_price_means_a_fixed_price() {
        let html = ALBUM_FIXTURE.replace(
            "&quot;is_set_price&quot;:null",
            "&quot;is_set_price&quot;:true",
        );
        match parse_item_page(&html).unwrap().pricing {
            Pricing::Flat(p) => {
                assert_eq!(p.to_string(), "9.00 GBP", "the set price, not the minimum");
            }
            other => panic!("expected a flat price, got {other:?}"),
        }
    }

    #[test]
    fn sums_track_durations_for_an_album() {
        assert_eq!(
            parse_item_page(ALBUM_FIXTURE).unwrap().duration_secs,
            Some(146)
        );
    }

    #[test]
    fn download_pref_one_is_free() {
        let html =
            ALBUM_FIXTURE.replace("&quot;download_pref&quot;:2", "&quot;download_pref&quot;:1");
        assert!(matches!(
            parse_item_page(&html).unwrap().pricing,
            Pricing::Free
        ));
    }

    #[test]
    fn no_download_pref_means_not_acquirable() {
        // Streaming-only or physical-only: listed, but there is nothing to fetch.
        let html =
            r#"<script data-tralbum="{&quot;current&quot;:{},&quot;packages&quot;:[]}"></script>"#;
        let facts = parse_item_page(html).unwrap();
        assert!(matches!(facts.pricing, Pricing::Unavailable { .. }));
        // And it must not be presented as something you can buy.
        let mut o = Offer::new(
            ItemRef::new(BackendId::Bandcamp, "a:1"),
            ItemKind::Album,
            "A",
            "T",
            "u",
        );
        o.pricing = facts.pricing;
        assert_eq!(o.cost_class(), CostClass::Unavailable);
    }

    #[test]
    fn a_digital_only_release_falls_back_to_the_rendered_currency() {
        // No packages array at all, which is the real shape of a digital-only
        // release; the code beside the price is then the only source.
        let html = r#"<script data-tralbum="{&quot;current&quot;:{&quot;download_pref&quot;:2,
            &quot;minimum_price&quot;:3.0,&quot;is_set_price&quot;:true,&quot;set_price&quot;:3.0}}"></script>
            <span class="buyItemExtra secondaryText">EUR</span>"#;
        match parse_item_page(html).unwrap().pricing {
            Pricing::Flat(p) => assert_eq!(p.to_string(), "3.00 EUR"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn an_unlabelled_price_is_marked_unknown_rather_than_guessed() {
        // Neither packages nor a rendered code. Inventing USD here would be the
        // worst possible outcome, so the currency is explicitly unknown.
        let html = r#"<script data-tralbum="{&quot;current&quot;:{&quot;download_pref&quot;:2,
            &quot;set_price&quot;:5.0,&quot;is_set_price&quot;:true}}"></script>"#;
        match parse_item_page(html).unwrap().pricing {
            Pricing::Flat(p) => {
                assert_eq!(p.currency, UNKNOWN_CURRENCY);
                assert_eq!(p.to_string(), "5.00 ???");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_zero_minimum_is_name_your_price_with_no_floor() {
        let html = r#"<script data-tralbum="{&quot;current&quot;:{&quot;download_pref&quot;:2,
            &quot;minimum_price&quot;:0.0,&quot;is_set_price&quot;:null}}"></script>"#;
        assert_eq!(
            parse_item_page(html).unwrap().pricing_variant_name(),
            "NameYourPrice(None)"
        );
    }

    #[test]
    fn a_free_download_page_is_noticed() {
        let html = r#"<script data-tralbum="{&quot;freeDownloadPage&quot;:&quot;https://bandcamp.com/download?x=1&quot;,
            &quot;current&quot;:{&quot;download_pref&quot;:2,&quot;minimum_price&quot;:0.0}}"></script>"#;
        assert!(parse_item_page(html).unwrap().free_download);
        assert!(!parse_item_page(ALBUM_FIXTURE).unwrap().free_download);
    }

    #[test]
    fn a_page_without_the_blob_reports_a_shape_change() {
        let err = parse_item_page("<html>redesigned</html>").unwrap_err();
        match err {
            BackendError::Parse { at, .. } => assert_eq!(at, "data-tralbum"),
            other => panic!("expected a Parse error naming the blob, got {other:?}"),
        }
        // A changed page shape must not be retried.
        assert!(!parse_item_page("<html/>").unwrap_err().is_retryable());
    }

    #[test]
    fn rendered_currency_ignores_anything_that_is_not_a_three_letter_code() {
        assert_eq!(
            rendered_currency("class=\"buyItemExtra secondaryText\">GBP<"),
            Some("GBP".into())
        );
        assert_eq!(
            rendered_currency("class=\"buyItemExtra secondaryText\">or more<"),
            None
        );
        assert_eq!(rendered_currency("nothing here"), None);
    }

    #[test]
    fn capabilities_reflect_whether_a_cookie_is_present() {
        use super::super::AcquisitionBackend;
        let anon = Bandcamp::new(&Credentials::default(), Duration::from_secs(5));
        if anon.identity.is_none() {
            let c = anon.capabilities();
            assert!(c.search, "search must work without credentials");
            assert!(!c.fetch, "fetching a purchase needs a session");
            assert!(!c.ownership_check);
            assert!(matches!(
                anon.credentials(),
                CredentialState::Missing { .. }
            ));
        }
    }

    #[test]
    fn claims_only_bandcamp_item_urls() {
        use super::super::AcquisitionBackend;
        let bc = Bandcamp::new(&Credentials::default(), Duration::from_secs(5));
        assert!(
            bc.claim_url("https://burial.bandcamp.com/album/untrue")
                .is_some()
        );
        assert!(bc.claim_url("https://x.bandcamp.com/track/y").is_some());
        // A band's landing page is not an acquirable item.
        assert!(bc.claim_url("https://burial.bandcamp.com").is_none());
        assert!(bc.claim_url("https://soundcloud.com/a/b").is_none());
    }

    #[test]
    fn a_claimed_url_round_trips_back_out_for_the_browser() {
        use super::super::AcquisitionBackend;
        let bc = Bandcamp::new(&Credentials::default(), Duration::from_secs(5));
        let url = "https://burial.bandcamp.com/album/untrue";
        let r = bc.claim_url(url).unwrap();
        match bc.purchase(&r).unwrap() {
            PurchaseFlow::OpenInBrowser { url: u, .. } => assert_eq!(u, url),
            other => panic!("got {other:?}"),
        }
    }

    // Small helper so a test can assert on a variant without matching every field.
    impl ItemFacts {
        fn pricing_variant_name(&self) -> String {
            match &self.pricing {
                Pricing::Unprobed => "Unprobed".into(),
                Pricing::Free => "Free".into(),
                Pricing::NameYourPrice { minimum: None } => "NameYourPrice(None)".into(),
                Pricing::NameYourPrice { minimum: Some(_) } => "NameYourPrice(Some)".into(),
                Pricing::Flat(_) => "Flat".into(),
                Pricing::PerFormat(_) => "PerFormat".into(),
                Pricing::Unavailable { .. } => "Unavailable".into(),
            }
        }
    }
}
