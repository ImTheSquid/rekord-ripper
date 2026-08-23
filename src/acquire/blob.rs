//! Extracting JSON that Bandcamp embeds inside HTML attributes.
//!
//! Their pages carry their state in attributes like
//! `<script data-tralbum="{&quot;id&quot;:123}">` and
//! `<div id="pagedata" data-blob="...">`, so the value has to be HTML-unescaped
//! before it is valid JSON.
//!
//! This is scraping an undocumented surface: it will break when Bandcamp changes
//! their markup. Everything here therefore reports *where* it gave up, so the
//! error can say "their page format changed" rather than blaming the network.

use anyhow::{Result, anyhow};

/// Pull the value of `attr` from the first element that has it, HTML-unescape it,
/// and parse it as JSON.
pub fn attr_json(html: &str, attr: &str) -> Result<serde_json::Value> {
    let raw = attr_value(html, attr)
        .ok_or_else(|| anyhow!("no {attr} attribute in the page"))?;
    let text = unescape(&raw);
    serde_json::from_str(&text).map_err(|e| anyhow!("{attr} was not valid JSON: {e}"))
}

/// Same, but for an attribute on a specific element id — `pagedata` appears on
/// exactly one div and looking it up by id avoids matching a later `data-blob`.
pub fn id_attr_json(html: &str, id: &str, attr: &str) -> Result<serde_json::Value> {
    let needle = format!("id=\"{id}\"");
    let at = html
        .find(&needle)
        .ok_or_else(|| anyhow!("no element with id=\"{id}\" in the page"))?;
    // Scan only the remainder of that opening tag, so we can't drift into the
    // next element's attributes.
    let tag_end = html[at..].find('>').map(|e| at + e).unwrap_or(html.len());
    let raw = attr_value(&html[at..tag_end], attr)
        .ok_or_else(|| anyhow!("element id=\"{id}\" has no {attr} attribute"))?;
    serde_json::from_str(&unescape(&raw))
        .map_err(|e| anyhow!("{id}/{attr} was not valid JSON: {e}"))
}

/// The raw (still-escaped) text of a double-quoted `attr="..."` value.
pub fn attr_value(html: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = html.find(&needle)? + needle.len();
    let end = start + html[start..].find('"')?;
    Some(html[start..end].to_string())
}

/// Decode the handful of entities Bandcamp's attribute encoding actually
/// produces, plus numeric references.
///
/// A full HTML entity table would be a dependency for no gain: this input is
/// machine-generated JSON, not prose. `&amp;` is resolved last so `&amp;quot;`
/// decodes to the literal text `&quot;` rather than to a quote character.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (decoded, len) = decode_entity(tail);
        match decoded {
            Some(c) => {
                out.push_str(&c);
                rest = &tail[len..];
            }
            None => {
                // Not an entity we know — keep the '&' and carry on.
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Returns the decoded text and how many bytes of `tail` it consumed.
fn decode_entity(tail: &str) -> (Option<String>, usize) {
    const NAMED: &[(&str, char)] = &[
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&apos;", '\''),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&nbsp;", '\u{a0}'),
        ("&amp;", '&'),
    ];
    for (pat, ch) in NAMED {
        if tail.starts_with(pat) {
            return (Some(ch.to_string()), pat.len());
        }
    }
    // Numeric: &#123; or &#x1F600;
    if let Some(body) = tail.strip_prefix("&#") {
        if let Some(semi) = body.find(';') {
            let digits = &body[..semi];
            let parsed = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse::<u32>().ok(),
            };
            if let Some(c) = parsed.and_then(char::from_u32) {
                return (Some(c.to_string()), 2 + semi + 1);
            }
        }
    }
    (None, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_the_entities_bandcamp_emits() {
        assert_eq!(unescape("&quot;id&quot;"), "\"id\"");
        assert_eq!(unescape("a&lt;b&gt;c"), "a<b>c");
        assert_eq!(unescape("Bob&#39;s"), "Bob's");
        assert_eq!(unescape("R&amp;B"), "R&B");
        assert_eq!(unescape("&#8217;"), "\u{2019}");
        assert_eq!(unescape("&#x263A;"), "\u{263A}");
    }

    #[test]
    fn ampersand_is_decoded_last_so_double_escaping_survives() {
        // If &amp; were resolved first, this would wrongly become a real quote
        // and corrupt the JSON string it sits inside.
        assert_eq!(unescape("&amp;quot;"), "&quot;");
    }

    #[test]
    fn unknown_entities_are_left_alone_rather_than_dropped() {
        assert_eq!(unescape("100 &fakeentity; x"), "100 &fakeentity; x");
        assert_eq!(unescape("bare & ampersand"), "bare & ampersand");
        assert_eq!(unescape("&#notanumber;"), "&#notanumber;");
    }

    #[test]
    fn unescape_leaves_plain_text_untouched() {
        assert_eq!(unescape("nothing to do here"), "nothing to do here");
        assert_eq!(unescape(""), "");
    }

    #[test]
    fn extracts_and_parses_an_attribute_blob() {
        let html = r#"<script data-tralbum="{&quot;id&quot;:856850876,&quot;t&quot;:&quot;R&amp;amp;B&quot;}"></script>"#;
        let v = attr_json(html, "data-tralbum").unwrap();
        assert_eq!(v["id"], 856850876_i64);
        assert_eq!(v["t"], "R&amp;B");
    }

    #[test]
    fn id_lookup_does_not_drift_into_a_later_element() {
        // A naive "find data-blob" would pick up the wrong div here.
        let html = r#"<div id="other" data-blob="{&quot;x&quot;:1}"></div>
                      <div id="pagedata" data-blob="{&quot;x&quot;:2}"></div>"#;
        assert_eq!(id_attr_json(html, "pagedata", "data-blob").unwrap()["x"], 2);
    }

    #[test]
    fn id_lookup_stops_at_the_end_of_its_own_tag() {
        // pagedata has no data-blob; the attribute belongs to the *next* element,
        // so this must fail rather than silently returning someone else's data.
        let html = r#"<div id="pagedata" class="x"></div><div data-blob="{&quot;x&quot;:9}"></div>"#;
        let err = id_attr_json(html, "pagedata", "data-blob").unwrap_err().to_string();
        assert!(err.contains("has no data-blob"), "got: {err}");
    }

    #[test]
    fn missing_attribute_says_which_one() {
        let err = attr_json("<html></html>", "data-tralbum").unwrap_err().to_string();
        assert!(err.contains("data-tralbum"), "got: {err}");
    }

    #[test]
    fn invalid_json_is_reported_as_such_not_as_absent() {
        let html = r#"<script data-tralbum="{not json"></script>"#;
        let err = attr_json(html, "data-tralbum").unwrap_err().to_string();
        assert!(err.contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn missing_id_says_which_id() {
        let err = id_attr_json("<html></html>", "pagedata", "data-blob")
            .unwrap_err()
            .to_string();
        assert!(err.contains("pagedata"), "got: {err}");
    }

    #[test]
    fn attr_value_returns_the_first_match_verbatim() {
        assert_eq!(attr_value(r#"<a x="1"><b x="2">"#, "x").as_deref(), Some("1"));
        assert_eq!(attr_value("<a>", "x"), None);
        // An unterminated attribute must not panic or run off the end.
        assert_eq!(attr_value(r#"<a x="unterminated"#, "x"), None);
    }
}
