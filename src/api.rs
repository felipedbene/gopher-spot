//! The machine API — a frozen, versioned contract at `/spot/api/1/*` for the
//! DeToca client (fio S1).
//!
//! Unlike the human menus (gophermaps in PT-BR served by a `.dcgi`), every API
//! endpoint is a plain **type-0 text document**: `key<TAB>value` lines, UTF-8,
//! one per line, CRLF-terminated. It is served **raw** by geomyidae through
//! `/srv/spot/api/index.cgi` — a `.cgi`, so geomyidae pipes stdout to the socket
//! verbatim (no gph interpretation), which is the only way tabs and non-Latin-1
//! bytes survive (a `.dcgi` would mangle both). `main.rs` emits these bytes
//! without the Latin-1 transcode the human menus get.
//!
//! Contract rules (see `API.md`):
//!   - Keys are ASCII/snake_case and never localized; values are Spotify's own
//!     strings, verbatim.
//!   - `/spot/api/1` is frozen: additive changes (new keys) stay in v1; breaking
//!     changes go to `/spot/api/2`.
//!   - Clients MUST ignore unknown keys.
//!   - Commands are idempotent where it makes sense (`play` while already playing
//!     returns a snapshot, not an error) and reply with a fresh `/now` snapshot so
//!     the client leaves with current state in one round-trip.

use crate::dcgi::DcgiArgs;
use crate::spotify::{Control, Playing, PlaylistsPage, SpotifyApi, Track, TracksPage};

/// The contract version, emitted as the leading `api` key on every response.
pub const API_VERSION: u32 = 1;

/// The canonical cover sizes the client may request. They map to the sizes
/// Spotify's CDN serves (`640/300/64` px); anything else is `bad_range`.
const COVER_SIZES: [u32; 3] = [64, 300, 640];

/// Route a `/spot/api/1/*` request to its response bytes. Almost every endpoint
/// is a tab-delimited UTF-8 text document (returned as bytes here so the raw
/// `.cgi` writes it verbatim); `cover` is the one binary endpoint, returning raw
/// JPEG on success and a v1 text error otherwise. `now_ms` is the dcgi's request
/// wall-clock (unix epoch ms), stamped as `ts` so the client can interpolate the
/// progress bar between polls.
pub fn route(path: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>, now_ms: i64) -> Vec<u8> {
    // Only v1 exists. Everything else is a versioned 404 (a future /spot/api/2
    // would be routed here too).
    let sub = match path.strip_prefix("/spot/api/1") {
        Some(s) => s.trim_matches('/'),
        None => return error("not_found", "unknown api version").into_bytes(),
    };
    // No OAuth Secret configured -> the whole upstream is unavailable. Report it
    // in-contract rather than serving a human mock menu.
    let api = match api {
        Some(a) => a,
        None => return error("upstream", "spotify api not configured").into_bytes(),
    };
    // Cover owns the byte path: JPEG on success, encoded text error on failure.
    if sub == "cover" || sub.starts_with("cover/") {
        return cover(api, sub.strip_prefix("cover/").unwrap_or(""));
    }
    let text = match sub {
        "now" => now_document(api, now_ms),
        "play" => command(api, now_ms, Control::Resume),
        "pause" => command(api, now_ms, Control::Pause),
        "next" => command(api, now_ms, Control::Next),
        "prev" => command(api, now_ms, Control::Prev),
        "volume" => volume(api, args, now_ms),
        "seek" => seek(api, args, now_ms),
        "queue" => queue_doc(api, now_ms),
        "queue/add" => queue_add(api, args, now_ms),
        "wake" => wake(api, args, now_ms),
        "search" => search_doc(api, args, now_ms),
        "playlists" => playlists_doc(api, args, now_ms),
        s if s.starts_with("playlists/") => {
            playlist_tracks_doc(api, args, &s["playlists/".len()..], now_ms)
        }
        other => error("not_found", &format!("unknown endpoint: {other}")),
    };
    text.into_bytes()
}

/// `/queue`: the upcoming tracks as indexed `item.<i>.*` keys, in play order.
fn queue_doc(api: &dyn SpotifyApi, now_ms: i64) -> String {
    match api.queue() {
        Ok(items) => queue_snapshot(&items, now_ms),
        Err(e) => upstream(&e),
    }
}

/// `/queue/add?<uri>`: enqueue a track, then return the fresh `/queue` snapshot
/// (what the client's playlist redraws). Non-track / malformed uri -> `bad_uri`.
/// Like every command this is eventually consistent: the returned snapshot may
/// not yet contain the item (~1-2 s), so the client re-polls.
fn queue_add(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let uri = args.raw_arg();
    let uri = uri.trim();
    if !is_track_uri(uri) {
        return error("bad_uri", "queue/add needs a spotify:track:<id> uri");
    }
    match api.queue_add(uri) {
        // The queue changed, so both the cached queue body and a cached /now
        // (with its stale queue_len) are now wrong — bust both, then return the
        // fresh /queue snapshot (fio S3/2).
        Ok(()) => {
            api.invalidate_queue_cache();
            api.invalidate_now_cache();
            queue_doc(api, now_ms)
        }
        Err(e) => upstream(&e),
    }
}

/// `/search?q=<urlencoded>`: track results as a v1 list (same `item.<i>.*` shape
/// as `/queue`), with `result_len` as the count header. Tracks only for now.
/// Empty/absent `q` -> `bad_query`. The query is UTF-8 (accents decode correctly —
/// see the dcgi urldecode). NB: Spotify 400s `limit>10` on /v1/search (a
/// documented quirk), so results are capped at 10 — the reused `search()` limit.
fn search_doc(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let q = args.query("q").unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return error("bad_query", "search needs a non-empty q");
    }
    match api.search(q) {
        Ok(r) => {
            let tracks = r.tracks.as_ref().map(|p| p.items.as_slice()).unwrap_or(&[]);
            search_snapshot(tracks, now_ms)
        }
        Err(e) => upstream(&e),
    }
}

/// The `/search` document: `result_len` then one indexed `item.<i>.*` block per
/// track, in Spotify's relevance order. Same item shape as `/queue`.
fn search_snapshot(tracks: &[Track], now_ms: i64) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "result_len", &tracks.len().to_string());
    for (i, t) in tracks.iter().enumerate() {
        push_item(&mut out, i, t);
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// `/playlists`: the user's playlists as an indexed list (`item.<i>.{id,name,
/// tracks_len}`), paginated via `?offset=`. `total`/`offset` let the client page.
/// Only playlists that carry an id are emitted (a client needs it to open one).
fn playlists_doc(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match api.playlists(offset) {
        Ok(p) => playlists_snapshot(&p, now_ms),
        Err(e) => upstream(&e),
    }
}

fn playlists_snapshot(p: &PlaylistsPage, now_ms: i64) -> String {
    let items: Vec<&crate::spotify::Playlist> =
        p.items.iter().filter(|pl| pl.id.is_some()).collect();
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "result_len", &items.len().to_string());
    kv(&mut out, "total", &p.total.to_string());
    kv(&mut out, "offset", &p.offset.to_string());
    for (i, pl) in items.iter().enumerate() {
        kv(&mut out, &format!("item.{i}.id"), pl.id.as_deref().unwrap());
        kv(&mut out, &format!("item.{i}.name"), &pl.name);
        kv(
            &mut out,
            &format!("item.{i}.tracks_len"),
            &pl.tracks.total.to_string(),
        );
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// `/playlists/<id>`: a playlist's tracks in the `/search` list shape, led by a
/// `name` header. Unknown id -> `not_found`; a playlist Spotify won't let this app
/// read -> `forbidden` (the Nov-2024 dev-mode block — see API.md). Paginated via
/// `?offset=`.
fn playlist_tracks_doc(api: &dyn SpotifyApi, args: &DcgiArgs, id: &str, now_ms: i64) -> String {
    let id = id.trim_end_matches('/');
    // GS-02: a non-base62 id must not be interpolated into the Web API path.
    if !crate::spotify::valid_id(id) {
        return error("not_found", "unknown playlist");
    }
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Name first: readable even where /tracks is blocked, and its 404 vs 403 is
    // how we tell an unknown id from a blocked-but-real one.
    let name = match api.playlist_name(id) {
        Ok(n) => n,
        Err(e) => return map_playlist_err(&e),
    };
    match api.playlist_tracks(id, offset) {
        Ok(t) => playlist_tracks_snapshot(&name, &t, now_ms),
        Err(e) => map_playlist_err(&e),
    }
}

/// Map a playlist upstream error to a v1 code: 404 -> `not_found` (unknown id),
/// 403 -> `forbidden` (Spotify blocks this app from reading it), else `upstream`.
fn map_playlist_err(e: &str) -> String {
    if e.contains("HTTP 404") {
        error("not_found", "unknown playlist")
    } else if e.contains("HTTP 403") {
        error(
            "forbidden",
            "spotify does not allow this app to read this playlist",
        )
    } else {
        upstream(e)
    }
}

fn playlist_tracks_snapshot(name: &str, t: &TracksPage, now_ms: i64) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "name", name);
    kv(&mut out, "result_len", &t.items.len().to_string());
    kv(&mut out, "total", &t.total.to_string());
    kv(&mut out, "offset", &t.offset.to_string());
    for (i, tr) in t.items.iter().enumerate() {
        push_item(&mut out, i, tr);
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// `/cover/<album_id>/<size>`: raw JPEG bytes. `size` outside {64,300,640} ->
/// `bad_range`; unknown album or an album with no cover -> `not_found`.
fn cover(api: &dyn SpotifyApi, rest: &str) -> Vec<u8> {
    let (album_id, size_s) = match rest.split_once('/') {
        Some((a, s)) => (a, s.trim_end_matches('/')),
        None => return error("bad_range", "cover path is <album_id>/<size>").into_bytes(),
    };
    let size = match size_s.parse::<u32>() {
        Ok(s) if COVER_SIZES.contains(&s) => s,
        _ => return error("bad_range", "size must be 64, 300 or 640").into_bytes(),
    };
    // Non-base62 ids (GS-02: `..`, `/`) never reach the Web API path — same
    // not_found a nonexistent album gets.
    if !crate::spotify::valid_id(album_id) {
        return error("not_found", "unknown album").into_bytes();
    }
    match api.album_cover(album_id, size) {
        Ok(bytes) => bytes,
        // A 404 from /v1/albums (unknown id) or an album with no images both mean
        // "no cover to serve" -> not_found; anything else is an upstream failure.
        Err(e) if e.contains("HTTP 404") || e.contains("no cover") => {
            error("not_found", "album cover not found").into_bytes()
        }
        Err(e) => upstream(&e).into_bytes(),
    }
}

/// A well-formed `spotify:track:<id>` uri (non-empty id, no trailing segment).
fn is_track_uri(uri: &str) -> bool {
    uri.starts_with("spotify:track:") && crate::spotify::id_from_uri(uri).is_some()
}

/// `/volume?<0-100>`: continuous. Out of range (or non-integer) -> `bad_range`.
fn volume(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    match args.raw_arg().trim().parse::<i64>() {
        Ok(v) if (0..=100).contains(&v) => match api.control(Control::Volume(v as u8)) {
            Ok(()) => fresh_now(api, now_ms),
            Err(e) => upstream(&e),
        },
        _ => error("bad_range", "volume must be an integer 0-100"),
    }
}

/// `/seek?<position_ms>`: clamped to `[0, duration_ms]` of the current track.
/// No track loaded -> `no_track`.
fn seek(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let pos = match args.raw_arg().trim().parse::<i64>() {
        Ok(p) if p >= 0 => p as u64,
        _ => {
            return error(
                "bad_range",
                "seek position_ms must be a non-negative integer",
            )
        }
    };
    let playing = match api.now_playing() {
        Ok(p) => p,
        Err(e) => return upstream(&e),
    };
    let duration = match playing.item.as_ref().map(|t| t.duration_ms) {
        Some(d) if d > 0 => d,
        _ => return error("no_track", "nothing playing to seek"),
    };
    match api.seek(pos.min(duration)) {
        Ok(()) => fresh_now(api, now_ms),
        Err(e) => upstream(&e),
    }
}

/// `/wake[?play=1]`: transfer playback to the gopher-spot device. `?play=1` also
/// resumes on transfer (the Web API's native flag); without it, playback is
/// transferred in whatever play/pause state it was. Returns a fresh `/now`
/// snapshot (convention). The gopher-spot device not being registered (librespot
/// down) -> `no_device`.
fn wake(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let play = args.query("play").as_deref() == Some("1");
    match api.wake(play) {
        // wake is a command: fresh_now busts the micro-cache so the transfer is
        // reflected immediately (fio S3/2 synergy).
        Ok(()) => fresh_now(api, now_ms),
        Err(e) if e.contains("no_device") => {
            error("no_device", "gopher-spot device is not registered")
        }
        Err(e) => upstream(&e),
    }
}

/// Run a play/pause/next/prev command, then reply with a fresh snapshot. For the
/// idempotent pair (`play`/`pause`), Spotify 403s "Restriction violated" when the
/// player is already in the requested state — swallow that and return the
/// snapshot, so `play` while playing is a no-op success (contract rule). The
/// probe is gated on the error actually being a 403: on a 429/5xx the answer is
/// already "no", and the extra player call would only feed the rate limiter.
fn command(api: &dyn SpotifyApi, now_ms: i64, cmd: Control) -> String {
    match api.control(cmd) {
        Ok(()) => {
            // Only next/prev can change the upcoming queue; play/pause/volume
            // keep the warm queue entry (its own 10s TTL covers drift).
            if matches!(cmd, Control::Next | Control::Prev) {
                api.invalidate_queue_cache();
            }
            fresh_now(api, now_ms)
        }
        Err(e) if e.contains("HTTP 403") && already_in_state(api, cmd) => fresh_now(api, now_ms),
        Err(e) => upstream(&e),
    }
}

/// True when a failed `play`/`pause` was actually a no-op because the player is
/// already in the target state (Spotify's idempotency 403).
fn already_in_state(api: &dyn SpotifyApi, cmd: Control) -> bool {
    let playing = match api.now_playing() {
        Ok(p) => p,
        Err(_) => return false,
    };
    match cmd {
        Control::Resume => playing.is_playing,
        Control::Pause => !playing.is_playing && playing.item.is_some(),
        _ => false,
    }
}

/// The `/now` document, served from the ~3s micro-cache (fio S3/2) when warm,
/// else fetched fresh and cached. Queue length is best-effort (never blocks the
/// snapshot on it), mirroring the human Now Playing. An error is never cached — a
/// transient upstream failure shouldn't stick for the whole TTL window — but a
/// rate-limit error (fio 429) degrades to the last good snapshot while one is
/// within its ~30s stale window, so a polling client keeps rendering through a
/// Spotify cooldown instead of blanking; past the window it surfaces as
/// `error rate_limited`.
fn now_document(api: &dyn SpotifyApi, now_ms: i64) -> String {
    if let Some(cached) = api.cached_now(now_ms) {
        return cached;
    }
    // Single-flight the miss path per replica (the GS-07 token pattern):
    // concurrent per-request processes that all miss serialize here and re-check
    // the cache after acquiring, so a poll burst costs one upstream fetch pair,
    // not one per process. Held (via drop at end of scope) across fetch + store.
    let _flight = api.now_fetch_lock();
    if let Some(cached) = api.cached_now(now_ms) {
        return cached;
    }
    let playing = match api.now_playing() {
        Ok(p) => p,
        Err(e) if e.starts_with(crate::spotify::RATE_LIMITED) => {
            return match api.stale_now(now_ms) {
                Some(doc) => doc,
                None => upstream(&e),
            }
        }
        Err(e) => return upstream(&e),
    };
    // No track loaded -> nothing upcoming worth a second player call for; emit
    // queue_len 0 (the key is documented best-effort) so an idle poller costs
    // one upstream call per miss instead of two.
    let queue_len = match playing.item {
        None => 0,
        Some(_) => api.queue().map(|q| q.len()).unwrap_or(0),
    };
    let doc = snapshot(&playing, queue_len, now_ms);
    api.store_now(now_ms, &doc);
    doc
}

/// A command's reply: bust the micro-cache (so this and every later `/now`
/// reflects the command instead of a pre-command snapshot), then return a fresh
/// `/now` document — which also reseeds the cache with post-command state.
fn fresh_now(api: &dyn SpotifyApi, now_ms: i64) -> String {
    api.invalidate_now_cache();
    now_document(api, now_ms)
}

/// The `/now` document. Metadata keys (`track`..`duration_ms`) appear only when a
/// track is loaded; `volume` only when the active device reports it. `state` is
/// always present, so a client keys off it first.
fn snapshot(p: &Playing, queue_len: usize, now_ms: i64) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    let state = match &p.item {
        None => "stopped",
        Some(_) if p.is_playing => "playing",
        Some(_) => "paused",
    };
    kv(&mut out, "state", state);
    if let Some(t) = &p.item {
        kv(&mut out, "track", &t.name);
        kv(&mut out, "artist", &t.artist_line());
        if let Some(album) = &t.album {
            kv(&mut out, "album", &album.name);
            // album_id lets the client fetch /cover/<album_id>/<size>. Only present
            // when the item carries an album uri (a client ignores unknown keys).
            if let Some(aid) = crate::spotify::id_from_uri(&album.uri) {
                kv(&mut out, "album_id", aid);
            }
        }
        if let Some(id) = &t.id {
            kv(&mut out, "track_id", id);
        }
        kv(&mut out, "position_ms", &p.progress_ms.to_string());
        kv(&mut out, "duration_ms", &t.duration_ms.to_string());
    }
    // device (fio S3/3): `active` iff the account's current player IS the
    // gopher-spot librespot device — the one the audio stream carries. Anything
    // else (playing on the phone, or no active device) is `idle`, the case the
    // `wake` endpoint recovers from. Always present; the client keys off it.
    let device = match &p.device {
        Some(d) if d.name == "gopher-spot" => "active",
        _ => "idle",
    };
    kv(&mut out, "device", device);
    if let Some(vol) = p.device.as_ref().and_then(|d| d.volume_percent) {
        kv(&mut out, "volume", &vol.to_string());
    }
    kv(&mut out, "queue_len", &queue_len.to_string());
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// The `/queue` document: `queue_len` then one indexed block per upcoming track,
/// `<i>` from 0 in play order. Keeps the "exactly one TAB per line" invariant by
/// flattening the list into `item.<i>.<field>` keys. `album_id` is omitted when
/// the item carries no album uri; empty queue is just `queue_len 0`.
fn queue_snapshot(items: &[Track], now_ms: i64) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "queue_len", &items.len().to_string());
    for (i, t) in items.iter().enumerate() {
        push_item(&mut out, i, t);
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// The shared `item.<i>.*` block for a track — used by both `/queue` and
/// `/search`: uri/track/artist, `album_id` when the album carries a uri, and
/// duration_ms. Keeps the "exactly one TAB per line" wire invariant.
fn push_item(out: &mut String, i: usize, t: &Track) {
    kv(out, &format!("item.{i}.uri"), &t.uri);
    kv(out, &format!("item.{i}.track"), &t.name);
    kv(out, &format!("item.{i}.artist"), &t.artist_line());
    if let Some(aid) = t
        .album
        .as_ref()
        .and_then(|a| crate::spotify::id_from_uri(&a.uri))
    {
        kv(out, &format!("item.{i}.album_id"), aid);
    }
    kv(
        out,
        &format!("item.{i}.duration_ms"),
        &t.duration_ms.to_string(),
    );
}

/// The error document for a failed upstream call. Almost always `error
/// upstream`, but a `rate_limited:`-sentinel error (fio 429: Spotify 429'd us
/// and the bridge is in its cooldown window) gets its own code so a client can
/// tell "Spotify is throttling, keep showing what you have" from a real
/// failure. The message stays fixed — the sentinel text is bridge-internal.
fn upstream(e: &str) -> String {
    if e.starts_with(crate::spotify::RATE_LIMITED) {
        error("rate_limited", "spotify is rate limiting; retry shortly")
    } else {
        error("upstream", e)
    }
}

/// An error document: `api` / `error <code>` / `message <english>`.
pub fn error(code: &str, message: &str) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "error", code);
    kv(&mut out, "message", message);
    out
}

/// Append one `key<TAB>value` line (CRLF). Any tab/newline in a value (which
/// would forge extra lines/keys) is neutralized to a space — the wire protocol's
/// only structural characters, so metadata can't corrupt the document.
fn kv(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('\t');
    for c in value.chars() {
        out.push(if c == '\t' || c == '\n' || c == '\r' {
            ' '
        } else {
            c
        });
    }
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::{Album, ApiError, Artist, Device, Page, SearchResults, Track};
    use crate::spotify::{AlbumDetail, AlbumsPage, Playlist, PlaylistTracksRef};
    use crate::spotify::{PlaylistsPage, TracksPage};
    use std::cell::RefCell;

    fn argv(selector: &str) -> Vec<String> {
        let (_sel, arguments) = selector.split_once('?').unwrap_or((selector, ""));
        vec![
            "".into(),
            arguments.into(),
            "10.0.100.9".into(),
            "70".into(),
            "".into(),
            selector.into(),
        ]
    }

    fn track(name: &str) -> Track {
        Track {
            name: name.into(),
            artists: vec![
                Artist {
                    name: "Chico Buarque".into(),
                    uri: String::new(),
                },
                Artist {
                    name: "MPB4".into(),
                    uri: String::new(),
                },
            ],
            album: Some(Album {
                name: "Construção".into(),
                uri: "spotify:album:al1".into(),
            }),
            id: Some("abc123".into()),
            uri: "spotify:track:abc123".into(),
            duration_ms: 380_000,
        }
    }

    /// A fake that records the last control/seek it received and can be told to
    /// fail control() (to exercise the idempotency + error paths).
    struct Fake {
        playing: Playing,
        /// `Some(err)` makes control() fail with that error (403 text for the
        /// idempotency path; other errors must NOT trigger the now-probe).
        control_err: Option<ApiError>,
        empty_queue: bool,
        last: RefCell<Option<Control>>,
        last_seek: RefCell<Option<u64>>,
        last_queued: RefCell<Option<String>>,
        // fio S3/2 micro-cache: count upstream now_playing fetches, and back the
        // cache with an in-memory (expiry_ms, doc) slot so a test can assert a
        // poll burst folds to one fetch and a command busts it.
        now_calls: RefCell<u32>,
        now_cache: RefCell<Option<(i64, String)>>,
        // Perf split: count upstream queue fetches and queue-cache busts, so
        // tests can assert which commands pay the queue refetch.
        queue_calls: RefCell<u32>,
        queue_busts: RefCell<u32>,
        // fio S3/3 wake: `no_device` simulates librespot being unregistered;
        // `last_wake` records the play flag the endpoint passed.
        no_device: bool,
        last_wake: RefCell<Option<bool>>,
        // fio 429: `now_fails` makes now_playing() return this error (e.g. the
        // rate_limited: sentinel); `stale_doc` is what stale_now() serves.
        now_fails: Option<ApiError>,
        stale_doc: Option<String>,
    }
    fn fake(playing: Playing) -> Fake {
        Fake {
            playing,
            control_err: None,
            empty_queue: false,
            last: RefCell::new(None),
            last_seek: RefCell::new(None),
            last_queued: RefCell::new(None),
            now_calls: RefCell::new(0),
            now_cache: RefCell::new(None),
            queue_calls: RefCell::new(0),
            queue_busts: RefCell::new(0),
            no_device: false,
            last_wake: RefCell::new(None),
            now_fails: None,
            stale_doc: None,
        }
    }
    /// The Spotify idempotency 403 the `control_fails` tests exercise.
    fn restriction_403() -> Option<ApiError> {
        Some("spotify HTTP 403: Restriction violated".into())
    }
    fn playing_track() -> Playing {
        Playing {
            is_playing: true,
            progress_ms: 42_000,
            item: Some(track("Construção")),
            device: Some(Device {
                id: Some("d1".into()),
                name: "gopher-spot".into(),
                is_active: true,
                volume_percent: Some(65),
            }),
        }
    }
    fn stopped() -> Playing {
        Playing {
            is_playing: false,
            progress_ms: 0,
            item: None,
            device: None,
        }
    }

    impl SpotifyApi for Fake {
        fn now_playing(&self) -> Result<Playing, ApiError> {
            *self.now_calls.borrow_mut() += 1;
            if let Some(e) = &self.now_fails {
                return Err(e.clone());
            }
            Ok(self.playing.clone())
        }
        fn stale_now(&self, _now_ms: i64) -> Option<String> {
            self.stale_doc.clone()
        }
        fn cached_now(&self, now_ms: i64) -> Option<String> {
            self.now_cache
                .borrow()
                .as_ref()
                .filter(|(exp, _)| now_ms < *exp)
                .map(|(_, d)| d.clone())
        }
        fn store_now(&self, now_ms: i64, doc: &str) {
            *self.now_cache.borrow_mut() = Some((now_ms + 1_000, doc.to_string()));
        }
        fn invalidate_now_cache(&self) {
            *self.now_cache.borrow_mut() = None;
        }
        fn queue(&self) -> Result<Vec<Track>, ApiError> {
            *self.queue_calls.borrow_mut() += 1;
            if self.empty_queue {
                return Ok(Vec::new());
            }
            Ok(vec![track("Deus lhe Pague"), track("Cotidiano")])
        }
        fn invalidate_queue_cache(&self) {
            *self.queue_busts.borrow_mut() += 1;
        }
        fn queue_add(&self, uri: &str) -> Result<(), ApiError> {
            *self.last_queued.borrow_mut() = Some(uri.to_string());
            Ok(())
        }
        fn album_cover(&self, album_id: &str, want_px: u32) -> Result<Vec<u8>, ApiError> {
            match album_id {
                "missing" => Err("spotify HTTP 404: not found".into()),
                "noimg" => Err("no cover image for album noimg".into()),
                // Bytes that are deliberately NOT valid UTF-8 (0xFF) and encode the
                // requested size, so the routing/verbatim tests can assert on them.
                _ => Ok(vec![0xFF, 0xD8, 0xFF, want_px as u8, 0x00, 0xFF, 0xD9]),
            }
        }
        fn control(&self, cmd: Control) -> Result<(), ApiError> {
            *self.last.borrow_mut() = Some(cmd);
            match &self.control_err {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn seek(&self, position_ms: u64) -> Result<(), ApiError> {
            *self.last_seek.borrow_mut() = Some(position_ms);
            Ok(())
        }
        fn search(&self, q: &str) -> Result<SearchResults, ApiError> {
            // Echo the (decoded) query into the track names so a test can assert
            // the API search path decoded UTF-8 correctly end-to-end.
            Ok(SearchResults {
                tracks: Some(Page {
                    items: vec![track(&format!("hit {q} A")), track(&format!("hit {q} B"))],
                }),
                artists: None,
                albums: None,
            })
        }
        fn track(&self, _id: &str) -> Result<Track, ApiError> {
            unimplemented!()
        }
        fn play(&self, _uri: &str) -> Result<(), ApiError> {
            unimplemented!()
        }
        fn wake(&self, play: bool) -> Result<(), ApiError> {
            if self.no_device {
                return Err("no_device: 'gopher-spot' is not registered".into());
            }
            *self.last_wake.borrow_mut() = Some(play);
            Ok(())
        }
        fn play_context(&self, _c: &str, _o: u32) -> Result<(), ApiError> {
            Ok(())
        }
        fn playlists(&self, offset: u32) -> Result<PlaylistsPage, ApiError> {
            Ok(PlaylistsPage {
                items: vec![
                    Playlist {
                        id: Some("pl1".into()),
                        name: "Sambas".into(),
                        tracks: PlaylistTracksRef { total: 12 },
                    },
                    Playlist {
                        id: Some("pl2".into()),
                        name: "MPB".into(),
                        tracks: PlaylistTracksRef { total: 40 },
                    },
                    // No id -> must be filtered out of the doc.
                    Playlist {
                        id: None,
                        name: "sem id".into(),
                        tracks: PlaylistTracksRef::default(),
                    },
                ],
                total: 25,
                offset,
            })
        }
        fn playlist_tracks(&self, id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            if id == "blocked" {
                return Err("spotify HTTP 403: Forbidden".into());
            }
            Ok(TracksPage {
                items: vec![track("Faixa da playlist")],
                total: 1,
                offset,
            })
        }
        fn playlist_name(&self, id: &str) -> Result<String, ApiError> {
            match id {
                "ghost" => Err("spotify HTTP 404: not found".into()),
                _ => Ok(format!("Playlist {id}")),
            }
        }
        fn album(&self, _id: &str) -> Result<AlbumDetail, ApiError> {
            unimplemented!()
        }
        fn album_tracks(&self, _id: &str, _o: u32) -> Result<TracksPage, ApiError> {
            unimplemented!()
        }
        fn artist(&self, _id: &str) -> Result<Artist, ApiError> {
            unimplemented!()
        }
        fn artist_albums(&self, _id: &str, _o: u32) -> Result<AlbumsPage, ApiError> {
            unimplemented!()
        }
        fn artist_top_tracks(&self, _id: &str) -> Result<Vec<Track>, ApiError> {
            unimplemented!()
        }
    }

    const TS: i64 = 1_700_000_000_000;

    /// Most endpoints are UTF-8 text; decode the route bytes so the existing
    /// `contains` assertions read naturally. The one binary endpoint (`cover`) is
    /// tested against the raw `route` bytes instead.
    fn call(f: &Fake, selector: &str) -> String {
        let args = DcgiArgs::from_argv(&argv(selector));
        String::from_utf8(route(&args.path(), &args, Some(f), TS)).unwrap()
    }
    fn call_bytes(f: &Fake, selector: &str) -> Vec<u8> {
        let args = DcgiArgs::from_argv(&argv(selector));
        route(&args.path(), &args, Some(f), TS)
    }

    /// Every response is CRLF-terminated `key<TAB>value` with exactly one tab per
    /// line — the wire invariant the raw `.cgi` relies on.
    fn assert_wire(doc: &str) {
        for line in doc.split("\r\n").filter(|l| !l.is_empty()) {
            assert_eq!(
                line.matches('\t').count(),
                1,
                "not exactly one TAB: {line:?}"
            );
        }
        assert!(doc.ends_with("\r\n"), "must be CRLF-terminated: {doc:?}");
    }

    #[test]
    fn now_snapshot_has_all_contract_keys() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/now");
        assert_wire(&out);
        assert!(out.contains("api\t1\r\n"));
        assert!(out.contains("state\tplaying\r\n"));
        assert!(out.contains("track\tConstrução\r\n"));
        assert!(out.contains("artist\tChico Buarque, MPB4\r\n"));
        assert!(out.contains("album\tConstrução\r\n"));
        assert!(out.contains("album_id\tal1\r\n"));
        assert!(out.contains("track_id\tabc123\r\n"));
        assert!(out.contains("position_ms\t42000\r\n"));
        assert!(out.contains("duration_ms\t380000\r\n"));
        assert!(out.contains("device\tactive\r\n")); // active device is gopher-spot
        assert!(out.contains("volume\t65\r\n"));
        assert!(out.contains("queue_len\t2\r\n"));
        assert!(out.contains(&format!("ts\t{TS}\r\n")));
    }

    #[test]
    fn stopped_state_omits_track_keys() {
        let f = fake(stopped());
        let out = call(&f, "/spot/api/1/now");
        assert_wire(&out);
        assert!(out.contains("state\tstopped\r\n"));
        assert!(!out.contains("track\t"));
        assert!(out.contains("device\tidle\r\n")); // no active device -> idle
        assert!(!out.contains("volume\t")); // no device
        assert!(out.contains("queue_len\t"));
        assert!(out.contains("ts\t"));
    }

    #[test]
    fn paused_state() {
        let mut p = playing_track();
        p.is_playing = false;
        let out = call(&fake(p), "/spot/api/1/now");
        assert!(out.contains("state\tpaused\r\n"));
    }

    #[test]
    fn commands_execute_and_return_snapshot() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/pause");
        assert_eq!(*f.last.borrow(), Some(Control::Pause));
        assert!(out.contains("state\tplaying\r\n")); // snapshot after
        assert_eq!(*fake(playing_track()).last.borrow(), None);
        let f2 = fake(playing_track());
        call(&f2, "/spot/api/1/next");
        assert_eq!(*f2.last.borrow(), Some(Control::Next));
    }

    #[test]
    fn play_while_playing_is_idempotent_not_error() {
        // control() 403s "Restriction violated"; player already playing -> no error.
        let f = Fake {
            control_err: restriction_403(),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/play");
        assert!(
            !out.contains("error\t"),
            "idempotent play must not error: {out}"
        );
        assert!(out.contains("state\tplaying\r\n"));
    }

    #[test]
    fn next_failure_surfaces_upstream_error() {
        // next is not idempotent-swallowed -> a control failure is a real error.
        let f = Fake {
            control_err: restriction_403(),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/next");
        assert!(out.contains("error\tupstream\r\n"));
    }

    #[test]
    fn volume_in_range_sets_and_snapshots() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/volume?70");
        assert_eq!(*f.last.borrow(), Some(Control::Volume(70)));
        assert!(out.contains("state\tplaying\r\n"));
    }

    #[test]
    fn volume_out_of_range_is_bad_range() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/volume?150").contains("error\tbad_range\r\n"));
        assert!(call(&f, "/spot/api/1/volume?-5").contains("error\tbad_range\r\n"));
        assert!(call(&f, "/spot/api/1/volume?abc").contains("error\tbad_range\r\n"));
        assert!(call(&f, "/spot/api/1/volume?").contains("error\tbad_range\r\n"));
        // Boundaries are valid.
        assert!(!call(&f, "/spot/api/1/volume?0").contains("error\t"));
        assert!(!call(&f, "/spot/api/1/volume?100").contains("error\t"));
    }

    #[test]
    fn seek_clamps_to_duration() {
        let f = fake(playing_track()); // duration 380000
        call(&f, "/spot/api/1/seek?999999");
        assert_eq!(*f.last_seek.borrow(), Some(380_000));
        call(&f, "/spot/api/1/seek?10000");
        assert_eq!(*f.last_seek.borrow(), Some(10_000));
    }

    #[test]
    fn seek_without_track_is_no_track() {
        let f = fake(stopped());
        assert!(call(&f, "/spot/api/1/seek?1000").contains("error\tno_track\r\n"));
    }

    #[test]
    fn seek_bad_value_is_bad_range() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/seek?-1").contains("error\tbad_range\r\n"));
        assert!(call(&f, "/spot/api/1/seek?xyz").contains("error\tbad_range\r\n"));
    }

    #[test]
    fn unknown_endpoint_is_not_found() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/bogus").contains("error\tnot_found\r\n"));
        assert!(call(&f, "/spot/api/2/now").contains("error\tnot_found\r\n"));
    }

    #[test]
    fn no_api_reports_upstream_not_error_free() {
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/now"));
        let out = String::from_utf8(route(&args.path(), &args, None, TS)).unwrap();
        assert!(out.contains("error\tupstream\r\n"));
    }

    #[test]
    fn tab_in_metadata_is_neutralized() {
        let mut p = playing_track();
        if let Some(t) = p.item.as_mut() {
            t.name = "a\tb\nc".into();
        }
        let out = call(&fake(p), "/spot/api/1/now");
        assert_wire(&out); // still exactly one tab per line
        assert!(out.contains("track\ta b c\r\n"));
    }

    // ---- fio S2: queue --------------------------------------------------------

    #[test]
    fn queue_lists_indexed_items() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/queue");
        assert_wire(&out);
        assert!(out.contains("api\t1\r\n"));
        assert!(out.contains("queue_len\t2\r\n"));
        // Fake queue is two tracks, in play order, with all per-item keys.
        assert!(out.contains("item.0.uri\tspotify:track:abc123\r\n"));
        assert!(out.contains("item.0.track\tDeus lhe Pague\r\n"));
        assert!(out.contains("item.0.artist\tChico Buarque, MPB4\r\n"));
        assert!(out.contains("item.0.album_id\tal1\r\n"));
        assert!(out.contains("item.0.duration_ms\t380000\r\n"));
        assert!(out.contains("item.1.track\tCotidiano\r\n"));
        assert!(out.contains(&format!("ts\t{TS}\r\n")));
    }

    #[test]
    fn empty_queue_is_len_zero_no_items() {
        let f = Fake {
            empty_queue: true,
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/queue");
        assert_wire(&out);
        assert!(out.contains("queue_len\t0\r\n"));
        assert!(!out.contains("item."));
    }

    // ---- fio S2: queue/add ----------------------------------------------------

    #[test]
    fn queue_add_enqueues_and_returns_queue_snapshot() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/queue/add?spotify:track:xyz");
        assert_eq!(*f.last_queued.borrow(), Some("spotify:track:xyz".into()));
        // Returns the /queue snapshot (not /now): queue_len + items, no `state`.
        assert!(out.contains("queue_len\t2\r\n"));
        assert!(!out.contains("state\t"));
    }

    #[test]
    fn queue_add_rejects_non_track_uri() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/queue/add?spotify:album:al1").contains("error\tbad_uri\r\n"));
        assert!(call(&f, "/spot/api/1/queue/add?garbage").contains("error\tbad_uri\r\n"));
        assert!(call(&f, "/spot/api/1/queue/add?").contains("error\tbad_uri\r\n"));
        assert!(call(&f, "/spot/api/1/queue/add?spotify:track:").contains("error\tbad_uri\r\n"));
        // A valid track uri must NOT be rejected.
        assert!(!call(&f, "/spot/api/1/queue/add?spotify:track:ok").contains("error\t"));
    }

    // ---- fio S2: cover --------------------------------------------------------

    #[test]
    fn cover_returns_jpeg_bytes_verbatim() {
        let f = fake(playing_track());
        // The fake encodes the requested size in byte[3]; bytes come back verbatim,
        // including the non-UTF-8 0xFF markers.
        assert_eq!(
            call_bytes(&f, "/spot/api/1/cover/al1/64"),
            vec![0xFF, 0xD8, 0xFF, 64, 0x00, 0xFF, 0xD9]
        );
        assert_eq!(call_bytes(&f, "/spot/api/1/cover/al1/300")[3], 300u32 as u8);
        assert_eq!(call_bytes(&f, "/spot/api/1/cover/al1/640")[3], 640u32 as u8);
    }

    #[test]
    fn cover_bad_size_is_bad_range() {
        let f = fake(playing_track());
        for sel in [
            "/spot/api/1/cover/al1/128",
            "/spot/api/1/cover/al1/0",
            "/spot/api/1/cover/al1/abc",
            "/spot/api/1/cover/al1", // missing size segment
        ] {
            let out = String::from_utf8(call_bytes(&f, sel)).unwrap();
            assert!(out.contains("error\tbad_range\r\n"), "{sel}: {out}");
        }
    }

    #[test]
    fn non_base62_ids_are_not_found_at_the_api_edge() {
        // GS-02: dotted ids must be rejected before reaching a Web API path,
        // where dot-normalization would redirect the authenticated GET.
        let f = fake(playing_track());
        let cover = String::from_utf8(call_bytes(&f, "/spot/api/1/cover/../300")).unwrap();
        assert!(cover.contains("error\tnot_found\r\n"), "{cover}");
        let pl = call(&f, "/spot/api/1/playlists/..");
        assert!(pl.contains("error\tnot_found\r\n"), "{pl}");
    }

    #[test]
    fn cover_unknown_album_is_not_found() {
        let f = fake(playing_track());
        let missing = String::from_utf8(call_bytes(&f, "/spot/api/1/cover/missing/300")).unwrap();
        assert!(missing.contains("error\tnot_found\r\n"));
        let noimg = String::from_utf8(call_bytes(&f, "/spot/api/1/cover/noimg/300")).unwrap();
        assert!(noimg.contains("error\tnot_found\r\n"));
    }

    // ---- fio S3/2: /now micro-cache -------------------------------------------

    /// Poll `/now` at an explicit wall-clock (ms), returning the document.
    fn now_at(f: &Fake, now_ms: i64) -> String {
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/now"));
        String::from_utf8(route(&args.path(), &args, Some(f), now_ms)).unwrap()
    }

    #[test]
    fn now_polls_within_ttl_hit_cache_once_with_original_ts() {
        let f = fake(playing_track());
        let a = now_at(&f, TS); // miss -> fetch + store
        let b = now_at(&f, TS + 500); // within 1s -> cache hit
        let c = now_at(&f, TS + 999); // still within -> cache hit
        assert_eq!(*f.now_calls.borrow(), 1, "three polls, one upstream fetch");
        // Every poll in the window carries the ORIGINAL ts, so the client's
        // interpolation absorbs the staleness.
        assert!(a.contains(&format!("ts\t{TS}\r\n")));
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn now_cache_expires_after_ttl() {
        let f = fake(playing_track());
        now_at(&f, TS); // fetch #1
        now_at(&f, TS + 1_000); // TTL boundary (now_ms >= expiry) -> fetch #2
        assert_eq!(*f.now_calls.borrow(), 2);
    }

    #[test]
    fn command_busts_now_cache_within_window() {
        let f = fake(playing_track());
        now_at(&f, TS); // fetch #1, caches until TS+1000
                        // A command mid-window must invalidate: without the bust, `next`'s reply
                        // would read the still-valid cached snapshot (no fetch) -> count 1.
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/next"));
        route(&args.path(), &args, Some(&f), TS + 100);
        assert_eq!(*f.now_calls.borrow(), 2, "command forced a fresh snapshot");
        // The command reseeded the cache at TS+100, so a poll at TS+200 is a hit.
        now_at(&f, TS + 200);
        assert_eq!(*f.now_calls.borrow(), 2);
    }

    #[test]
    fn queue_add_busts_now_cache() {
        let f = fake(playing_track());
        now_at(&f, TS); // caches /now
        call(&f, "/spot/api/1/queue/add?spotify:track:xyz"); // must invalidate
        now_at(&f, TS + 100); // would be a stale hit if not invalidated -> fetch
        assert_eq!(*f.now_calls.borrow(), 2);
    }

    // ---- fio 429: rate-limit degradation ---------------------------------------

    #[test]
    fn now_serves_stale_snapshot_while_rate_limited() {
        let mut f = fake(playing_track());
        f.now_fails = Some("rate_limited: spotify HTTP 429 (retry after 5s)".into());
        f.stale_doc = Some("api\t1\r\nstate\tplaying\r\nts\t123\r\n".into());
        let out = call(&f, "/spot/api/1/now");
        // The last good snapshot, verbatim (old ts and all) — not an error doc.
        assert_eq!(out, "api\t1\r\nstate\tplaying\r\nts\t123\r\n");
    }

    #[test]
    fn now_rate_limited_without_stale_is_rate_limited_error() {
        let mut f = fake(playing_track());
        f.now_fails = Some("rate_limited: spotify cooldown active (until ~999)".into());
        let out = call(&f, "/spot/api/1/now");
        assert_wire(&out);
        assert!(out.contains("error\trate_limited\r\n"), "got: {out}");
        // The sentinel text is bridge-internal; the message is the fixed one.
        assert!(!out.contains("rate_limited:"), "sentinel leaked: {out}");
    }

    #[test]
    fn now_non_rate_limit_error_is_still_upstream() {
        let mut f = fake(playing_track());
        f.now_fails = Some("spotify HTTP 500: boom".into());
        f.stale_doc = Some("api\t1\r\nstate\tplaying\r\nts\t123\r\n".into());
        let out = call(&f, "/spot/api/1/now");
        // A non-429 failure must NOT degrade to the stale snapshot.
        assert!(out.contains("error\tupstream\r\n"), "got: {out}");
    }

    #[test]
    fn upstream_mapper_keys_off_the_sentinel_prefix() {
        // Every endpoint funnels upstream failures through upstream(): the
        // sentinel becomes its own error code, anything else stays `upstream`.
        assert!(upstream("rate_limited: spotify HTTP 429 (retry after 5s)")
            .contains("error\trate_limited\r\n"));
        assert!(upstream("spotify HTTP 500: boom").contains("error\tupstream\r\n"));
        // Only the PREFIX triggers it — a 500 whose body happens to mention
        // rate limits must not be misclassified.
        assert!(upstream("spotify HTTP 500: rate_limited: nope")
            .contains("error\tupstream\r\n"));
    }

    // ---- perf fio: fewer upstream calls -----------------------------------------

    #[test]
    fn stopped_now_skips_the_queue_call() {
        // No track loaded -> /now must not pay the second player call; the
        // best-effort queue_len is emitted as 0.
        let f = fake(stopped());
        let out = call(&f, "/spot/api/1/now");
        assert!(out.contains("queue_len\t0\r\n"), "{out}");
        assert_eq!(*f.queue_calls.borrow(), 0, "queue() must not be called");
    }

    #[test]
    fn only_queue_changing_commands_bust_the_queue_cache() {
        // pause/volume/seek/wake can't change the queue -> keep the warm entry.
        let f = fake(playing_track());
        call(&f, "/spot/api/1/pause");
        call(&f, "/spot/api/1/volume?50");
        call(&f, "/spot/api/1/seek?1000");
        call(&f, "/spot/api/1/wake");
        assert_eq!(*f.queue_busts.borrow(), 0, "non-queue commands must not bust");
        // next/prev/queue_add do change it -> one bust each.
        call(&f, "/spot/api/1/next");
        assert_eq!(*f.queue_busts.borrow(), 1);
        call(&f, "/spot/api/1/prev");
        assert_eq!(*f.queue_busts.borrow(), 2);
        call(&f, "/spot/api/1/queue/add?spotify:track:xyz");
        assert_eq!(*f.queue_busts.borrow(), 3);
    }

    #[test]
    fn non_403_control_error_skips_the_now_probe() {
        // A rate-limited (or 5xx) control failure already answers the
        // idempotency question — probing now_playing would only add a player
        // call while Spotify is throttling.
        let f = Fake {
            control_err: Some("rate_limited: spotify cooldown active (until ~9)".into()),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/play");
        assert!(out.contains("error\trate_limited\r\n"), "{out}");
        assert_eq!(*f.now_calls.borrow(), 0, "no probe on a non-403 failure");
    }

    // ---- fio S3/3: device + wake ----------------------------------------------

    #[test]
    fn device_idle_when_playing_elsewhere() {
        // Active device is the phone, not gopher-spot -> device idle (even though
        // a track is playing).
        let mut p = playing_track();
        p.device = Some(Device {
            id: Some("phone".into()),
            name: "iPhone".into(),
            is_active: true,
            volume_percent: Some(40),
        });
        let out = call(&fake(p), "/spot/api/1/now");
        assert!(out.contains("device\tidle\r\n"));
        assert!(out.contains("state\tplaying\r\n")); // still playing, just elsewhere
    }

    #[test]
    fn wake_transfers_without_play_and_returns_now() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/wake");
        assert_eq!(*f.last_wake.borrow(), Some(false)); // no ?play -> transfer only
        assert!(!out.contains("error\t"));
        assert!(out.contains("state\t")); // a /now snapshot, per convention
        assert!(out.contains("device\t"));
    }

    #[test]
    fn wake_play_1_resumes_on_transfer() {
        let f = fake(playing_track());
        call(&f, "/spot/api/1/wake?play=1");
        assert_eq!(*f.last_wake.borrow(), Some(true));
    }

    #[test]
    fn wake_no_device_is_no_device_error() {
        let f = Fake {
            no_device: true,
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/wake");
        assert!(out.contains("error\tno_device\r\n"));
    }

    #[test]
    fn wake_busts_now_cache() {
        let f = fake(playing_track());
        now_at(&f, TS); // caches /now
        call(&f, "/spot/api/1/wake"); // command -> must invalidate + refetch
        assert_eq!(*f.now_calls.borrow(), 2);
    }

    // ---- fio S3/4: search -----------------------------------------------------

    #[test]
    fn search_lists_tracks_with_result_len() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/search?q=chico");
        assert_wire(&out);
        assert!(out.contains("api\t1\r\n"));
        assert!(out.contains("result_len\t2\r\n"));
        assert!(out.contains("item.0.uri\tspotify:track:abc123\r\n"));
        assert!(out.contains("item.0.track\thit chico A\r\n"));
        assert!(out.contains("item.0.artist\tChico Buarque, MPB4\r\n"));
        assert!(out.contains("item.0.album_id\tal1\r\n"));
        assert!(out.contains("item.0.duration_ms\t380000\r\n"));
        assert!(out.contains("item.1.track\thit chico B\r\n"));
        assert!(out.contains(&format!("ts\t{TS}\r\n")));
    }

    #[test]
    fn search_decodes_utf8_query_end_to_end() {
        // %C3%A7%C3%A3o -> ç ã o. The fake echoes the decoded query into the track
        // name, so an intact accent here proves the API search decodes UTF-8.
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/search?q=constru%C3%A7%C3%A3o");
        assert!(out.contains("item.0.track\thit construção A\r\n"), "{out}");
    }

    #[test]
    fn search_empty_or_absent_q_is_bad_query() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/search?q=").contains("error\tbad_query\r\n"));
        assert!(call(&f, "/spot/api/1/search").contains("error\tbad_query\r\n"));
        // whitespace-only decodes to empty after trim
        assert!(call(&f, "/spot/api/1/search?q=%20%20").contains("error\tbad_query\r\n"));
    }

    // ---- fio S3/5: playlists --------------------------------------------------

    #[test]
    fn playlists_list_has_id_name_tracks_len() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/playlists");
        assert_wire(&out);
        assert!(out.contains("api\t1\r\n"));
        assert!(out.contains("result_len\t2\r\n")); // the id-less one is filtered
        assert!(out.contains("total\t25\r\n"));
        assert!(out.contains("offset\t0\r\n"));
        assert!(out.contains("item.0.id\tpl1\r\n"));
        assert!(out.contains("item.0.name\tSambas\r\n"));
        assert!(out.contains("item.0.tracks_len\t12\r\n"));
        assert!(out.contains("item.1.id\tpl2\r\n"));
        assert!(out.contains("item.1.tracks_len\t40\r\n"));
        assert!(!out.contains("sem id")); // id-less playlist omitted entirely
    }

    #[test]
    fn playlist_tracks_has_name_header_and_items() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/playlists/pl1");
        assert_wire(&out);
        assert!(out.contains("name\tPlaylist pl1\r\n")); // the header
        assert!(out.contains("result_len\t1\r\n"));
        assert!(out.contains("item.0.track\tFaixa da playlist\r\n"));
        assert!(out.contains("item.0.uri\tspotify:track:abc123\r\n"));
    }

    #[test]
    fn playlist_unknown_id_is_not_found() {
        // playlist_name 404s for "ghost" -> unknown id.
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/playlists/ghost").contains("error\tnot_found\r\n"));
    }

    #[test]
    fn playlist_blocked_by_spotify_is_forbidden() {
        // name readable (200) but /tracks 403 -> forbidden (the dev-mode block).
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/playlists/blocked");
        assert!(out.contains("error\tforbidden\r\n"), "{out}");
    }
}
