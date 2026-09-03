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

The Soulseek backend needs a reachable [slskd](https://github.com/slskd/slskd).
It is optional — until you configure one, Soulseek reports itself as unconfigured
and the other backends carry on. Running it somewhere always-on is the point: a
Soulseek queue position can take hours, and a laptop that sleeps loses it.

Run `rekord-ripper backends` at any time to see what is configured and what is
missing.

## Transferring analysis

The basic version is `rekord-ripper tui`, which just drops you into the TUI. You
can also manually copy with `rekord-ripper cp`, auto copy with
`rekord-ripper auto`, and dump the database with `rekord-ripper dump`. See the
help pages for more information.

### Searching your library

Every `/` box in the TUI and `rekord-ripper dump` take the same query, and it
works the way a web search box does.

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

Two departures from Google. Every term matches as a **substring**, not a whole
word, because these boxes filter as you type and `buri` has to narrow before
`burial` is finished. And there is no ranking — a filter keeps a row or drops
it.

#### Keywords

`is:`, `has:` and `type:` are three spellings of one vocabulary, so whichever
you reach for first works.

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

The three origins are exclusive, so `-is:stream` is how you ask for "has a file
at all, here or in the cloud". Streaming rows carry no format keyword on
purpose: `is:lossy` means *you have a lossy file*, which is the question worth
asking before shopping.

`present` / `missing` is a separate axis, and only local rows get one. A cloud
path is relative to a sync root this tool cannot locate, and a stream has no
file, so both are left untagged rather than guessed at. A path belonging to
another machine — `C:/…` from a synced Windows library, or somebody else's
`/Users/…` — counts as missing, because this machine genuinely cannot open it.
The check runs once per load and costs under a millisecond for a few thousand
tracks; a file deleted behind rekordbox's back shows up after `R`.

The query the shop screen was waiting for:

```
p:"jn next" is:stream        everything in the next gig that is still a stream
p:"jn next" is:lossy         …and everything that is only a 128k rip
is:local is:missing          entries whose file moved or was deleted
```

`shop --match` takes the same query and shops for everything it selects, so the
first of those becomes one command:

```bash
rekord-ripper shop --match 'p:"jn next" is:stream'
```

It prints what it selected before searching anything, and refuses outright past
`--match-max` (25 by default) — each track is a fan-out across every backend, so
the difference between 25 and 2500 is the difference between a minute and a rate
limit.

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

A **bare** number covers the precision you typed, because an analysed BPM is
never exactly 128.00 and a filter that demanded it would be useless. A
**comparison or span** means exactly the number written — `len:>6m` is "longer
than six minutes", not "longer than six-something minutes".

A track with no BPM at all matches no `bpm:` term, so `-bpm:>100` keeps the
unanalysed ones: excluding a property should not also drop the rows that lack
it.

#### Playlists

Playlist names match against the case-insensitive folder-qualified path, so
`p:"jack night"` also catches every playlist inside a folder of that name.
Smart playlists store their membership as a query rekordbox evaluates rather
than as rows, so they never match.

From the shell, an excluded term's leading `-` is claimed by the flag parser, so
put the query after a `--`:

```bash
rekord-ripper dump --limit 5 -- p:"jack night" is:stream -remix
```

An all-digit query is still an exact track ID, not a search.

## Acquisition backends

Most of a library ends up on SoundCloud, where the audio is a lossy transcode.
The same tracks are usually buyable in lossless on Bandcamp, and the ones that
were never for sale anywhere are usually on Soulseek. These commands find them,
help you buy them, fetch the file, and move your cue points and beat grid onto
it.

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
of implementing search, enrich, purchase and fetch. Bandcamp, SoundCloud and
Soulseek ship with it.

### SoundCloud

SoundCloud rips go through `yt-dlp`. Cookies are optional and do not improve an
ordinary track — `hls_mp3` 128k, `hls_aac_96k`, `hls_aac_160k` are the same
signed in or not. What signing in adds:

- **`hls_aac_256k`**, 256kbps AAC flagged `Premium`, on tracks marked
  `quality: hq`. Needs Go+, absent from an anonymous manifest. One test track
  fetched 10.7MB anonymous, 17.1MB signed in — the main reason to bother.
- **The artist-enabled original**, the only lossless thing here. Rare, and its
  endpoint refuses anonymous requests.
- **Access**: private links, tracks that otherwise return a 30-second snippet,
  and much softer rate limits.

Go+ subscription-only tracks stay out of reach either way — a 30-second preview
anonymously, `This video is DRM protected` signed in.

```toml
[soundcloud]
cookies_from_browser = "firefox"     # or "chrome:Profile 1", "brave", "safari", …
```

`yt-dlp` knows only `brave`, `chrome`, `chromium`, `edge`, `firefox`, `opera`,
`safari`, `vivaldi` and `whale`. First use prompts for keychain access on macOS
— click **Always Allow** or the run hangs on the dialog.

**A Chromium fork outside that list needs `cookies_file`, not a path override.**
Pointing the `chromium` reader at the fork's profile reads the right cookie
database with the wrong decryption key: `yt-dlp` derives the keychain item from
the browser name, so `chromium:` looks up `Chromium Safe Storage` while Helium
keeps `Helium Storage Key` and Arc keeps `Arc Safe Storage`. Cookie names are
unencrypted and survive, so the jar looks populated while every value is blank —
silently anonymous. rekord-ripper treats that as fatal rather than carrying on.

#### Exporting a cookie jar

Also what you want when the logged-in browser is on another machine.

```toml
[soundcloud]
cookies_file = "~/soundcloud-cookies.txt"
```

It must be **Netscape format** — tab-separated, one cookie per line, starting
`# Netscape HTTP Cookie File`. Use a cookies.txt browser extension that exports
locally (avoid anything that uploads), from a signed-in SoundCloud tab.

**`document.cookie` is rejected.** Devtools' cookie panel and
`console.log(document.cookie)` give you `a=b; c=d`, which is both the wrong
format and unable to see `HttpOnly` cookies — so it can silently omit the
session. Auto-converting it was tried and dropped for that reason: it would work
often enough to be trusted, then fail invisibly.

The jar holds a live session token. Keep it `chmod 600` and out of the repo, and
re-export when the session rotates. To confirm one authenticated:

```bash
yt-dlp --cookies ~/soundcloud-cookies.txt -F <track-url> 2>&1 | grep -i "verif\|logging"
#   [soundcloud] Verifying login token...
#   [soundcloud] Logging in
```

No `Logging in` means `oauth_token` did not survive the export.

#### When auth is broken

`yt-dlp` reports cookie problems as *warnings* on an otherwise successful run, so
the default outcome would be `backends` claiming an authenticated session while
every fetch quietly returns a transcode. Instead, all of these are hard errors:
both cookie keys set, an unknown browser name, a `cookies_file` that is
unreadable, empty or a `document.cookie` dump, any cookie that fails to decrypt,
and a signed-in session SoundCloud still refuses the original to. Running
*without* cookies on a track that has an original is noted on the offer, so you
can see where cookies would pay off.

Two consequences: **`--lossless-only` skips SoundCloud entirely until cookies are
configured**, since the original is the only lossless option and is otherwise
unreachable; and changing the cookie setting invalidates what a previous `shop`
told you, so search again rather than fetching against an old offer table.

`extra_args` is appended after the cookie flags, so a hand-written
`--cookies-from-browser` there wins.

### Soulseek

Soulseek offers are free and carry their real format — slskd reports the
extension, the bitrate, and whether a lossy encode is VBR — so a FLAC from
Soulseek competes with a Bandcamp purchase on the same row and can win. There is
nothing to buy, so `buy` has nothing to do with them. Files a peer has locked
behind their own sharing rules are never offered, since a fetch could not
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

**`files_url` is the part that needs explaining.** slskd's API can list and
delete files in its download directory but cannot hand over their bytes — there
is no endpoint for it. So when slskd is on another machine, point `files_url` at
that directory served over HTTP and rekord-ripper fetches from there. One Caddy
route next to the API does it:

```
handle_path /files/* {
    root * /var/slskd/downloads
    basicauth { ripper <bcrypt hash> }
    file_server
}
```

**The route has to serve the same directory slskd downloads into** — whatever
`directories.downloads` is set to in `slskd.yml`. Getting those two out of step
is the easy mistake, and it fails confusingly: slskd reports the download as
succeeded because it *did* succeed, and the file route then 404s. rekord-ripper
names both paths when that happens.

Leave `files_url` empty when the download directory is reachable as a path — a
local slskd, or a mounted share — and the file is moved rather than downloaded.

Each fetch stages into `rekord-ripper/<id>/` under slskd's download directory.
That staging directory is **left in place** by default, so if you have the
download directory in slskd's `shares` the file keeps being shared — which is
the norm on Soulseek, and the thing that gets your own searches answered. Set
`clean_up_remote = true` to delete it instead (which also needs slskd's
`remote_file_management` enabled; without it the delete is refused and skipped
quietly).

Two behaviours worth knowing:

- **A search blocks, and `search_limit` is what bounds it.**
  `search_window_secs` (8, minimum 5) is slskd's *idle* timeout — it restarts on
  every response — so on a popular query the peer cap is what actually stops it.
- **`fetch_timeout_secs` (1800) is when rekord-ripper stops waiting, not when the
  transfer stops.** It is left running in slskd, and fetching the same offer
  again attaches to it rather than starting over, because the batch id is derived
  from the offer. A queue position you have already waited hours for is not worth
  throwing away.

### Things it deliberately does not do

- **Buy anything for you.** Bandcamp checkout is a card flow in their web UI with
  no API behind it. `buy` gets you to the right page; you pay.
- **Compare prices across currencies.** Prices come in each seller's own
  currency and there is no exchange-rate source here, so prices are always shown
  with their ISO code and any "cheapest" line is per-currency.
- **Pretend a SoundCloud rip is an upgrade.** Everything there is a transcode
  unless the artist enabled the original, which needs a signed-in session.
  `fetch` reports the format it actually got and says when it is a downgrade.
- **Vouch for a Soulseek file's quality.** All a search result carries is a peer's
  filename and their claimed bitrate, and a `.flac` upscaled from a 128kbps MP3
  looks identical to a real one from here. The fingerprint gate proves it is the
  same recording, not that it is a better master.
- **Run slskd for you.** `backends` says what is missing; managing a long-lived
  logged-in process is not this tool's job.

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

Soulseek needs an slskd API key in the same file, as `[soulseek] api_key` or
`api_key_file` (or `SLSKD_API_KEY`), plus `files_user`/`files_password` if the
files route is protected. Both are shown under "Acquisition backends" above. Put
slskd behind TLS if it is reachable from the internet — an API key in a header
over plain HTTP is a credential in the clear, and it never expires.

Rekordbox has no watch-folder feature, so out of the box importing a downloaded
file is a manual drag of the download directory — one drag per batch, not per
file. Turning on row insertion below removes even that: `pending --apply --import`
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
