// Scratch harness: exercise a real Soulseek search and fetch through the trait,
// then fingerprint the result against itself to prove the two halves connect.
//
// Not part of the crate's test suite. It needs a reachable slskd and depends on
// whichever peers happen to be online, so it is inherently flaky in a way a test
// must not be. The scripted-slskd tests in `src/acquire/soulseek/mod.rs` cover
// the API without any of that.
//
// Reads your real config, so set [soulseek] url and an api_key first:
//
//   cargo run --example soulseek_smoke -- "burial untrue"
//
// Set RR_SMOKE_FETCH=1 to download the top offer as well. Left off by default:
// a queue position can take a long time and this is meant to be a quick check.
use std::time::{Duration, Instant};

use rekord_ripper::acquire::{AcquisitionBackend, soulseek::Soulseek, *};
use rekord_ripper::config::{Config, Credentials};
use rekord_ripper::fingerprint as fp;
use rekord_ripper::paths;

fn main() -> anyhow::Result<()> {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "aphex twin selected ambient".to_string());

    // The real config, because an slskd address is the whole point here.
    let cfg = Config::load(&paths::config_path(None)?)?;
    let creds = Credentials::load(&paths::credentials_path()?)?;
    let slsk = Soulseek::new(&cfg.soulseek, &creds, Duration::from_secs(30));

    // The same check `backends` renders, and the usual reason this harness
    // fails: no url or no api key.
    println!("credentials: {:?}", slsk.credentials());

    let query = SearchQuery::from_text(&text, 10);
    let offers = slsk.search(&query)?;
    println!("search returned {} offers", offers.len());
    for (i, o) in offers.iter().take(10).enumerate() {
        println!(
            "  {:>2}. {:<28} {:<32} {}",
            i + 1,
            clip(&o.artist, 28),
            clip(&o.title, 32),
            o.formats
                .as_deref()
                .and_then(|f| f.first())
                .map(|f| f.to_string())
                .unwrap_or_else(|| "?".into()),
        );
        println!("      {}", o.item_ref);
    }

    let Some(offer) = offers.first() else {
        println!("\nnothing found — try another query");
        return Ok(());
    };

    if std::env::var("RR_SMOKE_FETCH").is_err() {
        println!("\nOK (search only — set RR_SMOKE_FETCH=1 to download)");
        return Ok(());
    }

    let dir = std::env::temp_dir().join("rr-slsk-smoke");
    std::fs::create_dir_all(&dir)?;
    let opts = FetchOpts {
        dest_dir: dir,
        format_pref: format_preference(&cfg)?,
        retention: Retention::Keep,
        overwrite: true,
        deadline: Instant::now() + Duration::from_secs(900),
    };

    for f in slsk.fetch(&offer.item_ref, &opts)? {
        println!(
            "fetched {} ({}, {} bytes)",
            f.path.display(),
            f.format,
            f.bytes
        );
        assert!(f.path.exists(), "the reported path must exist");
        assert!(f.bytes > 0);

        // The point of the whole pipeline: a file that arrived this way has to
        // pass through the fingerprint gate like any other.
        let a = fp::fingerprint_file(&f.path, 60)?;
        println!(
            "  fingerprint: {} items, {:.1}s",
            a.items.len(),
            a.scanned_secs
        );
        let v = fp::compare(
            &a,
            &a,
            fp::SpeedEvidence::default(),
            &fp::Thresholds::default(),
        )?;
        println!("  self-compare: {}", v.summary());
        assert!(v.is_accept(), "a file must match itself");
    }
    println!("\nOK");
    Ok(())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).chain(['…']).collect()
}
