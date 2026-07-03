# gopher-spot machine API — v1 (`/spot/api/1`)

The **machine API** is the contract the DeToca client consumes (from fio 9 on).
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
| `volume`      | device known | active device volume, `0`–`100`                   |
| `queue_len`   | always       | number of queued tracks (best-effort; `0` if n/a) |
| `ts`          | always       | unix epoch **ms** when the dcgi took the snapshot  |

The `track…duration_ms` keys are omitted when `state` is `stopped` (nothing
loaded); `album_id` is omitted when the current item carries no album uri;
`volume` is omitted when no active device reports one. A client keys off
`state` first.

`ts` exists so the client can **interpolate the progress bar between polls**:
`estimated_position = position_ms + (now − ts)` while `state == playing`.

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

### Commands

Each command executes, then returns the same **`/now` snapshot** so the client
leaves with fresh state in one round-trip.

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
on disk (24 h). Only a cache **miss** hits Spotify's CDN (logged as `[cover] miss …`
on stderr); the Radinho asking for a cover on every track change, and the playlist
asking for N thumbnails at once, are served from cache.

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

Codes: `bad_range`, `bad_uri`, `no_track`, `not_found`, `upstream` (a
Spotify/transport failure, or the OAuth Secret not being configured). `message`
is human-readable English and **not** part of the stable contract — switch on
`error`, not on text.

## Versioning

`/spot/api/1` is **frozen**. Rules:

- **Additive** changes stay in v1: new keys may be added to any response.
- **Breaking** changes (renaming/removing a key, changing a value's meaning) go to
  a new path, `/spot/api/2`.
- **Clients MUST ignore unknown keys.** This is the whole point of the freeze: v1
  can grow new keys without breaking an old client, and the client tolerates them.

## Known deviations

- **`/now` micro-cache (added in fio S3/2).** The server caches the rendered
  `/now` document for **~1 second**. A burst of polls inside that window collapses
  to a **single** upstream Spotify call, and every poll in the window returns the
  **same document — including the same `ts`**. This is deliberate: interpolate the
  progress with `ts` (`estimated_position = position_ms + (now − ts)`) and a
  ≤1 s-stale snapshot is invisible. **Commands bust the cache**: after any
  `play`/`pause`/`next`/`prev`/`volume`/`seek`/`queue/add`/`wake`, the next `/now`
  is fetched fresh, so a state change is never masked by the cache. The TTL is a
  fixed constant (no configuration); errors are never cached.
- **Eventual consistency after a command.** The snapshot a command returns
  reflects what Spotify's Web API reports *at that instant*. Spotify is eventually
  consistent for the librespot Connect device: a `seek` (and sometimes the device
  `volume`) settles on Spotify's side ~1–2 s later, so the returned snapshot can
  still show the pre-command `position_ms`/`volume`. The command **did** take
  effect; re-poll `/now` (and interpolate via `ts`) for the settled value. This is
  inherent to driving playback through the Web API rather than a local librespot
  control socket (librespot exposes none).
- `position_ms` precision: it is Spotify's reported device position at fetch time,
  typically accurate to ~1 s, plus one network RTT. Interpolate with `ts`.
