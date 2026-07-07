# gopher-spot machine API — v1 (`/spot/api/1`)

The **machine API** is the contract the native clients — DeToca (Mac OS X 10.6),
DeGelato (10.5/PowerPC), Casquinha (Mac OS 9) — consume (first shipped for
DeToca's fio 9).
It is deliberately separate from the human Gopher menus: the menus are PT-BR
gophermaps for MacOS 9 clients (Netscape, Bombadillo, MacAST); this API is a
stable, machine-parseable surface that never has to guess at a display string.

> Introduced in **fio S1**. `/spot/api/1` is **frozen**: see [Versioning](#versioning).

## Transport

Each endpoint is a Gopher **type-0 (text)** selector. The response body is:

- **UTF-8**, no BOM.
- One **`key<TAB>value`** line per field (a single literal TAB `U+0009`).
- **CRLF** (`\r\n`) line endings, including after the last line.
- No trailing Gopher `.` terminator — the response ends at connection close.

Keys are lowercase ASCII / `snake_case` and are **never localized**. Values are
Spotify's own strings, verbatim (so a track named `Construção` comes through as
`Construção`, in UTF-8). Any TAB/CR/LF inside a value is replaced with a space
so it can't forge a line or a key.

### Why a `.cgi`, not the `.dcgi`

The human menus are served by `/srv/spot/index.dcgi`. geomyidae **interprets**
`.dcgi` output as a gophermap: it rewrites `[t|name|sel|host|port]` lines and
**wraps any other line as an info item**, and it rejects a raw TAB outright. That
would destroy a `key<TAB>value` document.

So the API is served by `/srv/spot/api/index.cgi` — a **`.cgi`**, which geomyidae
pipes to the socket **verbatim** (`handlecgi`), tabs and UTF-8 intact. geomyidae's
REST path-walk resolves `/spot/api/1/<endpoint>` into the `/srv/spot/api`
directory and runs its `index.cgi`. One binary backs both wrappers; it emits raw
UTF-8 for `/spot/api/*` and Latin-1-transcoded gophermaps for everything else.

## Endpoints

### State

```
/spot/api/1/now
```

Returns a **snapshot**:

| key           | when         | value                                             |
|---------------|--------------|---------------------------------------------------|
| `api`         | always       | contract version — `1`                            |
| `state`       | always       | `playing` \| `paused` \| `stopped`                |
| `track`       | track loaded | track name                                        |
| `artist`      | track loaded | artist name(s), joined with `, `                  |
| `album`       | track loaded | album name                                        |
| `album_id`    | album known  | Spotify album id — feed it to `/cover/<id>/<size>`|
| `track_id`    | track loaded | Spotify track id                                  |
| `position_ms` | track loaded | playback position at snapshot time (int, ms)      |
| `duration_ms` | track loaded | track length (int, ms)                            |
| `device`      | always       | `active` \| `idle` — see below (added in fio S3/3) |
| `volume`      | device known | active device volume, `0`–`100`                   |
| `queue_len`   | always       | number of queued tracks (best-effort; `0` if n/a) |
| `ts`          | always       | unix epoch **ms** when the dcgi took the snapshot  |

The `track…duration_ms` keys are omitted when `state` is `stopped` (nothing
loaded); `album_id` is omitted when the current item carries no album uri;
`volume` is omitted when no active device reports one. A client keys off
`state` first.

`ts` exists so the client can **interpolate the progress bar between polls**:
`estimated_position = position_ms + (now − ts)` while `state == playing`.

**`device`** (always present) tells whether the account's current player *is* the
gopher-spot librespot device — the one the audio stream (`:8000/spotify.mp3`)
actually carries:

- `active` — gopher-spot is the current player. What `/now` reports is what the
  audio stream is playing.
- `idle` — gopher-spot is **not** the current player: playback is on another
  device (phone/desktop), or nothing is playing / no active device at all. The
  `state`/`track` fields may still be populated (they reflect the account's
  playback wherever it is), but the **audio stream won't carry it**. Recover with
  `wake` (below). With the fio S3/1 audio drainer, `idle` is now uncommon — it
  means a real handoff to another device or a librespot crash, not merely "nobody
  is listening".

Example (`printf '/spot/api/1/now\r\n' | nc <lb> 70`):

```
api	1
state	playing
track	Construção
artist	Chico Buarque
album	Construção
track_id	3FIuBxOxuQ6kYy8JO0gq2a
position_ms	26221
duration_ms	383626
volume	100
queue_len	0
ts	1783105644431
```

### Stream (added 2026-07, fio A)

```
/spot/api/1/stream
```

The **media plane reporting its own state**: what the Icecast audio stream
(`:8000/spotify.mp3`) is actually carrying, straight from Icecast's mount
stats. This is a different state owner than `/now` (which reports what the
Spotify Web API believes) — the two can legitimately disagree for a few
seconds, and comparing them is the point.

| key         | when   | value                                                       |
|-------------|--------|--------------------------------------------------------------|
| `api`       | always | `1`                                                          |
| `live`      | always | `1` — a live source feeds `/spotify.mp3` (real audio); `0` — the silence fallback is carrying (idle/paused past the source timeout, or the audio chain is down/respawning) |
| `listeners` | always | **external** listeners on the stream (the server's permanent internal drainer is subtracted, clamped at ≥ 0) |
| `ts`        | always | unix epoch **ms** when the server took the reading            |

How to read it:

- **`live 1`** — librespot's chain has an active source on the mount; the
  stream carries real audio (what plays is what `/now` describes, modulo the
  stream's ~3 s encoder latency).
- **`live 0` while `/now` says `state playing` + `device active`** — the
  genuine **anomaly**: Spotify believes the gopher-spot device is playing but
  the mount carries no audio (the chain is dead/respawning). This is the
  server-fact replacement for client-side "my receive went dry" heuristics
  ("waiting for Spotify"). Everything else about `live 0` is normal: paused or
  idle long enough for Icecast to drop the source is *expected* silence.
- `listeners` counts across the live/fallback failover (Icecast moves parked
  listeners to `/silence.mp3` when the live source drops), so it is stable
  through pause/resume cycles.

The document is served from a **~2 s micro-cache** and Icecast is fetched with
a short dedicated timeout — `/stream` never adds load or latency to `/now`,
and vice-versa (they consult different upstreams and never call each other's).
Icecast unreachable → `error upstream`. `/stream` answers even when the
Spotify OAuth Secret is missing (it is not a Spotify endpoint).

> **Feature-detect:** on an older server this selector answers `not_found` —
> the client's signal to keep its receive-side heuristic instead.

### Commands

Each command executes, then returns the same **`/now` snapshot** so the client
leaves with fresh state in one round-trip. Since fio A2 the server **settles**
the snapshot before returning it: it short-polls Spotify (up to ~2 s) until
the snapshot reflects the command, so the reply *usually* already shows the
new track / state / volume — see [Known deviations](#known-deviations).

```
/spot/api/1/play
/spot/api/1/pause
/spot/api/1/next
/spot/api/1/prev
/spot/api/1/volume?<0-100>
/spot/api/1/seek?<position_ms>
```

- `volume` — continuous `0`–`100`. Out of range or non-integer → `bad_range`.
- `seek` — clamped to `[0, duration_ms]`. Non-integer/negative → `bad_range`;
  nothing playing → `no_track`.
- The value for `volume`/`seek` is the bare argument after `?` (`volume?70`), not
  `key=value`.
- Commands are **idempotent** where it makes sense: `play` while already playing
  (and `pause` while already paused) returns a snapshot, **not** an error — even
  though Spotify itself 403s "Restriction violated" in that case, which we swallow.

### Wake (added in fio S3/3)

```
/spot/api/1/wake
/spot/api/1/wake?play=1
```

**Transfer playback to the gopher-spot device** (`PUT /v1/me/player` with
`device_ids`). This is how a client turns a `device idle` `/now` back into
`device active`: playback that drifted to the phone (or was lost to a librespot
crash) is pulled back onto the device the audio stream carries.

- Bare `wake` transfers **without** changing the play/pause state.
- `wake?play=1` also **resumes** playback on transfer (the Web API's native
  `play` boolean). A client that finds `device idle` and wants to hear audio
  should call `wake?play=1`.

Returns the fresh **`/now`** snapshot (convention; subject to the same eventual
consistency note — the returned snapshot may still show `device idle` for ~1–2 s
before Spotify settles). If the gopher-spot device isn't registered (librespot is
down), returns `no_device`.

### Queue (added in fio S2)

```
/spot/api/1/queue
```

The upcoming tracks, in the order they will play, as indexed `item.<i>.*` keys
(`<i>` from `0`). Flattening the list into keys keeps the wire invariant — exactly
one TAB per line.

| key                  | when         | value                                    |
|----------------------|--------------|------------------------------------------|
| `api`                | always       | `1`                                      |
| `queue_len`          | always       | number of upcoming tracks (`0` if empty) |
| `item.<i>.uri`       | per item     | `spotify:track:<id>`                     |
| `item.<i>.track`     | per item     | track name                               |
| `item.<i>.artist`    | per item     | artist name(s), joined with `, `         |
| `item.<i>.album_id`  | album known  | Spotify album id (for `/cover`)          |
| `item.<i>.duration_ms`| per item    | track length (int, ms)                   |
| `ts`                 | always       | unix epoch **ms** at snapshot time        |

An **empty queue** is just `queue_len` `0` with no `item.*` lines. `item.<i>.album_id`
is omitted for an item whose album carries no uri.

```
/spot/api/1/queue/add?<uri>
```

Enqueues `<uri>` (the bare argument after `?`, e.g. `queue/add?spotify:track:4iV5W9…`)
on the gopher-spot device, then returns the fresh **`/queue`** snapshot — the
document the client's playlist redraws (not `/now`). A non-track or malformed uri
→ `bad_uri`.

> **Eventual consistency.** Like every command, the returned snapshot reflects what
> Spotify reports *at that instant*; the just-added item may not appear for ~1–2 s.
> The add **did** take effect — the client re-polls `/queue`.

There is **no** `queue/clear`: the Spotify Web API exposes no endpoint to remove
from or clear the queue (only add). It is deliberately absent from v1; if Spotify
ever adds one it can be added additively. We do **not** emulate it with chained
skips.

```
/spot/api/1/queue/album?id=<album_id>
```

Enqueue a whole **album** onto up-next (added for album-context). Spotify's queue
endpoint takes one track uri at a time — there is no context enqueue — so the
server expands the album into a bounded run of per-track adds and returns the
fresh **`/queue`** snapshot, same as `queue/add`. Non-destructive: unlike
`play/context` (which replaces the queue and starts the album *now*), this
**appends** without interrupting the current track.

- Capped at **24 tracks** per call (bounds the burst inside geomyidae's request
  budget; covers essentially every single album — a longer compilation is
  truncated, best-effort).
- **Best-effort:** a rate-limit (or upstream error) partway through stops the run
  and returns what landed; if *nothing* landed it surfaces as `rate_limited` /
  `upstream`. Eventually consistent like `queue/add` — the client re-polls
  `/queue`.
- Non-base62 id → `not_found`; an album with no playable tracks → `not_found`.
- A new sub: an old server answers `not_found` (feature-detect).

> **No `queue/clear`.** Emulating it was investigated and abandoned: the public
> Web API can't remove queued items, and the workarounds don't survive Spotify's
> no-op coalescing — replaying the current track *or* its context (both verified
> live) leaves the queue intact. Only Spotify's private backend can clear, which
> third-party apps can't reach.

### Search (added in fio S3/4)

```
/spot/api/1/search?q=<urlencoded query>
```

Search across **tracks, artists, and albums**. Tracks lead as a v1 **list** (same
`item.<i>.*` shape as `/queue`), with `result_len` as the count header; artists
and albums follow in their own additive blocks:

| key                    | when     | value                                    |
|------------------------|----------|------------------------------------------|
| `api`                  | always   | `1`                                      |
| `result_len`           | always   | number of **tracks** (`0` if none)       |
| `item.<i>.uri`         | per track | `spotify:track:<id>`                     |
| `item.<i>.track`       | per track | track name                               |
| `item.<i>.artist`      | per track | artist name(s), joined with `, `         |
| `item.<i>.album_id`    | album known | Spotify album id (for `/cover`)        |
| `item.<i>.duration_ms` | per track | track length (int, ms)                   |
| `artist_len`           | always   | number of artist results                 |
| `artist.<i>.id`        | per artist | Spotify artist id (feed `/artist/<id>/albums`) |
| `artist.<i>.name`      | per artist | artist name                            |
| `album_len`            | always   | number of album results                  |
| `album.<i>.id`         | per album | Spotify album id (feed `/album/<id>` or `/play/context`) |
| `album.<i>.name`       | per album | album name                             |
| `ts`                   | always   | unix epoch **ms** at snapshot time        |

- `q` is the **urlencoded** query; UTF-8 is decoded correctly, so
  `search?q=constru%C3%A7%C3%A3o` searches for `construção`.
- Empty or absent `q` (including whitespace-only) → `bad_query`.
- The `artist_len` / `album_len` blocks are **additive** (added this fio): the
  `search()` call always asked Spotify for `type=track,album,artist`, so surfacing
  artists/albums costs no extra upstream call. Old clients that read only
  `result_len` + `item.*` are unaffected. An artist/album whose uri Spotify omitted
  is dropped (a client needs the id to drill in), so `artist_len` can be less than
  the raw hit count.
- Each kind is **capped at 10** results: Spotify's `/v1/search` **400s `limit>10`**
  ("Invalid limit") — verified empirically (`limit=20`/`50` both 400, `10` works).
  A client must read the `*_len` headers and not assume a fixed count.

### Artist discography (added for album-context)

```
/spot/api/1/artist/<id>/albums
/spot/api/1/artist/<id>/albums?offset=<n>
```

An artist's albums as an indexed list, paginated via `?offset=` (20/page). Feed a
`<id>` from a `search` `artist.<i>.id`.

| key             | when     | value                                    |
|-----------------|----------|------------------------------------------|
| `api`           | always   | `1`                                      |
| `result_len`    | always   | albums in this response                  |
| `total`         | always   | grand total (for paging)                 |
| `offset`        | always   | this page's offset                       |
| `item.<i>.id`   | per item | Spotify album id (for `/album`/`/play/context`) |
| `item.<i>.name` | per item | album name                               |
| `ts`            | always   | unix epoch **ms**                        |

Unknown or non-base62 `<id>` → `not_found`. An album with no uri (hence no id) is
dropped, so `result_len` can be less than the raw page size.

### Album detail (added for album-context)

```
/spot/api/1/album/<id>
/spot/api/1/album/<id>?offset=<n>
```

An album's header then its tracks in the **`/search` list shape**, paginated via
`?offset=` — lets a client show the track list before playing the whole thing.

| key           | when   | value                                       |
|---------------|--------|---------------------------------------------|
| `api`         | always | `1`                                         |
| `name`        | always | album name                                  |
| `artist`      | always | album artist(s), joined with `, `           |
| `total`       | always | total tracks on the album                   |
| `result_len`  | always | tracks in this response                     |
| `offset`      | always | this page's offset                          |
| `item.<i>.*`  | per item | same track block as `/search` / `/queue`  |
| `ts`          | always | unix epoch **ms**                           |

Unknown or non-base62 `<id>` → `not_found`.

### Playing a context / whole album (added for album-context)

```
/spot/api/1/play/context?uri=<spotify:album|artist|playlist:id>&offset=<n>
```

Play a whole **context** in order — the machine-API "queue this album". One
upstream `PUT` hands Spotify the `context_uri`, so **Spotify owns the
continuation**: it auto-advances track→track and `next`/`prev` follow the album
order (unlike `play/from`, which needs the explicit track ids). `offset` (default
`0`) is the 0-based start track within the context. Replies with a **settled**
`/now` snapshot (fio A2): the reply waits for playback to actually land on the
gopher-spot device, so it never reads `device idle` / "playing elsewhere".

- `uri` must be `spotify:album:`, `spotify:artist:`, or `spotify:playlist:` +
  base62 id. A `spotify:track:` uri (no context) or malformed uri → `bad_uri`;
  missing/empty `uri` → `bad_query`; non-integer `offset` → `bad_range`.
- gopher-spot device not registered (librespot down) → `no_device`.
- A **new** sub, like `play/from`: an old server answers `not_found`, which is the
  client's clean feature-detect signal.
- **Playlists are still dev-mode blocked** for track reads, but playing a playlist
  *context* works (same as the human `?context_uri=` path). Albums and artists are
  the reliable cases.

### Playlists (added in fio S3/5)

```
/spot/api/1/playlists
/spot/api/1/playlists?offset=<n>
```

The user's playlists as an indexed list, paginated via `?offset=` (20/page):

| key                   | when     | value                                       |
|-----------------------|----------|---------------------------------------------|
| `api`                 | always   | `1`                                         |
| `result_len`          | always   | playlists in this response                  |
| `total`               | always   | grand total (for paging)                    |
| `offset`              | always   | this page's offset                          |
| `item.<i>.id`         | per item | playlist id — feed to `/playlists/<id>`     |
| `item.<i>.name`       | per item | playlist name                               |
| `item.<i>.tracks_len` | per item | track count (see the caveat below)          |
| `ts`                  | always   | unix epoch **ms**                            |

Playlists with no id are omitted (a client can't open them). `result_len` counts
what's emitted; `total` is Spotify's grand total.

```
/spot/api/1/playlists/<id>
/spot/api/1/playlists/<id>?offset=<n>
```

A playlist's tracks in the **`/search` list shape**, led by a `name` header:

| key                    | when     | value                                    |
|------------------------|----------|------------------------------------------|
| `api`                  | always   | `1`                                      |
| `name`                 | always   | the playlist's display name              |
| `result_len`           | always   | tracks in this response                  |
| `total` / `offset`     | always   | paging                                   |
| `item.<i>.*`           | per item | same block as `/search` / `/queue`       |
| `ts`                   | always   | unix epoch **ms**                         |

- Unknown id → `not_found`.
- A playlist Spotify won't let this app read → **`forbidden`** (see below).

> **⚠ Empirical reality (fio S3/5 Phase 0).** For this app+token, **every**
> `/v1/playlists/{id}/tracks` returns **HTTP 403 — including the user's own
> playlists** — and `tracks.total` reports `0`. This is Spotify's Nov-2024
> dev-mode restriction on playlist track reads (the token *does* hold
> `playlist-read-private`, so it is **not** a scope gap, and it is **not**
> fixable client-side). Verified against the live account. Consequence: today
> `/playlists` lists names/ids fine, but `/playlists/<id>` almost always returns
> `forbidden` and `tracks_len` is `0`. The endpoint is implemented correctly and
> will start returning tracks the moment the app gains extended quota (or Spotify
> lifts the block); until then a client should expect `forbidden` and fall back to
> **playing the playlist as a context** (below), which does **not** require track
> read access.

### Playing a context (added in fio S3/5)

Context playback is triggered on the **human** `/spot/play` selector (the same one
the client already uses for single tracks — it returns a gophermap, not a v1
document), extended additively:

```
/spot/play?context_uri=<album|playlist|artist uri>&offset=<i>
```

Starts track `i` (0-based) **within** that context, so `next`/`prev` follow the
album/playlist order instead of the autoplay radio. The existing single-track
`/spot/play?uri=<track uri>` is unchanged. Unlike reading a playlist's tracks,
starting a playlist **context** works even where `/playlists/<id>` is `forbidden`
(playback resolves server-side and needs no track-read access). Poll `/spot/api/1/now`
for the resulting state.

### Play from a list (added 2026-07, for Casquinha)

```
/spot/api/1/play/from?ids=<id1>,<id2>,…,<idK>&offset=<n>
```

The native **"play from here onward"**: start playback of an explicit track
list at index `offset`, in one call. The server hands Spotify the whole list
(`PUT /v1/me/player/play` with a `uris` array + `offset.position`), so
**Spotify owns the continuation**: auto-advance at each track end, `next`/`prev`
move within the list. This is the v1 answer to jumping into a queue/playlist
view — without it, a jump via the single-uri play path leaves a one-track
context that **stops dead at the track's end** (librespot never pulls the
server-side queue), forcing clients into chained skips or client-side
sequencing, which this API rejects on principle.

- `ids` — comma-joined **bare base62 track ids** (the 22-char tail of
  `spotify:track:<id>`), `1 ≤ K ≤ 24`. Bare ids, not full uris: geomyidae's
  request-line buffer caps selector length, and 24 bare ids stays comfortably
  inside it. `K > 24` → `bad_range`.
- `offset` — 0-based index into the list to start at; optional, default `0`.

Returns the fresh **`/now`** snapshot (command convention) and busts the queue
cache (like `next`/`prev`). Errors:

- missing/empty `ids` → `bad_query`;
- any id that isn't exactly 22 base62 chars (`[0-9A-Za-z]{22}`) → `bad_uri`;
- `offset` ≥ K or non-numeric → `bad_range`;
- otherwise the standard upstream mapping (`rate_limited`, `no_device`,
  `upstream`).

> **Why a new sub instead of `ids=` on `play`?** The server strips the query
> when routing, so on an *old* server `play?ids=…` would silently match the
> existing resume arm — a client could never feature-detect. As its own
> selector (mirroring the `queue/add` naming), an old server answers
> `not_found`, which is the client's clean signal to fall back (e.g. Casquinha
> b41 → its b40 single-uri behavior). Additive, v1-legal per
> [Versioning](#versioning).

### Cover (added in fio S2)

```
/spot/api/1/cover/<album_id>/<size>
```

The album cover as **raw JPEG bytes** — this is the one endpoint that is *not* a
tab-delimited text document. It is meant to back a Gopher **type-`I`** (image)
item. `<size>` ∈ `{64, 300, 640}` (the sizes Spotify's CDN serves); the server
returns the smallest image ≥ the requested size, falling back to the largest
available.

Covers are **immutable** per `album_id`+size, so the server caches the JPEG bytes
on disk (24 h). Only a cache **miss** hits Spotify's CDN (nothing is logged on the
API path — geomyidae splices stderr into the socket); the Radinho asking for a
cover on every track change, and the playlist asking for N thumbnails at once,
are served from cache.

Errors are returned in the normal v1 **text** error format (so a client that gets
a text body instead of JPEG reads the code):

- `<size>` outside `{64, 300, 640}` (or non-integer / missing) → `bad_range`.
- unknown `album_id`, or an album with no cover image → `not_found`.

### Errors

```
api	1
error	<short code>
message	<english description>
```

Codes: `bad_range`, `bad_uri`, `bad_query` (empty search `q`, or missing
`play/from` ids),
`no_track`, `no_device` (fio S3/3: `wake` found no registered gopher-spot
device), `not_found`, `forbidden` (fio S3/5: Spotify blocks this app from reading
the playlist — distinct from `not_found`; the playlist exists), `rate_limited`
(fio 429: Spotify is throttling the bridge, which is inside its cooldown window —
keep showing the last snapshot and retry at your normal cadence; do **not** poll
faster), `upstream` (a Spotify/transport failure, or the OAuth Secret not being
configured). `message` is human-readable English and **not** part of the stable
contract — switch on `error`, not on text.

Client-side rules for all of this — cadence, backoff, caching — live in
[`CLIENTS.md`](CLIENTS.md).

## Versioning

`/spot/api/1` is **frozen**. Rules:

- **Additive** changes stay in v1: new keys may be added to any response.
- **Breaking** changes (renaming/removing a key, changing a value's meaning) go to
  a new path, `/spot/api/2`.
- **Clients MUST ignore unknown keys.** This is the whole point of the freeze: v1
  can grow new keys without breaking an old client, and the client tolerates them.

## Known deviations

- **`/now` micro-cache (fio S3/2, widened in fio 429).** The server caches the
  rendered `/now` document for **~3 seconds**. A burst of polls inside that window
  collapses to a **single** upstream Spotify fetch, and every poll in the window
  returns the **same document — including the same `ts`**. This is deliberate:
  interpolate the progress with `ts` (`estimated_position = position_ms +
  (now − ts)`) and a ≤3 s-stale snapshot is invisible. **Commands bust the
  cache**: after any `play`/`pause`/`next`/`prev`/`volume`/`seek`/`queue/add`/
  `wake`/`play/from`, the next `/now` is fetched fresh, so a state change is
  never masked by the cache. The TTL is a fixed constant (no configuration); errors are never
  cached.
- **Rate-limit degradation (fio 429).** When Spotify 429s the bridge, it stops
  calling Spotify for the `Retry-After` window. During that window `/now` serves
  the **last good snapshot** (up to ~30 s old, with its **original `ts`**) so a
  polling client keeps rendering; other endpoints (and `/now` past the stale
  window) return `error rate_limited`. A snapshot with an old `ts` is
  indistinguishable from a normal cache hit — interpolate as usual.
- **`not_found` is negative-cached (~5 min).** An unknown album/playlist id keeps
  answering `not_found` from cache without an upstream call, so a brand-new
  resource can take up to ~5 minutes to appear. Don't retry-loop a `not_found`.
- **`queue_len` is best-effort.** The queue is cached ~10 s server-side, and when
  nothing is playing `/now` reports `queue_len` `0` without consulting Spotify.
- **Eventual consistency after a command.** Spotify is eventually consistent
  for the librespot Connect device: a command's effect appears in the player
  state ~1–2 s later. **Since fio A2 the server absorbs most of this**: after
  issuing a command it short-polls the player (up to 4 polls, ~500 ms apart,
  ~2 s cap) until the snapshot *reflects* the command — the new `track_id`
  after `next`/`prev`/`play/from`, the target `state` after `play`/`pause`,
  the requested `volume`, a `position_ms` inside the seek window, `device
  active` after `wake` — and returns (and caches) that settled snapshot. The
  settle is strictly **best-effort**: on timeout the latest snapshot is
  returned anyway (never an error), and if a rate-limit cooldown arms
  mid-settle the polling stops immediately. So clients must still tolerate
  the **rare** unsettled echo (a reply that shows pre-command state): the
  command **did** take effect; re-poll `/now` for the settled value. Note the
  reply's `ts` is stamped when the settled reading was taken (up to ~1.5 s
  after the request landed) — interpolate from it as usual. Old servers
  (pre-A2) return the instant, possibly-unsettled snapshot every time.
- `position_ms` precision: it is Spotify's reported device position at fetch time,
  typically accurate to ~1 s, plus one network RTT. Interpolate with `ts`.
