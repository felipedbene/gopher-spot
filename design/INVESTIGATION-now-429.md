# Investigation: `/spot/api/1/now` returns `spotify HTTP 429`

**Date:** 2026-07-05
**Scope:** why `/now` intermittently returns `error upstream / message spotify HTTP 429: Too many requests`.
**Method:** read-only — source, Kubernetes logs, deploy manifests, live probes. **No changes made.**
**Trigger:** the Casquinha (Mac OS 9) client sees frequent 429s while polling `/now` every 2 s.

---

## Verdict

The 429 is a **load-correlated Spotify Web-API rate-limit**, not a client bug and not a
permanent block. Three structural properties of the bridge turn even modest polling into
a rate-limit trip and then keep it lit:

- **R1** — every `/now` cache-miss costs **two** Spotify *player* API calls.
- **R2** — the `/now` micro-cache (1 s TTL, per-replica, 2 round-robin replicas, no session
  affinity) is **effectively never hit** by a real 2 s poll, so R1 fires on nearly every request.
- **R3** — the bridge has **no `Retry-After` handling / circuit breaker**, so during a 429
  window it keeps calling Spotify, prolonging (and potentially escalating) the penalty.

---

## Evidence

### E1 — the 429 is transient / load-correlated
A single probe at low load succeeds; under sustained polling it 429s.

```
$ printf '/spot/api/1/now\r\n' | nc 10.0.100.112 70
api	1
state	stopped
device	idle
queue_len	0
ts	1783284970907          <- valid /now, NO 429, at this instant
```

Earlier, while the app was polling: `api 1 / error upstream / message spotify HTTP 429:
Too many requests`. => a rolling-window limit that clears when idle and trips under load.

### E2 — current load is *light*, yet it still trips under polling
Kubernetes logs, both `gopher-server` replicas, last hour:

```
 111 serving] /spot/api/1/now      <- ~1.9 /min
   4 serving] /spot/api/1/play
   2 serving] /spot/stream.pls
 117 |192.168.210.238|             <- ALL from one IP (the OS 9 VM); no other clients
```

So the trip threshold is low relative to what a 2 s poll generates in bursts — pointing at
a *per-request multiplier*, not raw client count.

### R1 — `/now` makes two Spotify player calls per miss  (`src/api.rs:339`)

```rust
fn now_document(api: &dyn SpotifyApi, now_ms: i64) -> String {
    if let Some(cached) = api.cached_now(now_ms) { return cached; }
    let playing = match api.now_playing() {                 // call #1
        Ok(p) => p,
        Err(e) => return error("upstream", &e),             // errors NOT cached
    };
    let queue_len = api.queue().map(|q| q.len()).unwrap_or(0);   // call #2 (unconditional)
    let doc = snapshot(&playing, queue_len, now_ms);
    api.store_now(now_ms, &doc);
    doc
}
```

- `now_playing()` -> `GET /v1/me/player` (`src/spotify.rs:553`).
- `queue()` -> `GET /v1/me/player/queue` (`src/spotify.rs:567`), **not** cached (calls
  `self.get(...)` directly).
- Both are Spotify **player-state** endpoints, rate-limited more aggressively than catalog.
- **Errors are never cached** (the `Err(e) => return error(...)` path stores nothing), so
  during a 429 every poll re-issues both calls.

### R2 — the micro-cache cannot serve a real poll
- TTL: `const NOW_CACHE_TTL_MS: i64 = 1_000;` (`src/spotify.rs:314`).
- Storage: `cache::get/put(&self.state_dir, "now_snapshot", ...)` — a **file cache in a
  per-request CGI process**, commented **"Per-replica, like every other cache entry."**
  (`src/spotify.rs:839-850`; `src/cache.rs:1-7`: geomyidae spawns a fresh process per
  request, so the cache is realized on disk).
- Topology: `replicas: 2` (`deploy/gopher-server.yaml:29`); Service is `type: LoadBalancer`
  with **no `sessionAffinity`** (`deploy/gopher-server.yaml:104-118`, field absent => default
  `None` = round-robin).
- **Consequence:** a client polling every 2 s is round-robined across 2 replicas, so each
  replica sees that client roughly every ~4 s — always **> the 1 s TTL** => **cache miss
  every time**. The 1 s cache only helps when *multiple* clients hit the *same* replica
  within the same second; it does nothing for a single steady poller.

**Call-rate math:** effective ≈ **2 Spotify player calls per `/now` poll**. At a 2 s cadence
that is **~60 calls/min per client**; with the sibling clients (DeToca/DeGelato) or probe
bursts, 120-180/min — squarely into Spotify player rate-limit territory.

### R3 — no `Retry-After` handling / no circuit breaker  (`src/spotify.rs:862`)

```rust
fn api_err(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(160).collect();
            format!("spotify HTTP {code}: {snippet}")   // 429 -> a string; Retry-After discarded
        }
        ureq::Error::Transport(t) => format!("spotify transport error: {t}"),
    }
}
```

Repo-wide grep for `retry.after | circuit | breaker | rate.?limit | cooldown` and any
429-handling -> **no matches** in `src/`. On a 429 the bridge **keeps calling Spotify on
every subsequent `/now`** (2 calls each), which prevents the rolling window from draining
and can escalate `Retry-After`.

### E4 — the team already flagged a *related* 429 vector
`AUDIT-2026-07.md` **GS-07 (P3, `spotify.rs:422`)**: token refresh has **no single-flight**;
on token expiry a burst of concurrent `/now` (2 replicas x per-request processes) each miss
and POST the token endpoint independently — *"O endpoint de token é rate-limited; uma
tempestade pode tomar 429."* This is a **third** call that can appear on a `/now` miss (token
refresh) and its own 429 source, confirming 429s are a known structural risk.

---

## Chain of causation

```
2 s client poll
  -> R2: per-replica 1 s cache never warm for that client   -> cache MISS every poll
    -> R1: each miss = GET /v1/me/player + GET /v1/me/player/queue  (+ token POST on expiry, GS-07)
      -> ~60 Spotify player calls/min/client -> exceeds Spotify's rolling player limit -> 429
        -> R3: bridge ignores Retry-After, keeps calling on every /now during the window
          -> window never drains; 429 persists under load, clears only when polling stops (E1)
```

---

## Confidence & limits

- **R1, R2, R3: high** — direct from source, config, and logs cited above.
- **Exact Spotify limit / `Retry-After` value: unmeasured** — the bridge discards
  `Retry-After` (R3), so its length is not observable from here; E1 only shows it *does*
  clear at low load.
- **Not examined:** whether the human `/spot` dcgi menu also calls `now_playing()` (another
  potential source if anything browses it). Logs show only the machine-API IP in this
  window, so it is not a current contributor.

---

## Where the leverage is (diagnosis only — no fix applied)

Purely as a map of the amplifiers identified above, for whoever picks this up:

1. **R3 is the one that makes it *stick*** — without honoring `Retry-After`, the window
   cannot drain while any client polls.
2. **R1** — `queue_len` costs a second, uncached player call on every `/now`.
3. **R2** — a 1 s per-replica cache under a round-robin LB is unreachable by a 2 s poll;
   the effective hit rate for a single steady client is ~0.

This document is a read-only investigation. No code, config, or deployment was changed.
