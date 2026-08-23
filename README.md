# Rekord Ripper

A utility to transfer Rekordbox analysis data across songs, and to find better
copies of the tracks you already have.

https://github.com/user-attachments/assets/0c9d8a70-0058-4870-9988-0bb712931234

## Installation

```bash
cargo install rekord-ripper
```

`shop`, `buy` and `fetch` also want `yt-dlp` and `ffmpeg` on your PATH:

```bash
brew install yt-dlp ffmpeg
```

Run `rekord-ripper backends` at any time to see what is configured and what is
missing.

## Transferring analysis

The basic version is `rekord-ripper tui`, which just drops you into the TUI. You
can also manually copy with `rekord-ripper cp`, auto copy with
`rekord-ripper auto`, and dump the database with `rekord-ripper dump`. See the
help pages for more information.

## Acquisition backends

Most of a library ends up on SoundCloud, where the audio is a 128kbps transcode.
The same tracks are usually buyable in lossless on Bandcamp. These commands find
them, help you buy them, fetch the file, and move your cue points and beat grid
onto it.

```bash
# Search every backend at once and compare what is on offer.
rekord-ripper shop "burial untrue"
rekord-ripper shop --track-id 12345678 --lossless-only

# Open the purchase page in your browser. Payment is never automated.
rekord-ripper buy "burial untrue"

# Download something free, or something you have bought, and queue the transfer.
rekord-ripper fetch --offer bandcamp:a:856850876 --src-track-id 12345678
rekord-ripper fetch https://soundcloud.com/artist/track --src-track-id 12345678

# Apply queued transfers once rekordbox has imported the files.
rekord-ripper pending --list
rekord-ripper pending --apply
```

Backends implement the `AcquisitionBackend` trait, so adding another is a matter
of implementing search, enrich, purchase and fetch. Bandcamp and SoundCloud ship
with it.

### Things it deliberately does not do

- **Buy anything for you.** Bandcamp checkout is a card flow in their web UI with
  no API behind it. `buy` gets you to the right page; you pay.
- **Compare prices across currencies.** Prices come in each seller's own
  currency and there is no exchange-rate source here, so prices are always shown
  with their ISO code and any "cheapest" line is per-currency.
- **Pretend a SoundCloud rip is an upgrade.** Free tracks cap at MP3-128 unless
  the artist enabled the original file, and Go+ tracks are DRM'd and simply fail.
  `fetch` reports the format it actually got and says when it is a downgrade.

### The fingerprint gate

A transfer only fires when an audio fingerprint says the two files are the same
recording **and** that they are time-aligned. The second half matters: cue points
are copied as absolute timestamps and the beat grid is copied as opaque ANLZ
binary, so a same-but-shifted pair would put every cue in the wrong place with no
way to compensate. It fails closed on either axis.

The thresholds ship deliberately loose. Calibrate them against your own library:

```bash
rekord-ripper fp path/to/soundcloud-rip.mp3 path/to/bandcamp.flac
```

That prints the per-segment scores, coverage, and the implied time offset. Run it
over pairs you know are the same track and pairs you know are not, then set
`score_max` and `coverage_min` in `config.toml` from the gap between them.

Known limit: one fingerprint item is ~124ms, so shifts below about **62ms** are
invisible and a ~50ms offset will be accepted. That is the resolution floor, not
a bug — the accept message states it.

## Configuration

```bash
rekord-ripper config            # where it lives
rekord-ripper config --init     # write a starter file
```

Bandcamp downloads need the `identity` cookie from a logged-in browser session,
in `credentials.toml` next to `config.toml` (or `BANDCAMP_IDENTITY` in the
environment). Keep that file mode 600 — it is a full-account credential, not a
read-only API key.

Rekordbox has no watch-folder feature, so importing a downloaded file is a manual
drag of the download directory. That costs one drag per batch, not per file.
