# Rekord Ripper

A utility to transfer Rekordbox analysis data across songs, and to find better
copies of the tracks you already have.

https://github.com/user-attachments/assets/0c9d8a70-0058-4870-9988-0bb712931234

## Installation

```bash
cargo install rekord-ripper
```

`shop`, `buy` and `fetch` require `yt-dlp` and `ffmpeg` on your PATH:

```bash
brew install yt-dlp ffmpeg
```

The Soulseek backend requires a reachable [slskd](https://github.com/slskd/slskd).
It is optional. Until one is configured, Soulseek reports itself as unconfigured
and the other backends continue to work. Run slskd on a machine that stays
awake: a Soulseek queue position can take hours, and a sleeping laptop loses it.

`rekord-ripper backends` lists what is configured and what is missing.

## Transferring analysis

`rekord-ripper tui` opens the TUI. `rekord-ripper cp` copies manually,
`rekord-ripper auto` copies automatically, and `rekord-ripper dump` prints the
database. See the help pages for the full options.

### Searching your library

Every `/` box in the TUI and `rekord-ripper dump` accept the same query syntax,
which follows the usual web-search conventions.

```
burial untrue           both words, anywhere, in any order
"burial untrue"         the words adjacent, in that order
burial -remix           has burial, does not have remix
burial OR zomby         either one (| works too)
p:jn4                   tracks in the JN4 playlist
p:"jack night"          quote a name with spaces (playlist: is the long form)
is:stream               keywords, listed below
bpm:120-130             numbers, listed below
p:jn4 burial -is:flac   mix freely — terms are ANDed, OR binds tighter
```

Two differences from a web search engine:

- Every term matches as a **substring**, not a whole word. These boxes filter as
  you type, so `buri` narrows before `burial` is finished.
- There is no ranking. A filter either keeps a row or drops it.

#### Keywords

`is:`, `has:` and `type:` are interchangeable spellings of one vocabulary.

| keyword | matches |
| --- | --- |
| `is:local` | rekordbox has a file path for it |
| `is:cloud` | Cloud Library Sync owns it — a real file, not necessarily downloaded here |
| `is:stream` | a SoundCloud / Spotify / Apple Music / Beatport link, with no file behind it |
| `is:present` / `is:missing` | whether that file is actually on this machine |
| `is:lossless` / `is:lossy` | what you have, when you have a file |
| `type:flac` etc. | `mp3` `m4a` `flac` `aiff` `wav` |
| `has:cues` | the track already has cue points |
| `is:locked` | the lock bit is set |

The three origins are mutually exclusive, so `-is:stream` selects tracks with a
file of any kind, here or in the cloud. Streaming rows carry no format keyword:
`is:lossy` means a lossy file is present locally.

`present` / `missing` is a separate axis, and only local rows carry it. A cloud
path is relative to a sync root this tool cannot locate, and a stream has no
file, so neither is tagged. A path belonging to another machine — `C:/…` from a
synced Windows library, or another user's `/Users/…` — counts as missing,
because this machine cannot open it. The check runs once per load and takes
under a millisecond for a few thousand tracks. A file deleted behind
rekordbox's back appears as missing after `R`.

Further examples:

```
p:"jn next" is:stream        everything in the next gig that is still a stream
p:"jn next" is:lossy         …and everything that is only a 128k rip
is:local is:missing          entries whose file moved or was deleted
```

`shop --match` takes the same query and shops for everything it selects:

```bash
rekord-ripper shop --match 'p:"jn next" is:stream'
```

It prints the selection before searching anything, and refuses to run past
`--match-max` (25 by default). Each track is a fan-out across every backend, so
25 tracks take about a minute and 2500 hit a rate limit.

#### Numbers

`bpm:` and `len:` (or `length:`) take a value, a comparison, or a span.

```
bpm:128                 128-something — 128.02 counts, 129.00 does not
bpm:128.5               more digits, a narrower band
bpm:>=128  bpm:<130     comparisons
bpm:120-130             a span, inclusive at both ends (120..130 too)
len:210  len:3:30  len:3m30s     the same duration, three ways
len:3m                  three-something minutes
len:>6m  len:3m-6m      comparisons and spans of duration
```

A bare number matches to the precision written. An analysed BPM is rarely
exactly 128.00, so `bpm:128` covers 128.00 to 128.99. A comparison or span
means exactly the number written: `len:>6m` is "longer than six minutes", not
"longer than six-something minutes".

A track with no BPM matches no `bpm:` term, so `-bpm:>100` keeps the unanalysed
ones. Excluding a property does not drop the rows that lack it.

#### Playlists

Playlist names match against the case-insensitive folder-qualified path, so
`p:"jack night"` also matches every playlist inside a folder of that name.
Smart playlists store their membership as a query rekordbox evaluates rather
than as rows, so they never match.

From the shell, an excluded term's leading `-` is claimed by the flag parser.
Put the query after a `--`:

```bash
rekord-ripper dump --limit 5 -- p:"jack night" is:stream -remix
```

An all-digit query is an exact track ID, not a search.

## Acquisition backends

Most libraries end up with tracks sourced from SoundCloud, where the audio is a
lossy transcode. The same tracks are often buyable in lossless on Bandcamp, and
tracks that were never for sale are often on Soulseek. These commands find them,
open the purchase page, fetch the file, and move your cue points and beat grid
onto it.

```bash
# Search every backend at once and compare what is on offer.
rekord-ripper shop "burial untrue"
rekord-ripper shop --track-id 12345678 --lossless-only

# Bulk: shop for several tracks in one run, grouped per track.
rekord-ripper shop --track-id 12345678 --track-id 87654321 --json

# Bulk by search: everything in the next gig that is still a SoundCloud rip.
rekord-ripper shop --match 'p:"jn next" is:stream'
rekord-ripper shop --match 'is:lossy bpm:170-176' --match-max 40

# Open the purchase page in your browser. Payment is never automated.
rekord-ripper buy "burial untrue"

# Download something free, or something you have bought, and queue the transfer.
rekord-ripper fetch --offer bandcamp:a:856850876 --src-track-id 12345678
rekord-ripper fetch https://soundcloud.com/artist/track --src-track-id 12345678

# Apply queued transfers. --import creates the rekordbox rows too, so nothing
# has to be dragged in first; the queue already knows each file's source track.
rekord-ripper pending --list
rekord-ripper pending --apply --import
rekord-ripper pending --apply          # if you dragged them in yourself

# When the check cannot run at all — a DRM stream, a source file on another
# machine — or when it rejected something you disagree with. Nothing then
# proves the two files are the same recording, so cues and the beat grid may
# land anywhere; master.db is still backed up and the entry is kept as
# UNVERIFIED.
rekord-ripper pending --apply --force

rekord-ripper pending --clear 12       # forget one entry
rekord-ripper pending --clear-all      # forget the lot; the files stay put
```

### In the TUI

The TUI has two screens.

The **transfer screen** is the src → dst view. `Space` picks destinations; the
source is always the highlighted row. `s` switches to the shop screen, landing
on the track you were on.

The **shop screen** is a track list beside an offer table. `s` searches the
highlighted track. Tapping it on several tracks searches them one after
another, with results accumulating into one grouped table; nothing is discarded
and nothing is searched twice. `Space` fills a basket and `S` searches all of
it, up to `search.bulk_max` (25) tracks a press — press it again for the next
batch. Each track carries a tag showing what its search found: a count, `·` for
nothing, `…` for still queued. `Enter` on an offer downloads it and queues an
analysis transfer against that offer's own source track, which after a batch of
searches is not necessarily the track under the list cursor. `Enter` on further
offers stacks them behind the running download rather than refusing: they run
one at a time, for the same reason searches do, and each finished file is paired
with the source track its own offer came from. The `D` column says which is
which — `↓` downloading, `…` queued, `✓` landed. `Esc` goes back.

Searches run on a background thread, so leaving the screen loses nothing and `s`
returns to it. They run sequentially rather than in parallel: each track is
already a fan-out across every backend, so running several at once would
multiply requests per backend and risk a rate limit.

Backends implement the `AcquisitionBackend` trait. Adding one means
implementing search, enrich, purchase and fetch. Bandcamp, SoundCloud and
Soulseek are included.

### SoundCloud

SoundCloud rips go through `yt-dlp`. Cookies are optional and make no
difference to an ordinary track: `hls_mp3` 128k, `hls_aac_96k` and
`hls_aac_160k` are the same signed in or not. Signing in adds:

- **`hls_aac_256k`**, 256kbps AAC flagged `Premium`, on tracks marked
  `quality: hq`. Requires Go+ and is absent from an anonymous manifest. One
  test track fetched 10.7MB anonymous and 17.1MB signed in.
- **The artist-enabled original**, the only lossless option here. It is rare,
  and its endpoint refuses anonymous requests.
- **Access** to private links, tracks that otherwise return a 30-second
  snippet, and softer rate limits.

Go+ subscription-only tracks are unavailable either way: a 30-second preview
anonymously, `This video is DRM protected` signed in.

```toml
[soundcloud]
cookies_from_browser = "firefox"     # or "chrome:Profile 1", "brave", "safari", …
```

`yt-dlp` recognises only `brave`, `chrome`, `chromium`, `edge`, `firefox`,
`opera`, `safari`, `vivaldi` and `whale`. First use prompts for keychain access
on macOS. Click **Always Allow**, or the run hangs on the dialog.

**A Chromium fork outside that list needs `cookies_file`, not a path
override.** Pointing the `chromium` reader at the fork's profile reads the right
cookie database with the wrong decryption key: `yt-dlp` derives the keychain
item from the browser name, so `chromium:` looks up `Chromium Safe Storage`
while Helium uses `Helium Storage Key` and Arc uses `Arc Safe Storage`. Cookie
names are unencrypted and survive, so the jar appears populated while every
value is blank, leaving the session anonymous. rekord-ripper treats this as
fatal.

#### Exporting a cookie jar

Also required when the logged-in browser is on another machine.

```toml
[soundcloud]
cookies_file = "~/soundcloud-cookies.txt"
```

The file must be in **Netscape format**: tab-separated, one cookie per line,
starting with `# Netscape HTTP Cookie File`. Use a cookies.txt browser
extension that exports locally, from a signed-in SoundCloud tab. Avoid
extensions that upload.

**`document.cookie` output is rejected.** Devtools' cookie panel and
`console.log(document.cookie)` produce `a=b; c=d`, which is the wrong format and
cannot see `HttpOnly` cookies, so it can omit the session without any error.
Auto-conversion was tried and dropped for that reason: it would work often
enough to be trusted, then fail invisibly.

The jar holds a live session token. Keep it `chmod 600` and out of the repo, and
re-export when the session rotates. To confirm a jar authenticated:

```bash
yt-dlp --cookies ~/soundcloud-cookies.txt -F <track-url> 2>&1 | grep -i "verif\|logging"
#   [soundcloud] Verifying login token...
#   [soundcloud] Logging in
```

No `Logging in` line means `oauth_token` did not survive the export.

#### When auth is broken

`yt-dlp` reports cookie problems as warnings on an otherwise successful run, so
the default outcome would be `backends` reporting an authenticated session while
every fetch returns a transcode. The following are therefore hard errors: both
cookie keys set, an unknown browser name, a `cookies_file` that is unreadable,
empty or a `document.cookie` dump, any cookie that fails to decrypt, and a
signed-in session that SoundCloud still refuses the original to. Running without
cookies on a track that has an original is noted on the offer.

Two consequences. **`--lossless-only` skips SoundCloud entirely until cookies
are configured**, because the original is the only lossless option and is
otherwise unreachable. And changing the cookie setting invalidates the results
of a previous `shop`, so search again rather than fetching against an old offer
table.

`extra_args` is appended after the cookie flags, so a hand-written
`--cookies-from-browser` there takes precedence.

### Soulseek

Soulseek offers are free and carry their real format: slskd reports the
extension, the bitrate, and whether a lossy encode is VBR. A FLAC from Soulseek
therefore competes with a Bandcamp purchase on the same row and can win. There
is nothing to buy, so `buy` does not apply to them. Files a peer has locked
behind their own sharing rules are never offered, because a fetch could not
deliver one.

```toml
[soulseek]
url = "https://slskd.example.com:5030"   # the slskd API
files_url = "https://slskd.example.com/files"
```

```toml
# credentials.toml, mode 600
[soulseek]
api_key = "..."            # slskd --generate-secret 32, role readwrite
files_user = "ripper"      # only if the files route is protected
files_password = "..."
```

`files_url` exists because slskd's API can list and delete files in its download
directory but cannot serve their contents; there is no endpoint for it. When
slskd runs on another machine, serve that directory over HTTP and point
`files_url` at it. One Caddy route next to the API is enough:

```
handle_path /files/* {
    root * /var/slskd/downloads
    basicauth { ripper <bcrypt hash> }
    file_server
}
```

**The route must serve the same directory slskd downloads into**, whatever
`directories.downloads` is set to in `slskd.yml`. If the two are out of step,
the failure is confusing: slskd reports the download as succeeded, because it
did succeed, and the file route then returns 404. rekord-ripper prints both
paths when that happens.

Leave `files_url` empty when the download directory is reachable as a path — a
local slskd, or a mounted share. The file is then moved rather than downloaded.

Each fetch stages into `rekord-ripper/<id>/` under slskd's download directory.
That staging directory is left in place by default, so if the download directory
is listed in slskd's `shares` the file continues to be shared. Set
`clean_up_remote = true` to delete it instead. Deletion also requires slskd's
`remote_file_management` to be enabled; without it the delete is refused and
skipped silently.

Set `fetch_folder_image = true` to also pull the `cover.jpg` / `folder.jpg` most
releases are shared with, and give it to the track. It is off by default because
it is a *second* transfer from the same peer, with its own place in their upload
queue. Three things keep it from slowing a download down:

- It runs only after the audio has already arrived.
- It is skipped entirely when the file has a cover of its own, which most FLACs
  on Soulseek do.
- It gets a short budget of its own (150s) rather than the download's, so the
  worst case is a bounded wait and no image.

The cover is embedded in the file where the container allows it, and saved
beside it as a hidden sidecar where it does not — WAV, which cannot carry a
picture. Either way `import` picks it up and puts it in rekordbox. Named covers
are preferred over an arbitrary image, the largest wins among equals, and
anything over 12 MB is treated as a booklet scan and left alone.

Timing behaviour:

- **A search blocks, and `search_limit` bounds it.** `search_window_secs` (8,
  minimum 5) is slskd's idle timeout and restarts on every response, so on a
  popular query the peer cap is what ends the search.
- **`fetch_timeout_secs` (1800) is when rekord-ripper stops waiting, not when
  the transfer stops.** The transfer is left running in slskd, and fetching the
  same offer again attaches to it rather than starting over, because the batch
  id is derived from the offer. This preserves a queue position that may have
  taken hours.

### What it does not do

- **Buy anything for you.** Bandcamp checkout is a card flow in their web UI
  with no API behind it. `buy` opens the right page; you pay.
- **Compare prices across currencies.** Prices come in each seller's own
  currency and there is no exchange-rate source here, so prices are shown with
  their ISO code and any "cheapest" line is per-currency.
- **Treat a SoundCloud rip as an upgrade.** Everything there is a transcode
  unless the artist enabled the original, which requires a signed-in session.
  `fetch` reports the format it actually got and says when it is a downgrade.
- **Vouch for a Soulseek file's quality.** A search result carries only a peer's
  filename and their claimed bitrate, and a `.flac` upscaled from a 128kbps MP3
  is indistinguishable from here. The fingerprint gate proves the file is the
  same recording, not that it is a better master.
- **Run slskd for you.** `backends` reports what is missing; managing a
  long-lived logged-in process is out of scope.

### The fingerprint gate

A transfer only runs when an audio fingerprint shows the two files are the same
recording **and** that they are time-aligned. The second condition matters
because cue points are copied as absolute timestamps and the beat grid is copied
as opaque ANLZ binary, so a same-but-shifted pair would place every cue wrongly
with no way to compensate. The gate fails closed on either axis.

The thresholds ship deliberately loose. Calibrate them against your own
library:

```bash
rekord-ripper fp path/to/soundcloud-rip.mp3 path/to/bandcamp.flac
```

That prints the per-segment scores, coverage, and the implied time offset. Run
it over pairs you know are the same track and pairs you know are not, then set
`score_max` and `coverage_min` in `config.toml` from the gap between them.

One fingerprint item is about 124ms, so shifts below roughly **62ms** are
invisible and a 50ms offset will be accepted. This is the resolution floor and
the accept message states it.

## Configuration

```bash
rekord-ripper config            # where it lives
rekord-ripper config --init     # write a starter file
```

Bandcamp downloads need the `identity` cookie from a logged-in browser session,
in `credentials.toml` next to `config.toml` (or `BANDCAMP_IDENTITY` in the
environment). Keep that file mode 600: it is a full-account credential, not a
read-only API key.

Soulseek needs an slskd API key in the same file, as `[soulseek] api_key` or
`api_key_file` (or `SLSKD_API_KEY`), plus `files_user` / `files_password` if the
files route is protected. Both are shown under "Acquisition backends" above.
Put slskd behind TLS if it is reachable from the internet: an API key in a
header over plain HTTP is a credential in the clear, and it never expires.

Rekordbox has no watch-folder feature, so by default importing a downloaded file
requires dragging in the download directory, once per batch rather than once per
file. Enabling row insertion removes that step: `pending --apply --import`
creates the rows itself and then runs the transfers.

## Creating rekordbox rows directly

`rekord-ripper import` writes the `djmdContent` row itself, so a downloaded file
appears in your collection without the drag. With `--src-track-id` it also runs
the fingerprint-gated transfer in the same command:

```bash
rekord-ripper pending --apply --import                     # the whole download queue
rekord-ripper import "new.flac"                            # dry-run: shows every value
rekord-ripper import "new.flac" --src-track-id 12345678 --apply
rekord-ripper import --undo 3052064790 --apply             # changed your mind
```

It reads the file's embedded tags and reuses existing artist/album/genre rows
rather than duplicating them. Three gates stand in front of it: the config key
`insert_content_rows` (off by default), `--apply`, and a confirmation showing
the full row. The same running-rekordbox refusal and automatic backup as `cp`
also apply.

Undo is a tombstone (`rb_local_deleted = 1` with a USN bump), not a delete,
because on a cloud-synced library a hard delete would leave your other devices
holding a row for a file they do not have. Every insert also writes an
`<backup>.inserted.json` note next to the backup so it can be undone later.

`cp` already inserts rows into five tables and clears `rb_local_synced`, so it
has always written rows your cloud agent pushes. What is new here is that a
track row points at a file, so under Cloud Library Sync rekordbox may upload
that audio and rewrite `FolderPath`.

`REKORDBOX_DIR` overrides the rekordbox directory, which is how the write paths
are tested against a copy of `master.db` rather than the real thing.

## Cover art

Rekordbox keeps art as a file under `share/PIONEER/Artwork/` and stores the path
on the track row (`djmdContent.ImagePath`), keyed on the row's own UUID. There is
no artwork table — `imageFile` is cloud-sync bookkeeping and stays empty. Three
copies are written per track: the source capped to 800px, plus a 240px and an
80px thumbnail, which is what rekordbox's own cache holds.

A download gets its cover automatically. `import` then takes it out of the file,
so nothing extra is needed. `rekord-ripper artwork` backfills rows that predate
this:

```bash
rekord-ripper artwork                          # dry-run: what the files can supply
rekord-ripper artwork --apply
rekord-ripper artwork --from-sources --limit 5 # also search the backends, on 5 tracks
rekord-ripper artwork --from-sources --apply --match 'p:"jn next"'
```

Without `--from-sources` it only uses what is already on disk: a cover embedded
in the file, or a sidecar a download left beside it. That is free and certain.

`--from-sources` handles the rest by searching the enabled backends — one search
per track — and taking the art off the best match. Bandcamp and SoundCloud both
return an artwork URL in their search results, so no extra request is needed;
Soulseek cannot help here, because its art is a queued transfer per track (use
`fetch_folder_image` at download time instead).

This is the one path where art does **not** come from the file it belongs to, so
a bad text match means a wrong cover. Matches are scored 0-100 on title, artist
and duration, and split in two:

- **80 or above** applies in bulk. That needs an exact title *and* the artist or
  the duration to agree. Note what it does *not* rule out: a re-upload with the
  same title and length under a different uploader also scores 80. Pass
  `--auto-score 90` to insist the artist agrees too.
- **Below that** is shown to you one at a time, with the cover drawn in the
  terminal where it supports inline images (Kitty protocol for Ghostty, kitty,
  WezTerm and Konsole; `OSC 1337` for iTerm2; otherwise the URL is printed).
  Answer `y`, `n`, `a` for all remaining, or `q` to stop. `--min-score` sets the
  floor worth showing at all.
- `-y` skips the review entirely rather than accepting it blind.

Everything is a dry-run until `--apply`, and the review runs before any write,
so quitting half way leaves the database untouched.

The cover is embedded in the file as well as given to rekordbox, which changes
the file's size — so `FileSize` on the row is corrected in the same transaction,
and tracks with a queued analysis transfer are skipped, since rewriting the file
would expire the pairing. A FLAC keeps every other metadata block, its seek
table included. Versions before 1.2.0 embedded through ffmpeg, which drops the
seek table; `check` (below) puts it back.

**A rewritten file has to come off the USB stick first.** Rekordbox exports a
changed file as a second copy (`name-1.m4a`) and keeps the first, so the player
lists the track twice and the old copy still loads. A `--from-sources` run lists
the tracks whose files it rewrote. In rekordbox, delete every copy of those from
the stick, then export again. Reformatting the stick works too. One real stick
held 156 such copies from a single backfill.

## Compatibility levels

Rekordbox plays files the players you export to cannot: no FLAC or ALAC on a
CDJ-2000NXS, nothing above 48 kHz on most of them, no 32-bit or float PCM on
any. `compat` converts what a level cannot play and points the existing track
row at the new file. The track ID stays the same, so cues, beat grid,
playlists and play history carry over, and the next USB export copies the
playable file.

```bash
rekord-ripper compat --levels                         # what each level allows
rekord-ripper compat --level legacy                   # dry-run over every local track
rekord-ripper compat --level legacy --match 'p:"jn next"' --apply
rekord-ripper compat --level legacy --allow-lossy --apply
rekord-ripper compat --undo 12345678 --apply          # back to the original file
```

| level | plays | players |
| --- | --- | --- |
| `legacy` | MP3, AAC, WAV, AIFF · 44.1/48 kHz · 16/24-bit | CDJ-2000, CDJ-2000NXS, CDJ-900, CDJ-900NXS, CDJ-850, CDJ-350, XDJ-1000, XDJ-700, XDJ-RX, XDJ-RX2 |
| `flac48` | adds FLAC · 44.1/48 kHz · 16/24-bit | XDJ-1000MK2, XDJ-RX3, XDJ-XZ |
| `nxs2` | adds FLAC, ALAC · 44.1–96 kHz · 16/24-bit | CDJ-2000NXS2, CDJ-3000, OPUS-QUAD |

Each row comes from the playable-formats table in the players' operating
instructions, e.g. the
[CDJ-2000NXS](https://downloads.support.alphatheta.com/manuals/dj-players/CDJ-2000NXS/CDJ-2000NXS_DRI1052_manual.pdf),
[XDJ-1000MK2](https://downloads.support.alphatheta.com/manuals/XDJ_1000MK2_DRI1396B_manual.pdf),
[XDJ-XZ](https://downloads.support.alphatheta.com/manuals/XDJ_XZ_DRI1625B_manual.pdf)
and [CDJ-3000](https://downloads.support.alphatheta.com/manuals/dj-players/CDJ-3000/CDJ-3000_DRI1586A_manual.pdf)
manuals. Every one lists MP3 and AAC at 44.1/48 kHz only, so lossy audio above
48 kHz does not count as playable at any level. The XDJ-1000MK2 also plays ALAC;
the XDJ-RX3 and XDJ-XZ do not, so `flac48` leaves it out. The XDJ-XZ gained FLAC
in firmware 1.10.

Your own levels go in `config.toml`. A level with a built-in's name replaces it:

```toml
[compat]
default_level = "legacy"

[compat.levels.my-booth]
codecs = ["mp3", "aac", "aiff", "wav"]
sample_rates = [44100, 48000]
bit_depths = [16, 24]
```

What a track becomes:

- **Lossless** (FLAC, ALAC, WAV, AIFF) becomes **AIFF**, which carries tags
  and cover art where WAV cannot. Rate and depth are kept where the level
  allows them. Otherwise they come down within the same family (88.2 → 44.1,
  96 → 48 kHz; 32-bit or float → 24), with dither only when reducing to 16-bit.
- **Lossy** (MP3, AAC) that the level cannot play is skipped unless you pass
  `--allow-lossy`. With it, the track is re-encoded as 320k CBR MP3. That is a
  second lossy generation, so `shop` for a lossless copy where you can.
- Streams, missing files, and files with a queued analysis transfer are left
  alone. Cloud Library Sync rows are too, unless you pass `--include-cloud`
  (below).

The converted file is written beside the original as `<name>.aiff`. If that
name is taken, it becomes `<name> [<level>].aiff`. Before the row moves, every
conversion is decoded and cross-correlated against its source. It must come
out in the planned format, the same length to within 50 ms, and shifted by no
more than 1 ms. The fingerprint gate could not be used for this: its 62 ms
floor is wider than an MP3 encoder delay. Each row is committed as its file
lands, so an interrupted run leaves every finished track consistent. The run
stops if rekordbox is opened while it is going.

Originals are never touched, so undo is cheap. Each conversion writes a note
beside the backup, and `--undo` points the row back at the original. It refuses
if the row has changed since or the original is gone. The converted file stays
where it is for you to delete.

Two limits:

- The row's ANLZ files still name the original path inside them, as after a
  `cp`. The grid and waveform remain correct because the audio timeline is
  unchanged.
- An MP3 made from a lossless track has cues that carry no MPEG frame offsets,
  and the alignment check sees the file the way ffmpeg decodes it. Load one
  `--allow-lossy` result in rekordbox and on a player before converting a crate
  that way.

Restart rekordbox after a run. The run lists the tracks it converted. On any
USB stick that already holds them, delete every copy of those tracks in
rekordbox (or reformat the stick), then export again. An export on top keeps
the old, unplayable copy beside the new one, the same way as after an artwork
backfill.

### Cloud Library Sync rows

A cloud row's `FolderPath` (`/contents_…/artist/album/file`) is relative to the
sync folder. Rekordbox records only the Dropbox folder, as `DropboxSharingPath`
in `rekordbox3.settings`, and the files sit in its `rekordbox` subfolder.
`--include-cloud` resolves rows through it:

```bash
rekord-ripper compat --level nxs2 --include-cloud                 # dry-run
rekord-ripper compat --level nxs2 --include-cloud --match '…' --limit 1 --apply
```

The converted file is written beside the original inside the sync folder, so
Dropbox uploads it. The row is repointed the same way as a local one, and the
edit syncs to every device on the library. Each row's `rb_file_id`, which sync
keeps for the file it uploaded, is cleared because it names the old file. 347
cloud rows in a real library already go without one. Undo puts it back.

Nothing documents how rekordbox's sync treats a row whose file changes under
it. Convert one track first and check it on this machine, on a second synced
device, and on a player before running a whole crate. Files Dropbox holds only
as online placeholders are skipped, since reading one would download it.
`repair` resolves cloud rows the same way. It only edits `FileType`, so it
writes nothing into Dropbox.

## Checking files before a gig

`check` reads every file in full and reports what will go wrong on a player:

```bash
rekord-ripper check --level nxs2 --include-cloud        # the library
rekord-ripper check --level nxs2 --path /Volumes/STICK/Contents
rekord-ripper check --include-cloud --apply             # add missing seek tables
```

- **Damaged audio.** FLAC frames are checked against their CRCs, which names
  the second the damage starts. Every other format is decoded by ffmpeg. A
  player that reaches damage drops into an emergency loop or stops, and nothing
  short of a clean copy fixes it, so `shop --track-id` for one.
- **Unplayable at the level**, the same test `compat` applies. `compat`
  converts them.
- **FLACs with no seek table.** Without one the player scans the file to reach
  a cue. ffmpeg writes none, so remuxed or tagged files often lack one.
  `--apply` adds a point every ten seconds, the same table
  `metaflac --add-seekpoint=10s` writes. The table goes into the file's padding,
  so its size, timestamp and every audio offset stay the same, and the
  rekordbox row and any exported copy still describe it. A file with too little
  padding is reported and left alone.

An exported stick holds byte-identical copies, so `--path <stick>/Contents
--apply` fixes them there too and no export is needed. Files starting with `.`,
such as macOS's `._` resource forks, are skipped.

## Playlists that still hold the stream

Downloading a track and transferring its analysis leaves the streaming row in
every playlist it was in. A USB stick cannot play a stream, so the export
carries a copy that fails on the player. `relink` swaps those playlist entries
to the downloaded copy:

```bash
rekord-ripper relink                  # dry-run: every pair, and where it swaps
rekord-ripper relink --apply
rekord-ripper relink --undo --apply   # reverses the most recent relink
```

A stream and a file count as the same track only when the title and artist are
identical (brackets included, so a remix never matches its original) and their
lengths are within two seconds. The file must be on this machine or synced
through the cloud. A stream with two such files is skipped and reported. Each
entry is edited in place, so its position in the playlist is kept. A playlist
that already holds the file keeps its stream entry for you to remove, because
swapping it would list the track twice.
