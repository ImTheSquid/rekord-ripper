//! The library filter language, shared by the TUI's `/` boxes and `dump`.
//!
//! Modelled on a web search box, because that is what everyone's fingers already
//! know: bare words are ANDed and order-independent, `"…"` is a phrase, `-` is
//! exclusion, uppercase `OR` alternates, and `p:` restricts to a playlist.
//!
//! Two deliberate departures from Google. Every term matches as a *substring*,
//! not a whole word, because these boxes filter as you type and `buri` has to
//! narrow before `burial` is finished. And there is no stemming, no synonyms and
//! no ranking — a filter either keeps a row or drops it.

/// What a query is matched against. Text fields are all pre-lowercased;
/// numbers stay in the units `master.db` stores them in.
#[derive(Clone, Copy, Default)]
pub struct Fields<'a> {
    /// `"{title} {artist}"`.
    pub text: &'a str,
    /// Folder-qualified playlist paths, one per line.
    pub playlists: &'a str,
    /// Space-delimited, space-padded keywords, from `format::track_tags`.
    pub tags: &'a str,
    /// Hundredths of a BPM. `None` when the track was never analysed.
    pub bpm: Option<i64>,
    /// Track length in whole seconds.
    pub length: Option<i64>,
}

/// The `text` haystack, built the one way so the TUI and the CLI agree on what
/// a bare word is searching.
pub fn text_blob(title: &str, artist: &str) -> String {
    format!("{} {}", title.to_lowercase(), artist.to_lowercase())
}

/// Rebuild one query string out of argv, restoring the quotes the shell ate off
/// a field value.
///
/// `dump p:"jack night"` reaches us as one argument, `p:jack night`. Joining on
/// spaces would read that as `p:jack AND night` — a query that quietly matches
/// the wrong thing instead of failing, so the value gets its quotes back.
///
/// Only a field value is re-quoted. `dump 'burial OR zomby'` arrives as one
/// argument too, and there the spaces are separating terms exactly as intended.
pub fn join_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| requote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn requote(arg: &str) -> String {
    // A quote already present says what it means.
    if arg.contains('"') || !arg.chars().any(char::is_whitespace) {
        return arg.to_string();
    }
    let negation = if arg.starts_with('-') { "-" } else { "" };
    let rest = arg.strip_prefix('-').unwrap_or(arg);
    match PLAYLIST_PREFIXES
        .iter()
        .find_map(|p| rest.strip_prefix(p).map(|v| (*p, v)))
    {
        Some((prefix, value)) => format!("{negation}{prefix}\"{value}\""),
        None => arg.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Text,
    Playlist,
    Tag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumField {
    Bpm,
    Length,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Matcher {
    Contains {
        field: Field,
        /// Lowercased needle.
        needle: String,
    },
    /// Inclusive bounds, in the field's stored units. Either end may be open.
    Between {
        field: NumField,
        lo: Option<i64>,
        hi: Option<i64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    matcher: Matcher,
    negated: bool,
}

impl Term {
    fn matches(&self, f: Fields<'_>) -> bool {
        let hit = match &self.matcher {
            Matcher::Contains { field, needle } => {
                let hay = match field {
                    Field::Text => f.text,
                    Field::Playlist => f.playlists,
                    Field::Tag => f.tags,
                };
                hay.contains(needle.as_str())
            }
            Matcher::Between { field, lo, hi } => {
                let value = match field {
                    NumField::Bpm => f.bpm,
                    NumField::Length => f.length,
                };
                // A row with no value simply does not match, so `-bpm:>120`
                // keeps the unanalysed tracks. Excluding a property nobody can
                // see should not also exclude the rows that lack it.
                value.is_some_and(|v| lo.is_none_or(|lo| v >= lo) && hi.is_none_or(|hi| v <= hi))
            }
        };
        hit != self.negated
    }
}

/// A parsed filter: an AND of groups, each group an OR of terms.
///
/// `burial OR zomby p:jn4` is two groups — (burial OR zomby) AND (in jn4) —
/// which is the precedence a search box implies and the one Google uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    groups: Vec<Vec<Term>>,
}

/// Field prefixes, longest first so `playlist:` is not read as `p:`.
const PLAYLIST_PREFIXES: [&str; 2] = ["playlist:", "p:"];

/// Prefixes for the keyword vocabulary. Three spellings, one namespace: whether
/// you reach for `is:flac`, `type:flac` or `has:cues`, the natural one works.
const TAG_PREFIXES: [&str; 3] = ["is:", "has:", "type:"];

const BPM_PREFIXES: [&str; 1] = ["bpm:"];
const LENGTH_PREFIXES: [&str; 2] = ["length:", "len:"];

fn range_term(field: NumField, raw: &str, unit: Unit, negated: bool) -> Option<Term> {
    let (lo, hi) = parse_range(raw, unit)?;
    Some(Term {
        matcher: Matcher::Between { field, lo, hi },
        negated,
    })
}

/// A number as typed, plus the window its precision implies.
///
/// `bpm:128` means "a hundred and twenty-eight something", not 128.00 exactly —
/// an analysed track sitting at 127.98 is what the person meant. So a value
/// carries the size of the step it was typed at, and a bare term matches
/// `[value, value + window)`. Type more digits to narrow it: `bpm:128.5` is a
/// tenth wide, `bpm:128.55` a hundredth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scalar {
    value: i64,
    window: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    /// Hundredths of a BPM.
    Bpm,
    /// Whole seconds.
    Seconds,
}

impl Unit {
    fn scalar(self, s: &str) -> Option<Scalar> {
        match self {
            Unit::Bpm => bpm_scalar(s),
            Unit::Seconds => duration_scalar(s),
        }
    }
}

/// `128` → 128.00 a whole BPM wide, `128.5` → a tenth, `128.55` → a hundredth.
fn bpm_scalar(s: &str) -> Option<Scalar> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (!frac.is_empty() && !digits(frac)) || frac.len() > 2 {
        return None;
    }
    let (hundredths, window) = match frac.len() {
        0 => (0, 100),
        1 => (frac.parse::<i64>().ok()? * 10, 10),
        _ => (frac.parse::<i64>().ok()?, 1),
    };
    Some(Scalar {
        value: whole.parse::<i64>().ok()? * 100 + hundredths,
        window,
    })
}

/// `210` seconds, `3m30s`, `3m` (three-something minutes), or `3:30`.
fn duration_scalar(s: &str) -> Option<Scalar> {
    if let Some((mins, secs)) = s.split_once(':') {
        if secs.len() != 2 || !secs.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let secs: i64 = secs.parse().ok()?;
        if secs > 59 {
            return None;
        }
        return Some(Scalar {
            value: mins.parse::<i64>().ok()? * 60 + secs,
            window: 1,
        });
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Scalar {
            value: s.parse().ok()?,
            window: 1,
        });
    }
    // A run of number+unit pairs. The smallest unit named sets the precision,
    // so `3m` spans a minute but `3m30s` is exact.
    let mut total = 0;
    let mut window = i64::MAX;
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let step = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total += digits.parse::<i64>().ok()? * step;
        digits.clear();
        window = window.min(step);
    }
    // Trailing digits with no unit (`3m30`) are a typo, not a duration.
    if !digits.is_empty() || window == i64::MAX {
        return None;
    }
    Some(Scalar {
        value: total,
        window,
    })
}

/// `>=128`, `<3m`, `120-130`, `120..130`, or a bare value.
///
/// Only the bare form is widened to its typed precision. A comparison or a span
/// means the number you actually wrote, because that is the only reading that
/// survives a coarse unit: `len:>6m` has to mean "longer than six minutes", not
/// "longer than six-something minutes".
///
/// `None` for anything unparseable, which drops the term rather than matching
/// nothing — the box filters as you type, and `bpm:12` on the way to `bpm:128`
/// must not blank the list.
fn parse_range(raw: &str, unit: Unit) -> Option<(Option<i64>, Option<i64>)> {
    let raw = raw.trim();
    // Longest operators first, so `>=` is never read as `>`.
    if let Some(rest) = raw.strip_prefix(">=") {
        return Some((Some(unit.scalar(rest)?.value), None));
    }
    if let Some(rest) = raw.strip_prefix("<=") {
        return Some((None, Some(unit.scalar(rest)?.value)));
    }
    if let Some(rest) = raw.strip_prefix('>') {
        return Some((Some(unit.scalar(rest)?.value + 1), None));
    }
    if let Some(rest) = raw.strip_prefix('<') {
        return Some((None, Some(unit.scalar(rest)?.value - 1)));
    }
    if let Some((a, b)) = split_span(raw) {
        return Some((Some(unit.scalar(a)?.value), Some(unit.scalar(b)?.value)));
    }
    let s = unit.scalar(raw)?;
    Some((Some(s.value), Some(s.value + s.window - 1)))
}

/// Split `A..B` or `A-B`. The hyphen is only ever a separator here: the leading
/// `-` was taken as negation before this ran, and neither a BPM nor a duration
/// is negative.
fn split_span(raw: &str) -> Option<(&str, &str)> {
    if let Some(at) = raw.find("..") {
        return Some((&raw[..at], &raw[at + 2..]));
    }
    // From index 1: a hyphen in front has nothing on its left to be a span of.
    let at = raw.get(1..)?.find('-')? + 1;
    Some((&raw[..at], &raw[at + 1..]))
}

impl Query {
    pub fn parse(raw: &str) -> Self {
        let mut groups: Vec<Vec<Term>> = Vec::new();
        // Set by an `OR`: the next term joins the group just closed rather than
        // opening one of its own.
        let mut alternate = false;
        for token in tokenize(raw) {
            if token.is_operator() {
                alternate = !groups.is_empty();
                continue;
            }
            let Some(term) = token.into_term() else {
                continue;
            };
            match groups.last_mut() {
                Some(group) if alternate => group.push(term),
                _ => groups.push(vec![term]),
            }
            alternate = false;
        }
        Self { groups }
    }

    /// True when the query constrains nothing, so every row is kept.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Whether any term needs playlist membership, so a caller can skip loading
    /// it when nothing asks.
    pub fn touches_playlists(&self) -> bool {
        self.groups.iter().flatten().any(|t| {
            matches!(
                t.matcher,
                Matcher::Contains {
                    field: Field::Playlist,
                    ..
                }
            )
        })
    }

    pub fn matches(&self, f: Fields<'_>) -> bool {
        self.groups.iter().all(|g| g.iter().any(|t| t.matches(f)))
    }
}

/// One whitespace-separated chunk of the input, with its quotes already resolved.
struct Token {
    /// Quotes removed; a quoted run keeps its spaces.
    text: String,
    /// Whether any part of it was quoted. A quoted `"OR"` is a search term.
    quoted: bool,
}

impl Token {
    fn is_operator(&self) -> bool {
        // Uppercase only, as in a web search box: lowercase "or" is a word
        // people put in titles, and swallowing it would be a silent surprise.
        !self.quoted && (self.text == "OR" || self.text == "|")
    }

    /// `-p:"jack night"` → a negated playlist term. `None` when nothing is left
    /// to match on, which is what a half-typed `p:` or a lone `-` is.
    fn into_term(self) -> Option<Term> {
        let rest = self.text.strip_prefix('-');
        let negated = rest.is_some();
        let rest = rest.unwrap_or(&self.text);

        let strip = |set: &[&str]| set.iter().find_map(|p| rest.strip_prefix(p));

        // Numeric fields first: they own their whole value, so nothing about
        // them should fall through to a substring search.
        if let Some(v) = strip(&BPM_PREFIXES) {
            return range_term(NumField::Bpm, v, Unit::Bpm, negated);
        }
        if let Some(v) = strip(&LENGTH_PREFIXES) {
            return range_term(NumField::Length, v, Unit::Seconds, negated);
        }

        let (field, value) = match (strip(&PLAYLIST_PREFIXES), strip(&TAG_PREFIXES)) {
            (Some(v), _) => (Field::Playlist, v.to_owned()),
            (_, Some(v)) => (Field::Tag, v.to_owned()),
            _ => (Field::Text, rest.to_owned()),
        };
        let value = value.trim().to_lowercase();
        if value.is_empty() {
            return None;
        }
        // Tags are a closed vocabulary, so a term matches a whole one. Padding
        // the needle here keeps `matches` a plain substring test.
        let needle = match field {
            Field::Tag => format!(" {value} "),
            _ => value,
        };
        Some(Term {
            matcher: Matcher::Contains { field, needle },
            negated,
        })
    }
}

/// Split on whitespace, except inside quotes.
///
/// An unterminated quote runs to the end of the input rather than being an
/// error: the list has to keep narrowing while `p:"jack ni` is still being
/// typed.
fn tokenize(raw: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut started = false;

    for c in raw.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    tokens.push(Token {
                        text: std::mem::take(&mut cur),
                        quoted,
                    });
                }
                quoted = false;
                started = false;
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(Token { text: cur, quoted });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(text: &'a str, playlists: &'a str) -> Fields<'a> {
        Fields {
            text,
            playlists,
            ..Fields::default()
        }
    }

    fn tagged(tags: &str) -> Fields<'_> {
        Fields {
            tags,
            ..Fields::default()
        }
    }

    fn hits(query: &str, text: &str) -> bool {
        Query::parse(query).matches(fields(text, ""))
    }

    #[test]
    fn bare_words_are_anded_and_order_does_not_matter() {
        assert!(hits("burial untrue", "untrue burial"));
        assert!(hits("untrue burial", "untrue burial"));
        assert!(!hits("burial zomby", "untrue burial"));
    }

    #[test]
    fn a_word_matches_as_a_substring_so_typing_narrows_as_you_go() {
        for typed in ["b", "bur", "burial"] {
            assert!(hits(typed, "untrue burial"), "{typed:?} should still match");
        }
    }

    #[test]
    fn a_quoted_phrase_has_to_be_contiguous() {
        assert!(hits("\"untrue burial\"", "untrue burial"));
        assert!(!hits("\"burial untrue\"", "untrue burial"));
    }

    #[test]
    fn a_minus_excludes() {
        assert!(hits("burial -zomby", "untrue burial"));
        assert!(!hits("burial -untrue", "untrue burial"));
        // An all-negative query keeps everything it does not name.
        assert!(hits("-zomby", "untrue burial"));
    }

    #[test]
    fn or_alternates_within_one_and_group() {
        assert!(hits("burial OR zomby", "untrue burial"));
        assert!(hits("burial OR zomby", "dedicated zomby"));
        assert!(!hits("kode9 OR zomby", "untrue burial"));
        // `|` is the same operator.
        assert!(hits("kode9 | burial", "untrue burial"));
        // OR binds tighter than the implicit AND between groups.
        assert!(hits("burial OR zomby untrue", "untrue burial"));
        assert!(!hits("burial OR zomby untrue", "dedicated zomby"));
    }

    #[test]
    fn lowercase_or_is_a_word_not_an_operator() {
        assert!(hits("or", "or nah"));
        assert!(!hits("burial or zomby", "untrue burial"));
    }

    #[test]
    fn playlist_terms_match_the_playlist_field_only() {
        let row = fields("untrue burial", "jack night/jn4");
        assert!(Query::parse("p:jn4").matches(row));
        assert!(Query::parse("playlist:jn4").matches(row));
        assert!(Query::parse("p:\"jack night\" burial").matches(row));
        assert!(!Query::parse("p:jn5").matches(row));
        // The playlist name is not searched by free text, nor the reverse.
        assert!(!Query::parse("jn4").matches(row));
        assert!(!Query::parse("p:burial").matches(row));
    }

    #[test]
    fn a_negated_playlist_term_excludes_members() {
        let in_jn4 = fields("untrue burial", "jack night/jn4");
        let loose = fields("untrue burial", "");
        assert!(!Query::parse("-p:jn4").matches(in_jn4));
        assert!(Query::parse("-p:jn4").matches(loose));
    }

    #[test]
    fn an_empty_or_half_typed_query_keeps_everything() {
        for q in ["", "   ", "p:", "playlist:", "-", "\"", "p:\"", "OR"] {
            assert!(Query::parse(q).is_empty(), "{q:?} should constrain nothing");
        }
        // An unterminated quote still filters on what has been typed so far.
        assert!(Query::parse("p:\"jack ni").matches(fields("", "jack night/jn4")));
        assert!(!Query::parse("p:\"jack ni").matches(fields("", "archive")));
    }

    #[test]
    fn a_colon_inside_ordinary_text_is_not_a_field() {
        assert!(hits("remix:", "untrue (remix: vip)"));
        assert!(hits("clap:clap", "clap:clap"));
    }

    #[test]
    fn keyword_terms_match_a_whole_tag_under_any_of_the_three_spellings() {
        let row = tagged(" local flac lossless cues ");
        for q in ["is:flac", "type:flac", "has:flac", "is:lossless is:cues"] {
            assert!(Query::parse(q).matches(row), "{q:?} should match");
        }
        assert!(!Query::parse("is:mp3").matches(row));
        // Whole tags only — a prefix of one is not a match.
        assert!(!Query::parse("is:fla").matches(row));
        assert!(!Query::parse("is:loc").matches(row));
    }

    #[test]
    fn keyword_terms_negate_and_combine_with_the_rest() {
        let stream = Fields {
            text: "untrue burial",
            playlists: "jack night/jn4",
            tags: " stream ",
            ..Fields::default()
        };
        assert!(Query::parse("p:jn4 is:stream").matches(stream));
        assert!(Query::parse("burial -is:local").matches(stream));
        assert!(!Query::parse("burial is:local").matches(stream));
        assert!(Query::parse("is:local OR is:stream").matches(stream));
    }

    #[test]
    fn origin_tags_are_mutually_exclusive() {
        // Cloud is a real file rekordbox syncs, so it is neither of the others —
        // `-is:stream` is how you ask for "has a file at all".
        let cloud = tagged(" cloud flac lossless ");
        assert!(Query::parse("is:cloud").matches(cloud));
        assert!(!Query::parse("is:local").matches(cloud));
        assert!(!Query::parse("is:stream").matches(cloud));
        assert!(Query::parse("-is:stream is:lossless").matches(cloud));
    }

    #[test]
    fn a_keyword_term_does_not_leak_into_the_text_search() {
        let row = Fields {
            text: "local heroes",
            playlists: "",
            tags: " stream ",
            ..Fields::default()
        };
        assert!(!Query::parse("is:local").matches(row));
        assert!(Query::parse("local").matches(row));
    }

    fn numeric(bpm: Option<i64>, length: Option<i64>) -> Fields<'static> {
        Fields {
            bpm,
            length,
            ..Fields::default()
        }
    }

    fn bpm_hits(query: &str, bpm: f64) -> bool {
        Query::parse(query).matches(numeric(Some((bpm * 100.0).round() as i64), None))
    }

    fn len_hits(query: &str, secs: i64) -> bool {
        Query::parse(query).matches(numeric(None, Some(secs)))
    }

    #[test]
    fn a_bare_bpm_covers_what_was_typed_not_an_exact_hundredth() {
        // The point of the window: an analysed track is never exactly 128.00.
        assert!(bpm_hits("bpm:128", 128.0));
        assert!(bpm_hits("bpm:128", 128.02));
        assert!(bpm_hits("bpm:128", 128.99));
        assert!(!bpm_hits("bpm:128", 127.99));
        assert!(!bpm_hits("bpm:128", 129.0));
        // More digits, a narrower window.
        assert!(bpm_hits("bpm:128.5", 128.55));
        assert!(!bpm_hits("bpm:128.5", 128.6));
        assert!(bpm_hits("bpm:128.55", 128.55));
        assert!(!bpm_hits("bpm:128.55", 128.56));
    }

    #[test]
    fn a_comparison_means_the_number_that_was_typed() {
        // Not the band the bare form would have covered: 128.5 is over 128.
        assert!(bpm_hits("bpm:>128", 128.01));
        assert!(!bpm_hits("bpm:>128", 128.0));
        assert!(bpm_hits("bpm:>=128", 128.0));
        assert!(!bpm_hits("bpm:>=128", 127.99));
        assert!(bpm_hits("bpm:<128", 127.99));
        assert!(!bpm_hits("bpm:<128", 128.0));
        assert!(bpm_hits("bpm:<=128", 128.0));
        assert!(!bpm_hits("bpm:<=128", 128.01));
    }

    #[test]
    fn a_span_is_inclusive_at_both_ends() {
        for spelling in ["bpm:120-130", "bpm:120..130"] {
            assert!(bpm_hits(spelling, 120.0), "{spelling}");
            assert!(bpm_hits(spelling, 125.5), "{spelling}");
            assert!(bpm_hits(spelling, 130.0), "{spelling}");
            assert!(!bpm_hits(spelling, 130.01), "{spelling}");
            assert!(!bpm_hits(spelling, 119.99), "{spelling}");
        }
    }

    #[test]
    fn durations_take_seconds_units_or_a_clock() {
        assert!(len_hits("len:210", 210));
        assert!(!len_hits("len:210", 211));
        assert!(len_hits("len:3:30", 210));
        assert!(len_hits("len:3m30s", 210));
        // A bare unit spans that unit: `3m` is three-something minutes.
        assert!(len_hits("len:3m", 180));
        assert!(len_hits("len:3m", 239));
        assert!(!len_hits("len:3m", 240));
        assert!(len_hits("length:>6m", 361));
        assert!(!len_hits("length:>6m", 360));
        assert!(len_hits("len:3m-6m", 200));
        assert!(!len_hits("len:3m-6m", 400));
    }

    #[test]
    fn a_row_with_no_value_is_excluded_but_a_negated_term_keeps_it() {
        let unanalysed = numeric(None, None);
        assert!(!Query::parse("bpm:128").matches(unanalysed));
        assert!(!Query::parse("bpm:>100").matches(unanalysed));
        // Excluding a property nobody can see must not also drop the rows
        // that simply lack it.
        assert!(Query::parse("-bpm:>100").matches(unanalysed));
    }

    #[test]
    fn an_unparseable_range_drops_the_term_rather_than_the_list() {
        // Mid-keystroke states, and outright typos.
        for q in [
            "bpm:", "bpm:>", "bpm:12x", "bpm:120-", "len:3m30", "len:1:2",
        ] {
            assert!(Query::parse(q).is_empty(), "{q:?} should constrain nothing");
        }
        // Three decimal places is more precision than master.db stores.
        assert!(Query::parse("bpm:128.555").is_empty());
    }

    #[test]
    fn numeric_terms_compose_with_everything_else() {
        let row = Fields {
            text: "untrue burial",
            playlists: "jack night/jn4",
            tags: " local present flac ",
            bpm: Some(13800),
            length: Some(320),
        };
        assert!(Query::parse("p:jn4 bpm:138 is:flac").matches(row));
        assert!(Query::parse("bpm:130-140 len:>5m burial").matches(row));
        assert!(!Query::parse("bpm:120-130").matches(row));
        assert!(Query::parse("bpm:120-130 OR bpm:130-140").matches(row));
        assert!(Query::parse("burial -bpm:120-130").matches(row));
    }

    #[test]
    fn argv_keeps_the_quoting_the_shell_ate() {
        let argv = |v: &[&str]| join_argv(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // `dump p:"jack night" burial` — the shell already removed the quotes.
        assert_eq!(argv(&["p:jack night", "burial"]), "p:\"jack night\" burial");
        assert_eq!(argv(&["-p:jack night"]), "-p:\"jack night\"");
        assert_eq!(argv(&["playlist:jack night"]), "playlist:\"jack night\"");
        // Ordinary words are left exactly as typed.
        assert_eq!(argv(&["burial", "untrue"]), "burial untrue");
        assert_eq!(argv(&["burial", "OR", "zomby"]), "burial OR zomby");
        // A whole query in single quotes reaches us as one argument, and its
        // spaces are separating terms. Re-quoting it would collapse the lot
        // into a single phrase and silently drop the operators.
        assert_eq!(argv(&["burial OR zomby"]), "burial OR zomby");
        assert_eq!(argv(&["is:cloud is:lossless"]), "is:cloud is:lossless");
        assert_eq!(
            argv(&["p:\"jack night\" -remix"]),
            "p:\"jack night\" -remix"
        );
    }

    #[test]
    fn a_shell_quoted_playlist_name_survives_the_round_trip() {
        let q = Query::parse(&join_argv(&["p:jack night".to_string()]));
        assert!(q.matches(fields("", "jack night/jn4")));
        assert!(!q.matches(fields("night", "jack/slush")));
    }

    #[test]
    fn playlists_are_only_loaded_when_something_asks_for_them() {
        assert!(!Query::parse("burial untrue").touches_playlists());
        assert!(Query::parse("burial p:jn4").touches_playlists());
        assert!(Query::parse("-p:jn4").touches_playlists());
    }
}
