//! The vocabulary that crosses the backend boundary.
//!
//! All plain owned data: serialized to `--json`, persisted in the pending store,
//! sorted, and later routed back to a backend. Nothing here borrows and nothing
//! holds a trait object.
//!
//! An `Offer` is deliberately *not* a track. The crate already has three track
//! structs (`analysis::TrackHeader`, `dump::Track`, `tui::data::TrackRow`) and
//! all three are projections of a `djmdContent` row, keyed on an ID a remote
//! listing does not have. An offer has price, currency, formats and ownership,
//! and has no UUID, cue count, or `Analysed` bits. Unifying them would produce a
//! struct that is two-thirds `None` at every call site.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Which backend. A closed enum rather than a `String`: this is a fixed set that
/// ships with the binary, not a plugin system, and an offer needs a routing
/// token it can be serialized with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    Bandcamp,
    SoundCloud,
    Soulseek,
}

impl BackendId {
    pub const ALL: &'static [BackendId] = &[
        BackendId::Bandcamp,
        BackendId::SoundCloud,
        BackendId::Soulseek,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bandcamp => "bandcamp",
            Self::SoundCloud => "soundcloud",
            Self::Soulseek => "soulseek",
        }
    }

    /// Final tie-break in the offer table, so output is deterministic and
    /// therefore snapshot-testable.
    pub fn sort_order(self) -> u8 {
        match self {
            Self::Bandcamp => 0,
            Self::SoundCloud => 1,
            Self::Soulseek => 2,
        }
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bandcamp" | "bc" => Ok(Self::Bandcamp),
            "soundcloud" | "sc" => Ok(Self::SoundCloud),
            "soulseek" | "slsk" | "sl" => Ok(Self::Soulseek),
            other => bail!("unknown backend '{other}' (known: bandcamp, soundcloud, soulseek)"),
        }
    }
}

/// A backend-scoped handle for one listing, round-trippable through a shell
/// argument: `bandcamp:t:1234567890`, `soundcloud:track/12345`.
///
/// This, not a printed row number, is the stable scriptable identifier. It is
/// also deliberately not the URL — Bandcamp slugs change when a band renames an
/// album, so a URL is display-only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemRef {
    pub backend: BackendId,
    /// Opaque to everything except its own backend.
    pub key: String,
}

impl ItemRef {
    pub fn new(backend: BackendId, key: impl Into<String>) -> Self {
        Self {
            backend,
            key: key.into(),
        }
    }
}

impl std::fmt::Display for ItemRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.backend, self.key)
    }
}

impl FromStr for ItemRef {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (backend, key) = s
            .split_once(':')
            .ok_or_else(|| anyhow!("item ref must look like 'backend:key', got '{s}'"))?;
        if key.trim().is_empty() {
            bail!("item ref '{s}' has an empty key");
        }
        Ok(Self::new(backend.parse()?, key.trim()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Track,
    Album,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Track => "track",
            Self::Album => "album",
        })
    }
}

/// A downloadable encoding.
///
/// `Ogg` is representable because Bandcamp offers it, but it is never a default
/// preference: rekordbox cannot read it, so downloading one would succeed and be
/// useless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioFormat {
    Flac,
    Aiff,
    Wav,
    Alac,
    /// `None` when the bitrate is unknown rather than unspecified.
    Mp3(Option<u16>),
    /// LAME V0 — variable bitrate, so it has no honest single number.
    Mp3V0,
    Aac(Option<u16>),
    Ogg,
    Opus,
}

impl AudioFormat {
    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Flac | Self::Aiff | Self::Wav | Self::Alac)
    }

    /// The `djmdContent.FileType` rekordbox will assign. Inverse of
    /// `crate::format::file_type_name`. `None` means rekordbox cannot read it.
    pub fn rekordbox_file_type(self) -> Option<i64> {
        match self {
            Self::Mp3(_) | Self::Mp3V0 => Some(0),
            Self::Alac | Self::Aac(_) => Some(1),
            Self::Wav => Some(4),
            Self::Flac => Some(5),
            Self::Aiff => Some(11),
            Self::Ogg | Self::Opus => None,
        }
    }

    /// True if rekordbox can actually open this.
    pub fn usable_in_rekordbox(self) -> bool {
        self.rekordbox_file_type().is_some()
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Aiff => "aiff",
            Self::Wav => "wav",
            Self::Alac => "m4a",
            Self::Mp3(_) | Self::Mp3V0 => "mp3",
            Self::Aac(_) => "m4a",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
        }
    }

    /// Rough quality ordering for display and tie-breaks, higher is better.
    /// Not a substitute for the user's configured preference list.
    pub fn quality_rank(self) -> u16 {
        match self {
            Self::Flac | Self::Wav | Self::Aiff | Self::Alac => 1000,
            Self::Mp3(Some(k)) | Self::Aac(Some(k)) => k,
            // V0 averages around 245kbps; used only for ordering, never shown.
            Self::Mp3V0 => 245,
            Self::Mp3(None) | Self::Aac(None) => 1,
            Self::Ogg | Self::Opus => 0,
        }
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flac => f.write_str("FLAC"),
            Self::Aiff => f.write_str("AIFF"),
            Self::Wav => f.write_str("WAV"),
            Self::Alac => f.write_str("ALAC"),
            Self::Ogg => f.write_str("OGG"),
            Self::Opus => f.write_str("OPUS"),
            Self::Mp3(Some(k)) => write!(f, "MP3-{k}"),
            Self::Mp3(None) => f.write_str("MP3"),
            Self::Mp3V0 => f.write_str("MP3-V0"),
            Self::Aac(Some(k)) => write!(f, "AAC-{k}"),
            Self::Aac(None) => f.write_str("AAC"),
        }
    }
}

impl FromStr for AudioFormat {
    type Err = anyhow::Error;

    /// Accepts both our own names and Bandcamp's download slugs, so a config
    /// value and an API response parse through the same path.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().to_ascii_lowercase();
        Ok(match s.as_str() {
            "flac" => Self::Flac,
            "aiff" | "aiff-lossless" | "aif" => Self::Aiff,
            "wav" => Self::Wav,
            "alac" => Self::Alac,
            "mp3" => Self::Mp3(None),
            "mp3-320" => Self::Mp3(Some(320)),
            "mp3-v0" => Self::Mp3V0,
            "mp3-128" => Self::Mp3(Some(128)),
            // Container extensions, so a downloaded file's own extension parses
            // rather than falling back to a guess and misreporting the format.
            "aac" | "m4a" | "mp4" | "alac-m4a" => Self::Aac(None),
            "aac-hi" => Self::Aac(Some(256)),
            "vorbis" | "ogg" => Self::Ogg,
            "opus" => Self::Opus,
            other => {
                // Tolerate an unseen "mp3-NNN" rather than failing the whole parse.
                if let Some(rest) = other.strip_prefix("mp3-")
                    && let Ok(k) = rest.parse::<u16>()
                {
                    return Ok(Self::Mp3(Some(k)));
                }
                bail!("unknown audio format '{other}'")
            }
        })
    }
}

/// Money as integer minor units. Never `f64` — this is a price the user pays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    pub amount_minor: i64,
    /// ISO-4217 as the backend reported it, uppercased. Never converted.
    pub currency: String,
}

impl Price {
    pub fn new(amount_minor: i64, currency: impl AsRef<str>) -> Self {
        Self {
            amount_minor,
            currency: currency.as_ref().trim().to_ascii_uppercase(),
        }
    }

    /// Build from the major-unit float Bandcamp reports. Rounds rather than
    /// truncates, so 4.999 does not become 4.99.
    pub fn from_major(amount: f64, currency: impl AsRef<str>) -> Self {
        Self::new((amount * 100.0).round() as i64, currency)
    }

    pub fn is_zero(&self) -> bool {
        self.amount_minor == 0
    }
}

impl std::fmt::Display for Price {
    /// Always amount *and* code. A bare `$` is ambiguous across USD/CAD/AUD, and
    /// a bare number invites a comparison that isn't valid.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let major = self.amount_minor / 100;
        let minor = (self.amount_minor % 100).abs();
        write!(f, "{major}.{minor:02} {}", self.currency)
    }
}

/// One purchasable format, for stores that price per format (24-bit vs 16-bit).
/// `price: None` means "covered by the item price".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatOffer {
    pub format: AudioFormat,
    pub price: Option<Price>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pricing {
    /// Not probed yet. Renders as `?` — never as free, never as 0.00.
    #[default]
    Unprobed,
    Free,
    NameYourPrice {
        minimum: Option<Price>,
    },
    Flat(Price),
    PerFormat(Vec<FormatOffer>),
    /// Listed but not acquirable: sold out, region-locked, streaming only.
    Unavailable {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    #[default]
    Unknown,
    /// The backend has no concept of ownership.
    NotApplicable,
    No,
    Yes {
        redownloadable: bool,
    },
}

/// The primary comparison axis after quality. Ordering here is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    AlreadyOwned,
    Free,
    NameYourPrice,
    Paid,
    Unknown,
    Unavailable,
}

impl std::fmt::Display for CostClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AlreadyOwned => "owned",
            Self::Free => "free",
            Self::NameYourPrice => "name your price",
            Self::Paid => "paid",
            Self::Unknown => "?",
            Self::Unavailable => "unavailable",
        })
    }
}

/// What to look for. Three primitive fields so nothing has to depend on
/// anything — callers build it from whichever local struct they have.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The local track's duration, for the caller's scoring. Backends may ignore
    /// it; neither Bandcamp nor SoundCloud can filter on it.
    pub duration_secs: Option<i64>,
    /// Per-backend cap on raw hits. Backends must respect this.
    pub limit: usize,
}

impl SearchQuery {
    /// Free-text form, for `shop "some words"`.
    pub fn from_text(text: &str, limit: usize) -> Self {
        Self {
            title: text.trim().to_string(),
            limit,
            ..Default::default()
        }
    }

    /// What a backend should actually send. Artist and title together, because
    /// both Bandcamp's autocomplete and `scsearch:` are single-field.
    pub fn search_text(&self) -> String {
        match self.artist.as_deref() {
            Some(a) if !a.trim().is_empty() => format!("{} {}", a.trim(), self.title.trim()),
            _ => self.title.trim().to_string(),
        }
    }
}

/// A remote listing. Not a track — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub item_ref: ItemRef,
    pub kind: ItemKind,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    /// For display and for opening a browser. Not an identity.
    pub url: String,
    pub artwork_url: Option<String>,
    pub duration_secs: Option<i64>,

    /// `Unprobed` until `enrich`.
    pub pricing: Pricing,
    /// `None` = not probed. `Some(vec![])` = probed, nothing usable. That
    /// distinction is why this is not a bare `Vec`.
    pub formats: Option<Vec<AudioFormat>>,
    pub ownership: Ownership,
    /// Per-offer enrichment failure, so a partial row is visibly partial rather
    /// than quietly wrong.
    pub enrich_error: Option<String>,
}

impl Offer {
    /// A search-stage offer: identity and metadata only, nothing probed.
    pub fn new(
        item_ref: ItemRef,
        kind: ItemKind,
        artist: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            item_ref,
            kind,
            artist: artist.into(),
            title: title.into(),
            album: None,
            url: url.into(),
            artwork_url: None,
            duration_secs: None,
            pricing: Pricing::Unprobed,
            formats: None,
            ownership: Ownership::Unknown,
            enrich_error: None,
        }
    }

    pub fn backend(&self) -> BackendId {
        self.item_ref.backend
    }

    pub fn cost_class(&self) -> CostClass {
        // Ownership wins over price: a track you already bought costs nothing to
        // fetch, whatever it is still listed at.
        if matches!(self.ownership, Ownership::Yes { .. }) {
            return CostClass::AlreadyOwned;
        }
        match &self.pricing {
            Pricing::Unprobed => CostClass::Unknown,
            Pricing::Free => CostClass::Free,
            Pricing::NameYourPrice { minimum } => match minimum {
                Some(p) if !p.is_zero() => CostClass::Paid,
                _ => CostClass::NameYourPrice,
            },
            Pricing::Flat(p) if p.is_zero() => CostClass::Free,
            Pricing::Flat(_) | Pricing::PerFormat(_) => CostClass::Paid,
            Pricing::Unavailable { .. } => CostClass::Unavailable,
        }
    }

    /// `None` when formats have not been probed — not the same as "no".
    pub fn has_lossless(&self) -> Option<bool> {
        self.formats
            .as_ref()
            .map(|fs| fs.iter().any(|f| f.is_lossless()))
    }

    /// First format in `pref` that this offer actually has.
    pub fn best_format(&self, pref: &[AudioFormat]) -> Option<AudioFormat> {
        let available = self.formats.as_ref()?;
        pref.iter().copied().find(|p| available.contains(p))
    }

    /// What the user would pay for `best_format`. `None` when unprobed, free, or
    /// unavailable. Reachable only through this method — there is no top-level
    /// price field, so every caller has to say which format it is pricing.
    pub fn effective_price(&self, pref: &[AudioFormat]) -> Option<Price> {
        match &self.pricing {
            Pricing::Flat(p) => Some(p.clone()),
            Pricing::NameYourPrice { minimum } => minimum.clone(),
            Pricing::PerFormat(offers) => {
                let chosen = self.best_format(pref)?;
                offers
                    .iter()
                    .find(|o| o.format == chosen)
                    .and_then(|o| o.price.clone())
            }
            Pricing::Unprobed | Pricing::Free | Pricing::Unavailable { .. } => None,
        }
    }

    /// True when money must change hands before `fetch` can work.
    pub fn requires_purchase(&self) -> bool {
        if matches!(
            self.ownership,
            Ownership::Yes { .. } | Ownership::NotApplicable
        ) {
            return false;
        }
        matches!(
            self.cost_class(),
            CostClass::Paid | CostClass::NameYourPrice | CostClass::Unknown
        )
    }
}

/// Why a file was downloaded. Decides where it lands and whether it survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Keep it in the download directory. The candidate for import.
    Keep,
    /// Throwaway, written under a scratch directory. Needed because a source
    /// track may be a rekordbox streaming row with no local audio, so the
    /// *reference* side of a fingerprint comparison sometimes has to be fetched.
    Scratch,
}

#[derive(Debug, Clone)]
pub struct AcquiredFile {
    pub path: PathBuf,
    pub format: AudioFormat,
    pub bytes: u64,
    pub retention: Retention,
    pub source: ItemRef,
    pub source_url: String,
    /// Metadata as the backend reported it — it knows this better than reading
    /// tags back off the file would.
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FetchOpts {
    pub dest_dir: PathBuf,
    pub format_pref: Vec<AudioFormat>,
    pub retention: Retention,
    pub overwrite: bool,
    pub deadline: std::time::Instant,
}

#[derive(Debug, Clone)]
pub enum PurchaseFlow {
    NotRequired,
    AlreadyOwned,
    OpenInBrowser { url: String, note: Option<String> },
}

/// Whether a backend's credentials are present and well-formed. Offline only —
/// liveness is discovered lazily as `AuthExpired` from a real request.
#[derive(Debug, Clone)]
pub enum CredentialState {
    NotRequired,
    Present { hint: String },
    Missing { how_to_fix: String },
    Malformed { detail: String },
}

impl CredentialState {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::NotRequired | Self::Present { .. })
    }
}

/// What a backend can do. A plain struct of bools — no `bitflags` dependency for
/// six fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub search: bool,
    /// Can answer price and format questions during `enrich`.
    pub price_quotes: bool,
    /// Can answer `Ownership::Yes`/`No`, which needs credentials.
    pub ownership_check: bool,
    /// Some items need paying for before `fetch` will work.
    pub requires_purchase: bool,
    pub fetch: bool,
    /// Can produce a lossless format at all. Lets `--lossless-only` skip a
    /// backend without a single network call.
    pub lossless_capable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> Offer {
        Offer::new(
            ItemRef::new(BackendId::Bandcamp, "t:1"),
            ItemKind::Track,
            "Artist",
            "Title",
            "https://x.bandcamp.com/track/t",
        )
    }

    #[test]
    fn item_ref_round_trips_through_its_string_form() {
        for s in ["bandcamp:t:1234567890", "soundcloud:track/12345"] {
            let r: ItemRef = s.parse().unwrap();
            assert_eq!(r.to_string(), s, "round trip must be exact");
        }
    }

    #[test]
    fn item_ref_keeps_colons_in_the_key() {
        // Splitting on the *first* colon matters: bandcamp keys contain colons.
        let r: ItemRef = "bandcamp:t:99".parse().unwrap();
        assert_eq!(r.backend, BackendId::Bandcamp);
        assert_eq!(r.key, "t:99");
    }

    #[test]
    fn item_ref_rejects_malformed_input() {
        for bad in ["nocolon", "bandcamp:", "nosuchbackend:t:1", ""] {
            assert!(bad.parse::<ItemRef>().is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn backend_id_accepts_short_aliases() {
        assert_eq!("bc".parse::<BackendId>().unwrap(), BackendId::Bandcamp);
        assert_eq!(
            "SoundCloud".parse::<BackendId>().unwrap(),
            BackendId::SoundCloud
        );
        assert!("beatport".parse::<BackendId>().is_err());
    }

    #[test]
    fn bandcamp_download_slugs_parse_to_formats() {
        assert_eq!("flac".parse::<AudioFormat>().unwrap(), AudioFormat::Flac);
        assert_eq!(
            "aiff-lossless".parse::<AudioFormat>().unwrap(),
            AudioFormat::Aiff
        );
        assert_eq!(
            "mp3-320".parse::<AudioFormat>().unwrap(),
            AudioFormat::Mp3(Some(320))
        );
        assert_eq!(
            "aac-hi".parse::<AudioFormat>().unwrap(),
            AudioFormat::Aac(Some(256))
        );
        // V0 is variable-bitrate, so it must not render as a fixed number.
        assert_eq!("mp3-v0".parse::<AudioFormat>().unwrap(), AudioFormat::Mp3V0);
        assert_eq!(AudioFormat::Mp3V0.to_string(), "MP3-V0");
        assert_eq!("vorbis".parse::<AudioFormat>().unwrap(), AudioFormat::Ogg);
        // An unseen bitrate should degrade gracefully, not fail the parse.
        assert_eq!(
            "mp3-192".parse::<AudioFormat>().unwrap(),
            AudioFormat::Mp3(Some(192))
        );
        assert!("wma".parse::<AudioFormat>().is_err());
    }

    #[test]
    fn rekordbox_file_types_match_the_forward_mapping() {
        // Mirrors crate::format::file_type_name.
        assert_eq!(AudioFormat::Mp3(None).rekordbox_file_type(), Some(0));
        assert_eq!(AudioFormat::Alac.rekordbox_file_type(), Some(1));
        assert_eq!(AudioFormat::Wav.rekordbox_file_type(), Some(4));
        assert_eq!(AudioFormat::Flac.rekordbox_file_type(), Some(5));
        assert_eq!(AudioFormat::Aiff.rekordbox_file_type(), Some(11));
    }

    #[test]
    fn ogg_is_never_usable_in_rekordbox() {
        // Downloading one would succeed and be useless, so it must be excluded
        // from any default preference and reported as unusable.
        assert!(!AudioFormat::Ogg.usable_in_rekordbox());
        assert!(!AudioFormat::Opus.usable_in_rekordbox());
        assert!(AudioFormat::Flac.usable_in_rekordbox());
    }

    #[test]
    fn price_always_prints_its_currency_code() {
        assert_eq!(Price::new(400, "gbp").to_string(), "4.00 GBP");
        assert_eq!(Price::new(129, "USD").to_string(), "1.29 USD");
        assert_eq!(Price::new(1000, "eur").to_string(), "10.00 EUR");
    }

    #[test]
    fn price_from_major_rounds_rather_than_truncating() {
        assert_eq!(Price::from_major(4.999, "USD").amount_minor, 500);
        assert_eq!(Price::from_major(0.0, "USD").amount_minor, 0);
        assert_eq!(Price::from_major(1.5, "USD").to_string(), "1.50 USD");
    }

    #[test]
    fn unprobed_pricing_is_unknown_not_free() {
        // The whole point of the Unprobed variant: never imply a price we
        // haven't actually looked up.
        let o = offer();
        assert_eq!(o.cost_class(), CostClass::Unknown);
        assert_eq!(o.effective_price(&[AudioFormat::Flac]), None);
        assert_eq!(o.has_lossless(), None);
    }

    #[test]
    fn ownership_outranks_price_in_cost_class() {
        let mut o = offer();
        o.pricing = Pricing::Flat(Price::new(700, "GBP"));
        assert_eq!(o.cost_class(), CostClass::Paid);
        o.ownership = Ownership::Yes {
            redownloadable: true,
        };
        assert_eq!(o.cost_class(), CostClass::AlreadyOwned);
        assert!(
            !o.requires_purchase(),
            "already bought means nothing to buy"
        );
    }

    #[test]
    fn name_your_price_with_a_nonzero_minimum_is_paid() {
        let mut o = offer();
        o.pricing = Pricing::NameYourPrice { minimum: None };
        assert_eq!(o.cost_class(), CostClass::NameYourPrice);
        o.pricing = Pricing::NameYourPrice {
            minimum: Some(Price::new(0, "GBP")),
        };
        assert_eq!(
            o.cost_class(),
            CostClass::NameYourPrice,
            "a zero minimum is still NYP"
        );
        o.pricing = Pricing::NameYourPrice {
            minimum: Some(Price::new(400, "GBP")),
        };
        assert_eq!(
            o.cost_class(),
            CostClass::Paid,
            "a real minimum means you pay"
        );
    }

    #[test]
    fn a_zero_flat_price_is_free() {
        let mut o = offer();
        o.pricing = Pricing::Flat(Price::new(0, "USD"));
        assert_eq!(o.cost_class(), CostClass::Free);
    }

    #[test]
    fn best_format_follows_the_preference_order_not_the_available_order() {
        let mut o = offer();
        o.formats = Some(vec![
            AudioFormat::Mp3(Some(320)),
            AudioFormat::Flac,
            AudioFormat::Aiff,
        ]);
        let pref = vec![
            AudioFormat::Aiff,
            AudioFormat::Flac,
            AudioFormat::Mp3(Some(320)),
        ];
        assert_eq!(o.best_format(&pref), Some(AudioFormat::Aiff));
        assert_eq!(o.has_lossless(), Some(true));
    }

    #[test]
    fn best_format_is_none_when_nothing_matches_the_preference() {
        let mut o = offer();
        o.formats = Some(vec![AudioFormat::Ogg]);
        assert_eq!(o.best_format(&[AudioFormat::Flac]), None);
        assert_eq!(o.has_lossless(), Some(false));
    }

    #[test]
    fn probed_but_empty_formats_differ_from_unprobed() {
        let mut o = offer();
        o.formats = Some(vec![]);
        assert_eq!(o.has_lossless(), Some(false), "probed, nothing lossless");
        o.formats = None;
        assert_eq!(o.has_lossless(), None, "not probed at all");
    }

    #[test]
    fn per_format_pricing_prices_the_format_actually_chosen() {
        let mut o = offer();
        o.formats = Some(vec![AudioFormat::Flac, AudioFormat::Mp3(Some(320))]);
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
        assert_eq!(
            o.effective_price(&[AudioFormat::Flac]),
            Some(Price::new(1200, "GBP"))
        );
        assert_eq!(
            o.effective_price(&[AudioFormat::Mp3(Some(320))]),
            Some(Price::new(700, "GBP"))
        );
    }

    #[test]
    fn soundcloud_offers_never_require_a_purchase() {
        let mut o = Offer::new(
            ItemRef::new(BackendId::SoundCloud, "track/1"),
            ItemKind::Track,
            "A",
            "T",
            "https://soundcloud.com/a/t",
        );
        o.pricing = Pricing::Free;
        o.ownership = Ownership::NotApplicable;
        assert!(!o.requires_purchase());
    }

    #[test]
    fn cost_class_ordering_puts_owned_first_and_unavailable_last() {
        let mut v = vec![
            CostClass::Unavailable,
            CostClass::Paid,
            CostClass::AlreadyOwned,
            CostClass::Free,
        ];
        v.sort();
        assert_eq!(v[0], CostClass::AlreadyOwned);
        assert_eq!(*v.last().unwrap(), CostClass::Unavailable);
    }

    #[test]
    fn search_text_combines_artist_and_title() {
        let q = SearchQuery {
            title: "Roygbiv".into(),
            artist: Some("Boards of Canada".into()),
            limit: 5,
            ..Default::default()
        };
        assert_eq!(q.search_text(), "Boards of Canada Roygbiv");

        let q2 = SearchQuery::from_text("  just a title  ", 5);
        assert_eq!(q2.search_text(), "just a title");
    }

    #[test]
    fn search_text_ignores_a_blank_artist() {
        let q = SearchQuery {
            title: "T".into(),
            artist: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(q.search_text(), "T");
    }
}
