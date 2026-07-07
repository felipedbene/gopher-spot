# Client best practices — `/spot/api/1`

How a well-behaved client uses the bridge. The server defends Spotify's rate
limits on its own ([README → Upstream protection](README.md#upstream-protection-spotify-rate-limits)),
but a sloppy client can still waste round-trips, fight the caches, or turn a
throttling blip into a frozen UI. Use the [checklist](#spot-check-checklist) to
audit a client (DeToca / DeGelato / Casquinha / anything new).

The wire contract itself (endpoints, keys, error codes) is [`API.md`](API.md);
this document is only about *how to consume it well*.

## Polling `/now`

1. **Poll at a fixed cadence, ≥2 s. Never faster.** The server's micro-cache
   window is ~3 s, so a 2 s poll costs roughly one upstream fetch per window.
   Sub-second polling buys nothing — you'll receive the same document again.
2. **Interpolate with `ts`, don't poll harder.** Progress display should be
   `position_ms + (local_now − ts)` while `state` is `playing`. Two polls in one
   cache window return byte-identical documents (same `ts`) — that's a cache
   hit, not a bug.
3. **Guard with newest-wins.** Keep the `ts` of the snapshot you're rendering
   and ignore any incoming snapshot with an older `ts`. This makes the
   rate-limit stale-serve (below) and out-of-order responses harmless.
4. **Don't fire a `/now` poll right after a command.** Every command already
   replies with a fresh snapshot — render *that*. An extra poll is a wasted
   round-trip (and on a miss, a wasted upstream call).

## Errors and backoff

5. **Switch on the `error` code, never on `message` text.** `message` is
   explicitly not part of the frozen contract.
6. **`rate_limited` = keep calm and keep the last snapshot.** Spotify is
   throttling the bridge; the bridge is already refusing to call upstream. The
   right client behavior: keep rendering what you have, keep your normal poll
   cadence (the bridge answers from cache — polls are cheap), and *don't* add a
   retry storm. Note `/now` usually shields you: for up to ~30 s of throttling
   you receive stale-but-valid snapshots, not this error.
7. **Never tight-loop a failed command.** One retry after a short pause (2–5 s)
   is fine; automated rapid retries of `play`/`next`/`wake` are how a cooldown
   window gets extended. Surface the error to the human instead.
8. **Don't retry-loop `not_found`.** It's negative-cached ~5 min server-side, so
   your retries are answered from cache and can't "refresh" anything.

## Covers

9. **Covers are immutable — cache them client-side, keyed `album_id:size`,
   forever** (or LRU-bounded by disk, never by time). The server caches them
   24 h, but your cache hit costs zero round-trips.
10. **Fetch a cover only when `album_id` changes**, not per `/now` poll. The
    `album_id` key on the snapshot is your cache key; no `album_id`, no fetch.
11. **Request only the canonical sizes** — `64`, `300`, `640`. Anything else is
    a `bad_range` error, guaranteed.
12. **Expect text on failure.** If the first bytes aren't JPEG (`FF D8`), parse
    the body as a v1 error document instead of rendering garbage.

## Queue, search, playlists

13. **`queue/add` is eventually consistent.** The returned `/queue` snapshot may
    not contain the item yet (~1–2 s). Re-poll `/queue` once after a short
    delay; don't loop until it appears.
14. **`queue_len` is best-effort** — a display hint, not truth. It can be up to
    ~10 s stale and is `0` whenever nothing is playing. Never gate logic on it;
    fetch `/queue` when the user actually opens the queue view.
15. **Debounce search.** Send `search?q=` on submit (or ≥500 ms idle), never per
    keystroke. Results cap at 10 — that's a Spotify limit, not a paging hint.
16. **URL-encode the query as UTF-8 bytes** (`construção` →
    `constru%C3%A7%C3%A3o`). Latin-1/MacRoman percent-encoding mangles accents.
17. **Playlists: expect `forbidden` on track reads** (Spotify's dev-mode block —
    even the user's own playlists). List names/ids, and play a playlist as a
    context via the human `/spot/play?context_uri=` endpoint; don't treat
    `forbidden` as a client bug or retry it.

## Commands and device state

18. **Jumping into a list: one `play/from`, never chained skips or a client-side
    sequencer.** Send the tail of the visible list (or the whole list plus
    `offset`) as comma-joined **bare** base62 ids — `play/from?ids=…&offset=N`,
    ≤24 ids — and Spotify owns the continuation (auto-advance, `next`/`prev`
    within the list). Do **not** jump via a single-track play and then chain
    `/next`, and do **not** sequence tracks client-side with an end-of-track
    watchdog: the single-track context stops dead at the track's end. On
    `error not_found` (an older server without the endpoint) fall back
    gracefully — the new sub exists precisely so old servers answer that
    cleanly.
19. **`wake` is a user action, not a reflex.** Offer it when `device` is `idle`;
    never auto-`wake` in a poll loop (a phone user would fight the bridge for
    the playback session).
20. **Trust the server's clamps.** `seek` clamps to the track duration and
    `volume` rejects out-of-range values server-side; client-side pre-validation
    is only a UX nicety.
21. **Tolerate the (now rare) unsettled command echo.** Since fio A2 the server
    settles the reply snapshot before returning it (short-polling Spotify up to
    ~2 s), so a command's reply *usually* already reflects it — a skip's reply
    carries the new `track_id`, a `wake?play=1` reply says `device` `active`.
    But settling is best-effort (it times out, and it aborts during a 429
    cooldown), and **old servers don't settle at all** — so keep the tolerance:
    if the echo looks pre-command, the command still took effect; let the next
    poll catch up. Don't re-issue the command because the echo looks stale.
22. **Parse forward-compatibly.** `key<TAB>value`, CRLF lines, UTF-8. Ignore
    unknown keys (v1 grows additively) and tolerate keys appearing in any order.
    Metadata keys (`track`…`duration_ms`) exist only when a track is loaded —
    key off `state` first.

## The media plane (`/stream`)

23. **Poll `/stream` at the same lazy cadence as `/now`, or lazier** (10 s is
    plenty — it's a slow-moving fact; the server caches it ~2 s anyway). Use it
    to *reconcile*, not to render playback state: "waiting on the audio chain"
    is `live` `0` **while** `/now` says `state` `playing` + `device` `active` —
    a server fact, replacing any "my receive went dry" guessing. Everything
    else about `live` `0` is expected silence (paused/idle), not an error.
24. **Feature-detect `/stream` once per launch.** An old server answers
    `not_found`; on that, stop asking for the rest of the run and fall back to
    your receive-side heuristic. Don't retry-loop it (rule 8 applies).

## Spot-check checklist

Watch one client session (server logs + a packet trace or client debug log) and
tick these off:

| # | Check | Pass looks like |
|---|-------|-----------------|
| 1 | `/now` cadence | fixed, ≥2 s apart; no bursts; no sub-second gaps ever |
| 2 | Cache-window polls | client renders identical-`ts` snapshots without re-drawing or complaining |
| 3 | Progress bar | advances smoothly *between* polls (interpolating), doesn't jump at each poll |
| 4 | After a command | no `/now` request within ~1 s of the command (the reply snapshot was used) |
| 5 | During throttling | UI keeps showing the last track through a 429 window; poll rate unchanged; no command retry storm |
| 6 | Covers | at most one `cover` request per distinct `album_id`+size per app run; none during steady `/now` polling |
| 7 | Cover sizes | only 64/300/640 ever requested |
| 8 | Search | one request per submitted query, not per keystroke; accents arrive intact |
| 9 | `queue/add` | one add + at most one follow-up `/queue` poll |
| 10 | `wake` | only after user intent, never automatic |
| 11 | Unknown keys | feeding the client an extra `x_test<TAB>1` line changes nothing |
| 12 | Errors | client switches on `error` code; `message` text nowhere in client logic |
| 13 | List jump | one `play/from` per jump; no `/next` chains; no end-of-track watchdog issuing plays |
| 14 | `/stream` | cadence ≥ the `/now` cadence; one feature-detect per launch; "waiting" UI only when `live` `0` ∧ `playing` ∧ `active` |

A quick server-side way to observe a client's request pattern (geomyidae logs
every selector):

```sh
kubectl -n gopher-spot logs deploy/gopher-server --since=10m \
  | grep 'serving' | awk '{print $1, $NF}'
```
