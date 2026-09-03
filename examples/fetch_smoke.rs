// Scratch harness: exercise a real SoundCloud fetch through the trait, then
// fingerprint the result against itself to prove the two halves connect.
//
// Not part of the crate's test suite: it hits the network and depends on a
// specific track staying up.
use std::time::{Duration, Instant};

use rekord_ripper::acquire::{
    AcquisitionBackend,
    soundcloud::{Cookies, SoundCloud},
    *,
};
use rekord_ripper::fingerprint as fp;

fn main() -> anyhow::Result<()> {
    // RR_SC_COOKIES_FROM_BROWSER=firefox or RR_SC_COOKIES_FILE=/path/to/jar
    // exercises the signed-in path, where the artist-enabled original and the
    // Go+ 256k AAC tier are reachable.
    let env = |k| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let cookies = match (env("RR_SC_COOKIES_FROM_BROWSER"), env("RR_SC_COOKIES_FILE")) {
        (Some(b), None) => Cookies::Browser(b),
        (None, Some(f)) => Cookies::File(f),
        (Some(_), Some(_)) => Cookies::Conflict,
        (None, None) => Cookies::None,
    };
    println!("cookies: {cookies:?}");
    let sc = SoundCloud::new("yt-dlp", cookies, vec![], Duration::from_secs(120));
    let query = SearchQuery::from_text("four tet baby", 3);

    let offers = sc.search(&query)?;
    println!("search returned {} offers", offers.len());
    let offer = offers.first().expect("no offers");
    println!("  picked: {} — {}", offer.artist, offer.title);
    println!("  ref:    {}", offer.item_ref);

    let dir = std::env::temp_dir().join("rr-fetch-test");
    std::fs::create_dir_all(&dir)?;

    let opts = FetchOpts {
        dest_dir: dir.clone(),
        format_pref: vec![
            AudioFormat::Flac,
            AudioFormat::Mp3(Some(320)),
            AudioFormat::Mp3(Some(128)),
            AudioFormat::Opus,
            AudioFormat::Aac(Some(256)),
        ],
        retention: Retention::Keep,
        overwrite: true,
        deadline: Instant::now() + Duration::from_secs(180),
    };

    let files = sc.fetch(&offer.item_ref, &opts)?;
    for f in &files {
        println!(
            "fetched {} ({}, {} bytes)",
            f.path.display(),
            f.format,
            f.bytes
        );
        assert!(f.path.exists(), "the reported path must exist");
        assert!(f.bytes > 0);

        // A file we just downloaded must fingerprint, and must match itself.
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
