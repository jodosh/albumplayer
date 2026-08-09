# AlbumPlayer

A music player where the **album is the unit of listening**, not the track.

Point it at a directory of music, and it browses, queues, shuffles, and counts
plays by album. Shuffle reorders albums and leaves their track order intact.

## Status

**Phases 1 and 2 complete** — scanning, storage, the play-history log, and
gapless album playback, all driven from a CLI. The desktop UI is not built yet.

| Phase | Contents | State |
|---|---|---|
| 1 | `albumplayer-core` + `albumplayer-cli`: scan, album identity, play log | done |
| 2 | `albumplayer-engine`: gapless album queue over GStreamer | done |
| 2b | `albumplayer-enrich`: measured loudness + fetched cover art | done |
| 3a | `albumplayer-server`: HTTP API, auth, streaming, Docker image | done |
| 3b | Svelte web UI, served by the server | done |
| 3c | Tauri v2 desktop shell with gapless GStreamer playback | done |
| 4 | Android app: Kotlin, Compose and Media3 | browse and play |
| 4b | Android Auto: one button, no browsing | done |
| 5 | Exposure: TLS and remote access | not started |

## Try it

```sh
cargo build --release
./target/release/albumplayer scan ~/Music
./target/release/albumplayer albums --sort plays
./target/release/albumplayer doctor
```

`doctor` is the one to run first on a real library. It reports tagging problems
specifically in terms of how they will break album-order playback.

### Commands

| Command | Purpose |
|---|---|
| `scan [DIRS...]` | Add roots and scan; omit args to rescan known roots. `--force` re-reads every tag |
| `albums` | List albums (`--sort artist\|title\|year\|plays\|added\|last`) |
| `artists` | Artists ranked by listening |
| `album <ID>` | Tracklist, grouped by disc |
| `stats` | Library totals |
| `doctor` | Tagging problems that affect album playback (9 categories) |
| `log <ALBUM_ID>` | Record a listen by hand (stands in for the engine) |
| `play` | Play albums (`--all`, `--shuffle`, `--repeat album\|queue`, `--sink`) |
| `replaygain` | Measure album loudness with ffmpeg (`--jobs`, `--force`, `--limit`) |
| `artwork` | Fetch missing covers from the Cover Art Archive (`--force`, `--limit`) |
| `top` | Most-played albums and artists (`--days N` to window it) |

The database defaults to `$XDG_DATA_HOME/albumplayer/library.db`; override with
`--db`.

## Design decisions

### Album identity is resolved in two stages

Getting this right is the hard part of an album-first player.

**Grouping** buckets files by `(album directory, normalized album tag)`. The
directory is the strongest signal for a library laid out one folder per album,
and it handles compilations correctly: everything in the folder groups together
regardless of differing track artists.

Cases that fragment naive scanners, each found in a real 9,400-file library and
handled explicitly:

- **Multi-disc folders** — a parent named `CD1`, `Disc 2`, `disk_03` collapses
  into the grandparent, so a double album is one album with two discs.
- **Missing disc tags** — plenty of rips split a release across `CD1`/`CD2`
  folders and never write a `disc` tag, leaving every track on "disc 1" with
  colliding track numbers. The folder name supplies the disc number instead.
- **Disc markers in the album title** — `Hullabaloo CD1` / `Hullabaloo CD2` and
  `Forty Licks (Disc One)` / `Forty Licks ( Disc 2 )` are one release each. The
  marker is stripped for grouping and used as a further disc-number fallback.
- **Discs in named sibling folders** — Mellon Collie's "Dawn to Dusk" and
  "Twilight to Starlight" share an album tag but not a directory. Drafts that
  resolve to the same identity are merged afterwards, and folder order supplies
  the disc numbers the tags never had.
- **Compilations** — grouping never keys on the track artist. Album artist is
  resolved *after* grouping, preferring the `albumartist` tag, then the
  compilation flag, then a unanimous track artist, and only then falling back to
  "Various Artists".
- **Wholly untagged files** — a library laid out `<root>/<artist>/<album>` gives
  the artist away in the path, so the folder is used rather than surrendering to
  "Unknown Artist". Albums sitting directly in a root are exempt, since the
  root's own name says nothing about who made the record.
- **ID3v1 truncation** — that format caps text at 30 bytes, so a title of
  exactly that length is only a prefix. Two unrelated releases can share one
  ("The End Is The Beginning Is Th" for both a single and its remix EP), so such
  titles are scoped to their directory and never merged across folders. Within
  one folder the opposite applies: a truncated title that is a strict prefix of
  a longer one belongs to that album, and a truncated *artist* credit is the
  same artist — otherwise a single ID3v1-only track turns its album into a
  one-track orphan and a bogus compilation.
- **Album tags that are really credits** — classical rips often put a performer
  ("NCO, Nicholas Ward") in the album field across a whole box set of separate
  works. Above four sibling folders sharing one tag, the tag is treated as a
  credit rather than a title, and each folder becomes its own album named after
  itself, so a single work can be played on its own.

**Identity** then derives the database key from the resolved album, preferring a
MusicBrainz release ID, then album artist + title + year, and only then the
directory path. Renaming a well-tagged folder therefore keeps its play history.

Merging is the one heuristic that can fuse two genuinely different releases, so
`doctor` always reports what it merged under `merged-from-several-folders`.

### Album-first is enforced by the queue, not by the UI

`albumplayer-engine` splits into a pure [queue model](crates/engine/src/queue.rs)
and a [GStreamer driver](crates/engine/src/player.rs). All the album-first
behaviour lives in the model, so it is testable without an audio device:

- **Shuffle reorders albums and never their tracks.** Each album still plays as
  one contiguous run, in sequence. The seed comes from the clock, so shuffle
  differs between runs, and shuffling before playback starts can land on any
  album rather than always the first.
- **Next-track and next-album are separate controls**, because in an album-first
  player they are different intentions. `Repeat::Album` loops a record, but a
  deliberate next-track still escapes it.
- **Album ReplayGain**, capped by album peak so positive gain cannot clip.

### Gapless playback

`playbin3` emits `about-to-finish` before the current file runs out; the next
URI is set from inside that callback so the files splice with no silence. The
switch is confirmed later by `stream-start` on the bus, and *that* is when the
queue cursor advances and the finished track is written to the play log.

The engine thread owns the pipeline. Only the queue is shared, because the
callback runs on a GStreamer streaming thread. The rule that keeps it from
deadlocking: never hold the queue lock across a pipeline state change —
`set_state` can block on a streaming thread that is itself waiting for the lock.

### Enrichment fills in what the files lack

A ripped library often carries neither loudness data nor cover art, and both
matter here — an album-first player that jumps in volume between records has
failed at its one job, and an album grid needs something to show.

`albumplayer-enrich` supplies both, under two rules:

- **Nothing is written into your music.** Measurements go in the database and
  covers into the cache directory. The audio files are never touched.
- **Existing metadata wins.** A tagged ReplayGain value is preferred over a
  measured one, and a `cover.jpg` in the album folder beats a fetched image.

**Loudness** is measured by ffmpeg's `ebur128` filter over the album's tracks
concatenated into one stream, because ReplayGain album gain is defined as the
loudness of the whole record played end to end — not the mean of its tracks.
Gain is `-18 LUFS − measured`, clamped, with true peak recorded so positive gain
cannot clip. Results are written as each album finishes rather than batched, so
an interrupted run keeps everything it had already measured.

**Covers** are resolved through MusicBrainz and downloaded from the Cover Art
Archive. MusicBrainz permits one request per second and requires an identifying
`User-Agent`, so lookups are serialized and paced; there is no parallelism to be
had. Albums that come back empty are marked as checked so later runs skip them.

### The web UI

Svelte 5 and Vite, built to a ~60 kB bundle that the server serves itself. The
same bundle will be wrapped by the desktop shell, so there is one interface to
maintain rather than two.

It enforces the same album-first rules as the Rust engine: the queue holds
albums, shuffle reorders albums and never their tracks, and next-track and
next-album are separate controls in the transport bar.

Two details worth knowing:

- **Covers fall back to generated tiles.** A quarter of this library has no
  artwork anywhere, so the placeholder is a real design — a colour derived from
  the artist and title, with the album's initials — rather than a grey square.
- **Browser playback is only near-gapless.** Two `<audio>` elements alternate so
  the next track is buffered before the current one ends, which is as close as a
  browser gets without Media Source Extensions. The desktop shell will route
  audio through GStreamer and be genuinely gapless. Album ReplayGain is applied
  either way, though an `<audio>` element cannot amplify above unity, so
  positive gain only avoids attenuation.

```sh
cd ui && npm install && npm run build   # server picks up ui/dist
npm run dev                             # or Vite's dev server against a running API
```

### The desktop shell

`albumplayer-desktop` wraps the very same Svelte bundle the server serves, and
replaces the browser's audio with the GStreamer engine. That is the only reason
it exists: two `<audio>` elements can approximate gapless playback, but an album
whose tracks segue needs the real thing.

The split:

- **Rust** owns the queue and the pipeline, streaming from the server over HTTP.
  `playbin3` plays an `https://` URI as readily as a local file, so gapless
  survives the music living in a homelab.
- **The UI** owns the library, login and play history, talking to the server
  exactly as it does in a browser. Playback events come back over Tauri's event
  channel so it can log what was heard.

History stays server-side on purpose: one listening record across the desktop, a
browser, and eventually a phone.

The shell needs a server address — inside Tauri `window.location.origin` is
`tauri://localhost`, which is not a server — so the login form requires one.

Because the shell loads from `tauri://localhost`, **every call it makes is
cross-origin**, and the server sends CORS headers accordingly. Only the browser
UI, served by the same process, is same-origin; without CORS the shell's
preflight is refused and the UI reports a bare "Load failed". Any origin is
allowed on purpose: authentication is a bearer token a browser never attaches by
itself, no cookies are involved, and credentials are explicitly disallowed,
which is what makes a wildcard origin safe here rather than reckless.

### Installing it

On Arch:

```sh
cd packaging && makepkg -si
```

That clones the tagged release, builds the UI and the shell, and installs the
binary, icons and desktop entry. It appears in the launcher as **AlbumPlayer**.

On Debian or Ubuntu, build a `.deb`:

```sh
cd ui && npm install && npm run build
cargo install tauri-cli --version "^2" --locked
cd crates/desktop && cargo tauri bundle
sudo dpkg -i ../../target/release/bundle/deb/AlbumPlayer_*_amd64.deb
```

The packages depend on the GStreamer plugin sets explicitly. GStreamer loads
its decoders at runtime, so a package that omits them installs perfectly well
and then cannot play a note.

### Building it by hand

The shell embeds the built UI, so the UI has to exist first. It is therefore
left out of the workspace's default members: a fresh clone can run `cargo build`
and `cargo test` without Node installed, and the shell is built explicitly.

```sh
cd ui && npm install && npm run build
cargo build --release -p albumplayer-desktop
```

WebKitGTK fails to allocate its render buffers on a good many Linux systems and
the window comes up blank, logging only `Failed to create GBM buffer`. The app
disables that renderer itself, since an installed application should not require
knowing an environment variable to show anything. An explicit
`WEBKIT_DISABLE_DMABUF_RENDERER` is respected, and `ALBUMPLAYER_ENABLE_DMABUF=1`
opts back into the faster path where the drivers are fine.

### The Android app

Kotlin, Compose and `androidx.media3`. Deliberately *not* the Svelte interface
in a wrapper: on a phone the dominant requirements are background playback,
lock-screen and Bluetooth controls, and battery, and a webview loses at all
three. Media3 provides them, along with gapless playback and Android Auto.

The album-first rules survive the port, and one of them takes explicit work:
**ExoPlayer's own shuffle reorders tracks**, which is exactly what this player
exists to avoid. It stays switched off, and shuffling instead permutes whole
albums before flattening them into the playlist, so each record still plays
start to finish. Next-track and next-album remain separate controls, with
album boundaries carried in each queue entry's extras.

The phone is a client like any other: the server owns the library and the play
history, so a record heard on the sofa shows up in the desktop app's counts.

```sh
cd android && ./gradlew :app:assembleDebug
```

It needs a JDK 17-21 (the Android Gradle Plugin does not accept newer ones) and
an Android SDK. Cleartext HTTP is permitted because a homelab server is
typically plain HTTP on a private address; once it is exposed it should be
behind TLS and this permission is never exercised.

### Android Auto

The car gets one thing to press — *Play a random album* — plus skip-track and
skip-album. There is deliberately no way to choose a record from the car.

That is a safety decision rather than a limitation of the platform. Picking an
album from a library of several hundred means reading a list at speed, and the
interesting decision — what to listen to — belongs at a standstill. Skipping is
enough to escape a record that turns out to be wrong for the drive.

Two consequences worth knowing:

- The service is a `MediaLibraryService`, not merely a `MediaSessionService`,
  because the car browses it directly and **the phone's Activity never runs
  while driving**. Anything the car can reach has to be loadable from the
  service itself, so it holds its own repository built from stored settings.
- Skipping a whole album is a custom session command with its own button in the
  car's layout; skipping a track is the standard transport control the car
  draws for itself.

Instrumented tests connect a `MediaBrowser` — exactly what Android Auto does —
and assert the browse tree stays one entry deep. If a future change starts
exposing the album list to the car, they fail.

```sh
cd android && ANDROID_SERIAL=<device> ./gradlew :app:connectedDebugAndroidTest
```

Sideloaded media apps only appear in Android Auto once its developer settings
allow unknown sources.

### Plays are events, never counters

`play_event` and `album_session` are append-only. Every "how many times"
question is a SQL view over them. Counters would be a one-way door; events let
you ask questions you have not thought of yet ("most played this year", full
listening history) without having recorded them in advance.

A track counts as played after half its duration or four minutes, whichever
comes first — the usual scrobbling rule, which behaves for both 90-second
interludes and 20-minute epics. An album sitting counts as a play once 75% of
its tracks complete, so skipping a hidden track does not disqualify it.

### Migrations, not rescans

The schema is versioned and upgraded in place. Rescanning an 80 GB library over
a network share is minutes of work, but re-measuring its loudness is close to an
hour — so enrichment columns are added by `ALTER TABLE` and the scanner never
writes them, leaving measured values untouched by any later rescan.

### Scanning never deletes

Files that disappear flip `present = 0` instead of being removed, so
reorganizing the library on disk cannot destroy listening history.

Rescans are incremental: unchanged files (matched on path, mtime, and size) reuse
the tag values already stored on the `track` row rather than re-reading from
disk. Tag reads for changed files run in parallel, and cover art is never parsed
out of the audio (album art comes from the folder), which keeps a full 9,500-file
rescan around 40 seconds.

Real libraries contain files that play fine but are not spec-compliant. Anything
the default parser rejects is retried in a lenient mode, and anything still
refused is handed to `ffprobe`, which is more forgiving again. On the
development library that recovered 59 of 61 initially-unreadable files; the last
two are genuinely corrupt — ffprobe reports no duration and a sample rate of
zero.

A scan will **not** mark anything as missing when an unusual share of files fail
to read. Flaky network storage once failed 538 reads in a single pass here and
shrank the library by 529 tracks; past a 5% failure rate the scan now records
what it read and leaves the rest alone, because those files are still on disk.

### Known limitations

`ms_played` is the furthest playback position reached, not a true sum of audible
time, so seeking forward past most of a track will mark it played. Good enough
for the completion rule; worth revisiting if the statistics start to matter.

Nine files in the development library remain unreadable even in lenient mode,
though ffprobe handles them. Closing that gap means shelling out to ffprobe as a
last resort.

Track rows are keyed by path, so *moving* files splits track-level history even
though album-level history survives via the tag-based identity key. Album plays
are the statistic this player is built around, so this is an accepted tradeoff
for now; content hashing would close the gap.

## Architecture

```
albumplayer-core     domain model, scanner, SQLite, play log, queries
albumplayer-enrich   loudness measurement + cover fetching
albumplayer-server   HTTP API, auth, cover and audio serving   <- runs in Docker
albumplayer-engine   album queue model + gapless GStreamer playback
albumplayer-cli      scan/inspect/audit/enrich/play commands
albumplayer-desktop  Tauri v2 shell: same UI, audio through GStreamer
ui/                  Svelte 5 web UI, served by the server and by the shell
android/             Kotlin app: Compose interface, Media3 playback
```

**One server, many clients.** The server owns the library, the database and the
play history, and runs in a container with the music mounted read-only. It has
no sound card and plays nothing; every client decodes for itself. That is what
lets a desktop app, a browser and eventually a phone share a single listening
history.

The desktop app streams over HTTP and decodes with GStreamer, so it keeps true
gapless playback — `playbin3` handles `https://` URIs as happily as local files.
A plain browser tab can play the same streams with a weaker gapless story, which
is why the Tauri shell earns its place rather than just bookmarking the web UI.

## Running the server

```sh
cp .env.example .env      # set ALBUMPLAYER_PASSWORD and MUSIC_PATH
docker compose up -d
```

Or deploy `docker-compose.yml` as a Portainer stack. Two volumes matter: `/music`
holds your library and is mounted read-only, and `/data` holds the database and
cover cache and must persist.

| Variable | Default | Purpose |
|---|---|---|
| `ALBUMPLAYER_PASSWORD` | *(required)* | The server refuses to start without one |
| `ALBUMPLAYER_MUSIC_ROOT` | `/music` | Where the library is mounted |
| `ALBUMPLAYER_DATA_DIR` | `/data` | Database and cover cache |
| `ALBUMPLAYER_ART_DIR` | `/data/art` | Cover cache; server and CLI must agree |
| `ALBUMPLAYER_BIND` | `0.0.0.0:8080` | Listen address |
| `ALBUMPLAYER_SCAN_ON_START` | `true` | Unchanged files are skipped, so this is cheap |
| `ALBUMPLAYER_SESSION_HOURS` | `720` | Login lifetime |
| `ALBUMPLAYER_TRUST_PROXY` | `false` | Read the client address from `CF-Connecting-IP` / `X-Forwarded-For` |

Scanning and enrichment run inside the container:

```sh
docker exec albumplayer albumplayer --db /data/library.db doctor
docker exec albumplayer albumplayer --db /data/library.db replaygain
docker exec albumplayer albumplayer --db /data/library.db artwork
```

### If the server cannot open its database

`unable to open database file` (SQLite error 14) means the `/data` volume is not
writable by the unprivileged user the server runs as. The image creates `/data`
owned by uid 10001 so a fresh volume inherits that, but a volume created before
this was fixed — or one seeded by hand — will still be owned by root:

```sh
docker run --rm -u 0 -v albumplayer-data:/data alpine chown -R 10001:10001 /data
docker restart albumplayer
```

Portainer prefixes stack volumes with the stack name, so the volume is likely
`<stack>_albumplayer-data`; `docker volume ls` will show it.

### Reaching it from outside the house

`docker-compose.tailscale.yml` adds a [Tailscale](https://tailscale.com) sidecar,
putting the server on a private WireGuard mesh with nothing exposed to the
public internet.

The sidecar runs Tailscale *inside a container's network namespace*, so the
host's DNS, routing table and firewall rules are untouched. That matters on a
machine already running something that works — a Cloudflare tunnel, say —
because backing this out is `compose down` rather than unpicking changes to the
host. A Tailscale install on the host is simpler, but it rewrites
`/etc/resolv.conf` for MagicDNS, which is the usual way it surprises an existing
setup (`--accept-dns=false` avoids that, at the cost of the names).

```sh
cp .env.example .env      # add TS_AUTHKEY alongside the password
docker compose -f docker-compose.tailscale.yml up -d
docker exec albumplayer-tailscale tailscale serve --bg 8080
```

In Portainer, set the stack's **Compose path** to
`docker-compose.tailscale.yml`; it defaults to `docker-compose.yml`. **Keep the
stack name unchanged** when switching — Portainer prefixes volume names with it,
and a new name means a new empty volume and a library that has to be scanned and
enriched again.

The auth key comes from the Tailscale admin console; make it **reusable and not
ephemeral** so the node survives a restart. With MagicDNS and HTTPS enabled on
the tailnet, that `serve` command publishes the server at
`https://albumplayer.<your-tailnet>.ts.net` with a real certificate — still
reachable only from your own devices. It is run once and persists in the state
volume, rather than being a mounted config file that would tie the stack to a
checkout of this repository.

Be careful of the neighbouring command: `tailscale funnel` publishes to the
public internet. `serve` does not.

**Which address to use where.** Tailscale prefers direct connections and only
relays when it cannot make one. Inside a container namespace the address it
advertises is Docker-internal, which a phone on your own wifi cannot reach, so
*at home* a Tailscale client is relayed rather than direct. Port 8080 is still
published on the LAN for that reason:

| Client | Address | Path |
|---|---|---|
| Desktop at home | `http://<lan-ip>:8080` | straight over the LAN |
| Phone away from home | `https://albumplayer.<tailnet>.ts.net` | direct, via NAT traversal |
| Phone at home | either | the tailnet address is relayed; the LAN one is not |

Leave `ALBUMPLAYER_TRUST_PROXY` off. `tailscale serve` presents the genuine peer
address, and trusting forwarded headers would let a forged one evade the login
lockout.

### Moving an existing library into the container

`docker cp` writes as root, so ownership has to be corrected afterwards or the
unprivileged server cannot open its own database:

```sh
docker cp library.db albumplayer:/data/library.db
docker cp art        albumplayer:/data/
docker run --rm -u 0 -v albumplayer-data:/data alpine chown -R 10001:10001 /data
docker restart albumplayer
```

The next scan re-keys every track under the new root, which is expected. Album
identities — and with them measured loudness, covers and play history — survive
the move, because identity is derived from tags and from paths *relative* to the
library root rather than absolute ones.

### API

Every route except `/api/health` and `/api/auth/login` needs a bearer token.
Media routes also accept `?token=`, because `<audio src>` and `<img src>` cannot
send headers.

| Route | Purpose |
|---|---|
| `POST /api/auth/login` | Password in, session token out |
| `GET /api/albums` | Listing (`sort`, `search`, `limit`) |
| `GET /api/albums/{id}` | Detail, tracklist, ReplayGain |
| `GET /api/albums/{id}/cover` | Cover image |
| `GET /api/artists` | Artists by listening |
| `GET /api/tracks/{id}/stream` | Audio, with range requests for seeking |
| `POST /api/sessions` … `/end` | Album listening sessions |
| `POST /api/plays` | Record a track listen |

Clients never see or send filesystem paths. Audio and covers are addressed by
database ID; the server resolves the ID and then verifies the result really sits
under a directory it is allowed to serve, so a stray symlink cannot escape the
music tree.

## Development

```sh
cargo test      # 145 tests: album identity, queue semantics, real playback,
                #            loudness maths, auth, and path-traversal defences

# Playback tests encode short files with ffmpeg and run a real GStreamer
# pipeline into a fake sink; they skip themselves if either is unavailable.
cargo clippy --all-targets
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. This is the usual arrangement in the Rust ecosystem: MIT is the
simpler licence, and Apache-2.0 adds an explicit patent grant, which is worth
having in audio software where codec patent claims are not hypothetical.

Unless you state otherwise, any contribution you submit shall be dual-licensed
as above, with no additional terms.

### Guessing the password

Argon2 alone does not protect a single-password login. Verification takes about
15 ms, which a handful of parallel connections turns into roughly 150 guesses a
second — a dictionary password falls in minutes.

Failed logins are therefore counted per client address. Five are free, so a
typo costs nothing; after that each failure doubles the lockout, from two
seconds up to a cap of fifteen minutes. The lockout is checked before the
password is verified, so a throttled client cannot keep the CPU busy, and it
applies to the correct password too, so it cannot be used as an oracle. A
successful login clears the record.

Measured against the running server, a hundred parallel guesses now get 76
refusals and 24 attempts, against 100 attempts at ~150/second before.

**Turn on `ALBUMPLAYER_TRUST_PROXY` only behind a tunnel or reverse proxy.**
Forwarded-for headers are trivially forged: trusted on a directly-exposed
server, an attacker simply invents a new address per guess and the lockout never
engages. Left off behind a tunnel, every request appears to come from the proxy
and one attacker locks out the household — so it must match the deployment
either way.

### Dependencies

Every Rust dependency is permissively licensed. GStreamer and WebKitGTK are
LGPL-2.1 and are linked dynamically, which imposes no constraint on this
project's licence. No GPL-licensed GStreamer plugin is required to decode
mp3, AAC, Opus or FLAC.
