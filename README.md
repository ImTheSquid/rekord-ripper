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

# Bulk: shop for several tracks in one run, grouped per track.
rekord-ripper shop --track-id 12345678 --track-id 87654321 --json

# Open the purchase page in your browser. Payment is never automated.
rekord-ripper buy "burial untrue"

# Download something free, or something you have bought, and queue the transfer.
rekord-ripper fetch --offer bandcamp:a:856850876 --src-track-id 12345678
rekord-ripper fetch https://soundcloud.com/artist/track --src-track-id 12345678

# Apply queued transfers once rekordbox has imported the files.
rekord-ripper pending --list
rekord-ripper pending --apply
```

### In the TUI

The TUI has two screens, and each one's selection means exactly one thing.

The **transfer screen** is the src → dst view. `Space` picks destinations, and
that is all it does — the source is always the highlighted row. `s` crosses to
the shop screen, landing on the track you were on.

The **shop screen** is a track list beside an offer table. `s` searches the
highlighted track; tap it on several and they search one after another, results
accumulating into one grouped table — nothing is discarded and nothing is
searched twice. `Space` fills a basket and `S` searches all of it. Each track
carries a tag showing what its search found: a count, `·` for nothing, `…` for
still queued. `Enter` on an offer downloads it and queues an analysis transfer
against that offer's *own* source track, which after a batch of searches is not
necessarily the one under the list cursor. `Esc` goes back.

Searches run on a background thread, so leaving the screen loses nothing and `s`
brings it back. They run sequentially rather than in parallel: each track is
already a fan-out across every backend, so firing several at once would multiply
requests per backend and invite a rate limit.

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

Rekordbox has no watch-folder feature, so by default importing a downloaded file
is a manual drag of the download directory. That costs one drag per batch, not per
file — or turn on row insertion below and skip it.

## Creating rekordbox rows directly

`rekord-ripper import` writes the `djmdContent` row itself, so a downloaded file
appears in your collection without the drag. With `--src-track-id` it also runs
the fingerprint-gated transfer in the same command:

```bash
rekord-ripper import "new.flac"                            # dry-run: shows every value
rekord-ripper import "new.flac" --src-track-id 12345678 --apply
rekord-ripper import --undo 3052064790 --apply             # changed your mind
```

It reads the file's embedded tags and reuses existing artist/album/genre rows
rather than duplicating them. Three gates stand in front of it: the config key
`insert_content_rows` (off by default), `--apply`, and a confirmation showing the
full row — plus the same running-rekordbox refusal and automatic backup as `cp`.

Undo is a tombstone (`rb_local_deleted = 1` with a USN bump), not a delete,
because on a cloud-synced library a hard delete would leave your other devices
holding a row for a file they don't have. Every insert also writes an
`<backup>.inserted.json` note next to the backup so it can be undone later.

Worth knowing: this is not as exotic as it sounds. `cp` already inserts rows into
five tables and clears `rb_local_synced`, so it has always written rows your cloud
agent pushes. The genuinely new part is that a *track* row points at a file, so
under Cloud Library Sync rekordbox may upload that audio and rewrite `FolderPath`.

`REKORDBOX_DIR` overrides the rekordbox directory, which is how the write paths
are tested against a copy of `master.db` rather than the real thing.
