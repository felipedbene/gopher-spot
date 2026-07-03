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
| `track_id`    | track loaded | Spotify track id                                  |
| `position_ms` | track loaded | playback position at snapshot time (int, ms)      |
| `duration_ms` | track loaded | track length (int, ms)                            |
| `volume`      | device known | active device volume, `0`–`100`                   |
| `queue_len`   | always       | number of queued tracks (best-effort; `0` if n/a) |
| `ts`          | always       | unix epoch **ms** when the dcgi took the snapshot  |

The `track…duration_ms` keys are omitted when `state` is `stopped` (nothing
loaded); `volume` is omitted when no active device reports one. A client keys off
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

### Errors

```
api	1
error	<short code>
message	<english description>
```

Codes: `bad_range`, `no_track`, `not_found`, `upstream` (a Spotify/transport
failure, or the OAuth Secret not being configured). `message` is human-readable
English and **not** part of the stable contract — switch on `error`, not on text.

## Versioning

`/spot/api/1` is **frozen**. Rules:

- **Additive** changes stay in v1: new keys may be added to any response.
- **Breaking** changes (renaming/removing a key, changing a value's meaning) go to
  a new path, `/spot/api/2`.
- **Clients MUST ignore unknown keys.** This is the whole point of the freeze: v1
  can grow new keys without breaking an old client, and the client tolerates them.

## Known deviations

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
