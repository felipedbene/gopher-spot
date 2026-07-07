//! The machine API — a frozen, versioned contract at `/spot/api/1/*` for the
//! native clients (DeToca, DeGelato, Casquinha; introduced for DeToca, fio S1).
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
use crate::spotify::{
    AlbumDetail, AlbumsPage, Control, Playing, PlaylistsPage, SearchResults, SpotifyApi, Track,
    TracksPage,
};
use crate::stream::{StreamFacts, StreamSource};

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
pub fn route(
    path: &str,
    args: &DcgiArgs,
    api: Option<&dyn SpotifyApi>,
    stream: Option<&dyn StreamSource>,
    now_ms: i64,
) -> Vec<u8> {
    // Only v1 exists. Everything else is a versioned 404 (a future /spot/api/2
    // would be routed here too).
    let sub = match path.strip_prefix("/spot/api/1") {
        Some(s) => s.trim_matches('/'),
        None => return error("not_found", "unknown api version").into_bytes(),
    };
    // The media plane's own state (fio A): answered from Icecast, not Spotify —
    // deliberately BEFORE the OAuth gate below, so /stream reports even when
    // the Web API is unconfigured.
    if sub == "stream" {
        return stream_doc(stream, now_ms).into_bytes();
    }
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
        // A NEW sub, not a query on `play`: `path()` strips the query, so an old
        // server would silently match `play?ids=…` to the resume arm above. As
        // its own selector, old servers answer `not_found` — the client's clean
        // feature-detect signal (Casquinha falls back to its b40 behavior).
        "play/from" => play_from(api, args, now_ms),
        // Play an album/artist/playlist as a CONTEXT (whole thing, in order,
        // auto-advancing). Its own sub, like play/from: an old server answers
        // not_found -> the client's feature-detect signal.
        "play/context" => play_context_doc(api, args, now_ms),
        "pause" => command(api, now_ms, Control::Pause),
        "next" => command(api, now_ms, Control::Next),
        "prev" => command(api, now_ms, Control::Prev),
        "volume" => volume(api, args, now_ms),
        "seek" => seek(api, args, now_ms),
        "queue" => queue_doc(api, now_ms),
        "queue/add" => queue_add(api, args, now_ms),
        // Enqueue a whole album's tracks (Spotify's queue takes one at a time, so
        // this is the server-side expansion the client can't express in one call).
        "queue/album" => queue_album_doc(api, args, now_ms),
        "wake" => wake(api, args, now_ms),
        "search" => search_doc(api, args, now_ms),
        "playlists" => playlists_doc(api, args, now_ms),
        s if s.starts_with("playlists/") => {
            playlist_tracks_doc(api, args, &s["playlists/".len()..], now_ms)
        }
        // An artist's discography: /spot/api/1/artist/<id>/albums (checked before
        // the bare album/ arm so the /albums suffix wins).
        s if s.starts_with("artist/") && s.ends_with("/albums") => {
            let id = &s["artist/".len()..s.len() - "/albums".len()];
            artist_albums_doc(api, args, id, now_ms)
        }
        // An album's header + tracks: /spot/api/1/album/<id>.
        s if s.starts_with("album/") => album_doc(api, args, &s["album/".len()..], now_ms),
        other => error("not_found", &format!("unknown endpoint: {other}")),
    };
    text.into_bytes()
}

/// `/stream`: the media plane's own state — whether a live source feeds the
/// `/spotify.mp3` mount, and how many external listeners hear it. `live 0`
/// while `/now` reports `state playing` + `device active` is the genuine
/// anomaly (librespot's chain lost the mount — the "waiting for Spotify" case
/// clients used to infer from rx dryness). Served from a ~2 s micro-cache;
/// Icecast unreachable/undecodable -> `error upstream` (never cached — same
/// law as `/now`).
fn stream_doc(src: Option<&dyn StreamSource>, now_ms: i64) -> String {
    let src = match src {
        Some(s) => s,
        None => return error("upstream", "stream status not configured"),
    };
    if let Some(doc) = src.cached_stream(now_ms) {
        return doc;
    }
    match src.stream_facts() {
        Ok(f) => {
            let doc = stream_snapshot(&f, now_ms);
            src.store_stream(now_ms, &doc);
            doc
        }
        Err(e) => error("upstream", &e),
    }
}

/// The `/stream` document: `live` + external `listeners` + `ts`.
fn stream_snapshot(f: &StreamFacts, now_ms: i64) -> String {
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "live", if f.live { "1" } else { "0" });
    kv(&mut out, "listeners", &f.listeners.to_string());
    kv(&mut out, "ts", &now_ms.to_string());
    out
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

/// Cap on tracks enqueued by one `queue/album`. Bounds the burst of `queue/add`
/// POSTs (one per track) so the request stays inside geomyidae's ~10 s budget and
/// doesn't hammer the rate-limited player endpoint. Covers essentially every
/// single album; a longer compilation is truncated (best-effort).
const QUEUE_ALBUM_MAX: usize = 24;

/// `/queue/album?id=<album_id>`: enqueue an album's tracks onto up-next. Spotify's
/// queue endpoint takes ONE track uri at a time (no context enqueue), so this
/// expands the album server-side into a bounded run of `queue/add` POSTs — the
/// "add a whole album" a client can't express as a single call, without playing
/// it now (that's `play/context`). Best-effort: a rate-limit (or upstream error)
/// mid-run stops early and returns whatever landed; if nothing landed, the error
/// surfaces. Returns the fresh `/queue` snapshot like `queue/add`. Non-base62 id
/// -> `not_found`; an album with no playable tracks -> `not_found`.
fn queue_album_doc(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let id = match args.query("id") {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return error("bad_query", "queue/album needs an id"),
    };
    if !crate::spotify::valid_id(&id) {
        return error("not_found", "unknown album");
    }
    // Collect up to QUEUE_ALBUM_MAX track uris, paging the album if needed.
    let mut uris: Vec<String> = Vec::new();
    let mut offset = 0u32;
    while uris.len() < QUEUE_ALBUM_MAX {
        let page = match api.album_tracks(&id, offset) {
            Ok(p) => p,
            Err(e) => return upstream(&e),
        };
        if page.items.is_empty() {
            break;
        }
        for t in &page.items {
            if uris.len() >= QUEUE_ALBUM_MAX {
                break;
            }
            if is_track_uri(&t.uri) {
                uris.push(t.uri.clone());
            }
        }
        offset += page.items.len() as u32;
        if offset as usize >= page.total as usize {
            break;
        }
    }
    if uris.is_empty() {
        return error("not_found", "album has no playable tracks");
    }
    let mut added = 0usize;
    for uri in &uris {
        match api.queue_add(uri) {
            Ok(()) => added += 1,
            // Any enqueue failure ends the run. A rate-limit must never be
            // amplified; other errors won't heal within the request. If NOTHING
            // landed, surface it (mapping the rate_limited sentinel); otherwise
            // the partial add is a success — return the queue.
            Err(e) => {
                if added == 0 {
                    return upstream(&e);
                }
                break;
            }
        }
    }
    api.invalidate_queue_cache();
    api.invalidate_now_cache();
    queue_doc(api, now_ms)
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
        Ok(r) => search_snapshot(&r, now_ms),
        Err(e) => upstream(&e),
    }
}

/// The `/search` document. Tracks lead, unchanged for old clients: `result_len`
/// then one `item.<i>.*` block per track (same shape as `/queue`). Additive since
/// this fio: `artist_len` + `artist.<i>.{id,name}` and `album_len` +
/// `album.<i>.{id,name}` — the `search()` call already asks Spotify for
/// `type=track,album,artist`, so this just stops discarding the other two kinds.
/// An id-less ref (Spotify omitted the uri) is skipped: a client needs the id to
/// open the artist's discography or play the album context.
fn search_snapshot(r: &SearchResults, now_ms: i64) -> String {
    let tracks = r.tracks.as_ref().map(|p| p.items.as_slice()).unwrap_or(&[]);
    let artists = r
        .artists
        .as_ref()
        .map(|p| p.items.as_slice())
        .unwrap_or(&[]);
    let albums = r.albums.as_ref().map(|p| p.items.as_slice()).unwrap_or(&[]);
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "result_len", &tracks.len().to_string());
    for (i, t) in tracks.iter().enumerate() {
        push_item(&mut out, i, t);
    }
    let artist_refs: Vec<(&str, &str)> = artists
        .iter()
        .filter_map(|a| crate::spotify::id_from_uri(&a.uri).map(|id| (id, a.name.as_str())))
        .collect();
    kv(&mut out, "artist_len", &artist_refs.len().to_string());
    for (i, (id, name)) in artist_refs.iter().enumerate() {
        kv(&mut out, &format!("artist.{i}.id"), id);
        kv(&mut out, &format!("artist.{i}.name"), name);
    }
    let album_refs: Vec<(&str, &str)> = albums
        .iter()
        .filter_map(|a| crate::spotify::id_from_uri(&a.uri).map(|id| (id, a.name.as_str())))
        .collect();
    kv(&mut out, "album_len", &album_refs.len().to_string());
    for (i, (id, name)) in album_refs.iter().enumerate() {
        kv(&mut out, &format!("album.{i}.id"), id);
        kv(&mut out, &format!("album.{i}.name"), name);
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

/// Cap on the `play/from` id list. The binding constraint is geomyidae's
/// request-line buffer, not Spotify: 24 bare ids ≈ 580 bytes of selector stays
/// comfortably inside it.
const PLAY_FROM_MAX_IDS: usize = 24;

/// A plausible **bare** track id: exactly 22 base62 chars — the tail of
/// `spotify:track:<id>`. Stricter than `valid_id` on purpose: the shape is
/// fixed-width, and rejecting anything else keeps ids un-interpolatable into a
/// Web API path (GS-02) and catches a client sending full uris by mistake.
fn is_track_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `/play/from?ids=<id1>,…,<idK>&offset=<n>`: start playback of an explicit
/// track list at index `offset` — the native "play from here onward". One
/// upstream PUT hands Spotify the whole list, so Spotify owns the continuation
/// (auto-advance at track end, next/prev within the list); the single-uri jump
/// path leaves a one-track context that stops dead at the track's end.
fn play_from(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let ids_raw = match args.query("ids") {
        Some(s) if !s.trim().is_empty() => s,
        _ => return error("bad_query", "play/from needs a non-empty ids list"),
    };
    let ids: Vec<String> = ids_raw.split(',').map(|s| s.trim().to_string()).collect();
    if ids.len() > PLAY_FROM_MAX_IDS {
        return error(
            "bad_range",
            &format!("at most {PLAY_FROM_MAX_IDS} ids per call"),
        );
    }
    if let Some(bad) = ids.iter().find(|id| !is_track_id(id)) {
        return error("bad_uri", &format!("not a bare base62 track id: {bad}"));
    }
    let offset = match args.query("offset") {
        None => 0,
        Some(s) => match s.trim().parse::<u32>() {
            Ok(n) if (n as usize) < ids.len() => n,
            Ok(n) => {
                return error(
                    "bad_range",
                    &format!("offset {n} is outside the {}-item list", ids.len()),
                )
            }
            Err(_) => return error("bad_range", "offset must be a non-negative integer"),
        },
    };
    match api.play_uris(&ids, offset) {
        Ok(()) => {
            // Jumping into a list replaces the upcoming tracks, so the cached
            // queue is stale — same bust next/prev pay.
            api.invalidate_queue_cache();
            let intent = Intent::PlayFrom {
                track_id: ids[offset as usize].clone(),
            };
            settled_now(api, now_ms, &intent)
        }
        Err(e) if e.contains("no_device") => {
            error("no_device", "gopher-spot device is not registered")
        }
        Err(e) => upstream(&e),
    }
}

/// A `spotify:{album|artist|playlist}:<base62>` uri — the kinds Spotify accepts
/// as a play `context_uri`. `track` is excluded on purpose: a single track has no
/// context (that's what `play/from` / `queue/add` are for). The id segment is
/// gated by `valid_id` so nothing dotted can be interpolated upstream (GS-02).
fn is_context_uri(uri: &str) -> bool {
    let kind = match uri.strip_prefix("spotify:") {
        Some(rest) => rest,
        None => return false,
    };
    let (kind, id) = match kind.split_once(':') {
        Some(kv) => kv,
        None => return false,
    };
    matches!(kind, "album" | "artist" | "playlist") && crate::spotify::valid_id(id)
}

/// `/play/context?uri=<spotify:album|artist|playlist:id>&offset=<n>`: play a whole
/// context in order — the native "queue this album". One upstream PUT hands
/// Spotify the `context_uri`, so it owns the continuation (auto-advance, next/prev
/// follow the album/playlist order) exactly like the human `?context_uri=` path.
/// Non-context uri -> `bad_uri`; the settle waits for playback to actually land on
/// the gopher-spot device (fio A2), so the reply never reads "playing elsewhere".
fn play_context_doc(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    let uri = match args.query("uri") {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => return error("bad_query", "play/context needs a non-empty uri"),
    };
    if !is_context_uri(&uri) {
        return error(
            "bad_uri",
            "uri must be spotify:album:/artist:/playlist:<id>",
        );
    }
    let offset = match args.query("offset") {
        None => 0,
        Some(s) => match s.trim().parse::<u32>() {
            Ok(n) => n,
            Err(_) => return error("bad_range", "offset must be a non-negative integer"),
        },
    };
    match api.play_context(&uri, offset) {
        Ok(()) => {
            // A new context replaces the upcoming tracks — bust the cached queue,
            // then settle on "playing on gopher-spot" (the first track of the
            // context; its exact id is Spotify's to pick, so Intent::Play suffices).
            api.invalidate_queue_cache();
            settled_now(api, now_ms, &Intent::Play)
        }
        Err(e) if e.contains("no_device") => {
            error("no_device", "gopher-spot device is not registered")
        }
        Err(e) => upstream(&e),
    }
}

/// `/artist/<id>/albums`: an artist's discography as an indexed list
/// (`item.<i>.{id,name}`), paginated via `?offset=`. `total`/`offset` let the
/// client page. An album with no uri (hence no id) is skipped — a client needs the
/// id to open or play it. Unknown/non-base62 id -> `not_found`.
fn artist_albums_doc(api: &dyn SpotifyApi, args: &DcgiArgs, id: &str, now_ms: i64) -> String {
    let id = id.trim_matches('/');
    if !crate::spotify::valid_id(id) {
        return error("not_found", "unknown artist");
    }
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match api.artist_albums(id, offset) {
        Ok(p) => albums_snapshot(&p, now_ms),
        Err(e) => upstream(&e),
    }
}

fn albums_snapshot(p: &AlbumsPage, now_ms: i64) -> String {
    let refs: Vec<(&str, &str)> = p
        .items
        .iter()
        .filter_map(|a| crate::spotify::id_from_uri(&a.uri).map(|id| (id, a.name.as_str())))
        .collect();
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "result_len", &refs.len().to_string());
    kv(&mut out, "total", &p.total.to_string());
    kv(&mut out, "offset", &p.offset.to_string());
    for (i, (id, name)) in refs.iter().enumerate() {
        kv(&mut out, &format!("item.{i}.id"), id);
        kv(&mut out, &format!("item.{i}.name"), name);
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// `/album/<id>`: an album's `name`/`artist`/`total` header then its tracks in the
/// `/search` list shape (`item.<i>.*`), paginated via `?offset=`. Lets a client
/// show the track list before playing the whole thing via `/play/context`.
/// Unknown/non-base62 id -> `not_found`.
fn album_doc(api: &dyn SpotifyApi, args: &DcgiArgs, id: &str, now_ms: i64) -> String {
    let id = id.trim_matches('/');
    if !crate::spotify::valid_id(id) {
        return error("not_found", "unknown album");
    }
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match (api.album(id), api.album_tracks(id, offset)) {
        (Ok(al), Ok(t)) => album_snapshot(&al, &t, now_ms),
        (Err(e), _) | (_, Err(e)) => upstream(&e),
    }
}

fn album_snapshot(al: &AlbumDetail, t: &TracksPage, now_ms: i64) -> String {
    let artist = al
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = String::new();
    kv(&mut out, "api", &API_VERSION.to_string());
    kv(&mut out, "name", &al.name);
    kv(&mut out, "artist", &artist);
    kv(&mut out, "total", &al.total.to_string());
    kv(&mut out, "result_len", &t.items.len().to_string());
    kv(&mut out, "offset", &t.offset.to_string());
    for (i, track) in t.items.iter().enumerate() {
        push_item(&mut out, i, track);
    }
    kv(&mut out, "ts", &now_ms.to_string());
    out
}

/// `/volume?<0-100>`: continuous. Out of range (or non-integer) -> `bad_range`.
fn volume(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    match args.raw_arg().trim().parse::<i64>() {
        Ok(v) if (0..=100).contains(&v) => match api.control(Control::Volume(v as u8)) {
            Ok(()) => settled_now(api, now_ms, &Intent::Volume(v as u8)),
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
    let target_ms = pos.min(duration);
    match api.seek(target_ms) {
        Ok(()) => settled_now(api, now_ms, &Intent::Seek { target_ms }),
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
        // wake is a command: the settled reply busts the micro-cache so the
        // transfer is reflected immediately (fio S3/2 synergy), and the settle
        // waits for `device active` to actually land (fio A2).
        Ok(()) => settled_now(api, now_ms, &Intent::Wake { play }),
        Err(e) if e.contains("no_device") => {
            error("no_device", "gopher-spot device is not registered")
        }
        Err(e) => upstream(&e),
    }
}

/// Run a play/pause/next/prev command, then reply with a settled snapshot (fio
/// A2). For the idempotent pair (`play`/`pause`), Spotify 403s "Restriction
/// violated" when the player is already in the requested state — swallow that
/// and return the snapshot, so `play` while playing is a no-op success
/// (contract rule; the settle predicate is trivially satisfied there). The
/// probe is gated on the error actually being a 403: on a 429/5xx the answer is
/// already "no", and the extra player call would only feed the rate limiter.
fn command(api: &dyn SpotifyApi, now_ms: i64, cmd: Control) -> String {
    // The intent is captured BEFORE the command: once it lands, the flip the
    // Skip predicate watches for may already be visible.
    let intent = match cmd {
        Control::Resume => Intent::Play,
        Control::Pause => Intent::Pause,
        Control::Next | Control::Prev => Intent::Skip {
            pre_track_id: pre_track_id(api, now_ms),
        },
        Control::Volume(v) => Intent::Volume(v),
    };
    match api.control(cmd) {
        Ok(()) => {
            // Only next/prev can change the upcoming queue; play/pause/volume
            // keep the warm queue entry (its own 10s TTL covers drift).
            if matches!(cmd, Control::Next | Control::Prev) {
                api.invalidate_queue_cache();
            }
            settled_now(api, now_ms, &intent)
        }
        Err(e) if e.contains("HTTP 403") && already_in_state(api, cmd) => {
            settled_now(api, now_ms, &intent)
        }
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

// ---- Settle-before-return (fio A2) ------------------------------------------
// Spotify's player state is eventually consistent (~1-2 s), so the snapshot a
// command used to return could still show pre-command state — a "lying"
// snapshot every client then paid for with its own ack/timeout machinery
// (Casquinha b48 watching track_id flip for 8 s). Commands now short-poll the
// player until a PURE predicate says the snapshot reflects the intent, bounded
// at 4 polls ~500 ms apart (~2 s cap). Best-effort by design: on timeout the
// latest snapshot is returned anyway (today's behavior, never an error), and a
// rate-limit arming mid-settle aborts immediately (settling must never amplify
// a 429).

/// Settle polls per command: the first is immediate (what commands always did),
/// each retry is preceded by [`SpotifyApi::settle_wait`] (~500 ms).
const SETTLE_ATTEMPTS: u32 = 4;
/// The nominal gap between settle polls, mirrored in `Client::settle_wait`.
/// Also the elapsed-time estimate fed to the [`Intent::Seek`] window.
const SETTLE_INTERVAL_MS: u64 = 500;

/// What a command intends the player state to become — captured BEFORE the
/// command is issued (`Skip` compares against the pre-command track).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// `play`: the player resumes.
    Play,
    /// `pause`: the player pauses (a track stays loaded).
    Pause,
    /// `next`/`prev`: the track flips away from the pre-command one. `None`
    /// pre-id (no recent snapshot to read it from) settles immediately —
    /// there is no fact to compare against, so behave as before A2.
    Skip { pre_track_id: Option<String> },
    /// `volume?v`: the device reports exactly `v`.
    Volume(u8),
    /// `seek?p`: the position landed at `p` — a window, not equality, because
    /// playback keeps advancing while we wait.
    Seek { target_ms: u64 },
    /// `wake[?play=1]`: gopher-spot is the active device (and playing, if asked).
    Wake { play: bool },
    /// `play/from`: the track at `offset` is what's loaded.
    PlayFrom { track_id: String },
}

/// The settle predicate: does `snap` already reflect `intent`? PURE
/// (`intent + snapshot + elapsed → bool`) — this is where the test value is;
/// the polling loop around it is trivial glue. `elapsed_ms` is the nominal
/// time since the command (attempt × interval), which widens the seek window
/// as playback advances under us.
pub fn settled(intent: &Intent, snap: &Playing, elapsed_ms: u64) -> bool {
    // Every machine-API command targets the gopher-spot librespot device, so a
    // reply is only "settled" once the snapshot also agrees the ACTIVE device is
    // gopher-spot. Spotify's `/v1/me/player` is eventually consistent on the
    // device field, not just playback: the first post-command poll can show the
    // track/state already flipped while `device` still names the previous player.
    // That snapshot renders `device idle` → the client shows "playing elsewhere"
    // for a track gopher-spot is in fact producing. `wake` always required this;
    // extend it to the other playing intents (fio A2 follow-up). The stopped/None
    // escapes stay ungated — a genuinely stopped player reports no device (204),
    // which is itself a valid settle.
    let gs_active = matches!(&snap.device, Some(d) if d.name == "gopher-spot");
    match intent {
        Intent::Play => gs_active && snap.is_playing && snap.item.is_some(),
        Intent::Pause => gs_active && !snap.is_playing && snap.item.is_some(),
        Intent::Skip { pre_track_id: None } => true,
        Intent::Skip {
            pre_track_id: Some(pre),
        } => match &snap.item {
            // Skipping past the end of the queue legitimately stops playback.
            None => true,
            Some(t) => gs_active && t.id.as_deref() != Some(pre.as_str()),
        },
        Intent::Volume(v) => {
            matches!(&snap.device, Some(d) if d.name == "gopher-spot" && d.volume_percent == Some(*v as u32))
        }
        Intent::Seek { target_ms } => {
            gs_active
                && snap.progress_ms >= *target_ms
                && snap.progress_ms <= target_ms + elapsed_ms + SETTLE_SEEK_SLACK_MS
        }
        Intent::Wake { play } => gs_active && (!play || snap.is_playing),
        Intent::PlayFrom { track_id } => {
            gs_active && matches!(&snap.item, Some(t) if t.id.as_deref() == Some(track_id.as_str()))
        }
    }
}

/// Slack on the seek window: Spotify's reported position is ~1 s-grained plus
/// a round-trip, so demand `[target, target + elapsed + slack]`, not equality.
const SETTLE_SEEK_SLACK_MS: u64 = 1_500;

/// The value of `key` in a rendered v1 document — lets a command read the
/// pre-command state off the last snapshot without paying an upstream call.
fn doc_value<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
    doc.split("\r\n")
        .filter_map(|l| l.split_once('\t'))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// The pre-command track id for a `Skip` intent, read from the last good
/// snapshot (the ~30 s stale copy `store_now` keeps, refreshed by every `/now`
/// fetch — under any polling client it is at most one poll old). Zero upstream
/// cost; `None` (no recent snapshot, or nothing was loaded) degrades the skip
/// to settling on its first poll, exactly the pre-A2 behavior.
fn pre_track_id(api: &dyn SpotifyApi, now_ms: i64) -> Option<String> {
    api.stale_now(now_ms)
        .and_then(|doc| doc_value(&doc, "track_id").map(str::to_string))
}

/// A command's reply (fio A2): bust the micro-cache, then short-poll the player
/// until the snapshot reflects `intent` (or the attempts run out — the latest
/// snapshot is returned regardless, never an error). The settled snapshot is
/// stored as the fresh `now_snapshot`, so followers read post-command state.
/// Deliberately NOT under `now_fetch_lock`: holding it across the settle would
/// park concurrent `/now` polls for up to ~2 s.
fn settled_now(api: &dyn SpotifyApi, now_ms: i64, intent: &Intent) -> String {
    api.invalidate_now_cache();
    let mut last: Option<(Playing, u64)> = None;
    let mut first_err: Option<String> = None;
    for attempt in 0..SETTLE_ATTEMPTS {
        if attempt > 0 {
            api.settle_wait();
        }
        let elapsed = attempt as u64 * SETTLE_INTERVAL_MS;
        match api.now_playing() {
            Ok(p) => {
                let done = settled(intent, &p, elapsed);
                last = Some((p, elapsed));
                if done {
                    break;
                }
            }
            // Any failure ends the settle: the rate_limited sentinel means a
            // cooldown armed mid-settle (polling on would amplify the 429),
            // and other upstream errors won't heal within a poll interval.
            Err(e) => {
                if last.is_none() {
                    first_err = Some(e);
                }
                break;
            }
        }
    }
    match last {
        Some((p, elapsed)) => {
            let queue_len = if p.item.is_some() {
                api.queue().map(|q| q.len()).unwrap_or(0)
            } else {
                0
            };
            // Stamp ts with the settle clock: the position was sampled up to
            // ~1.5 s after the request landed, and a client interpolates from
            // ts — the request-time stamp would double-count that gap.
            let ts = now_ms + elapsed as i64;
            let doc = snapshot(&p, queue_len, ts);
            api.store_now(ts, &doc);
            doc
        }
        // No snapshot at all: same degradation ladder as /now (stale serve
        // during a cooldown, else the upstream error).
        None => {
            let e = first_err.unwrap_or_default();
            if e.starts_with(crate::spotify::RATE_LIMITED) {
                match api.stale_now(now_ms) {
                    Some(doc) => doc,
                    None => upstream(&e),
                }
            } else {
                upstream(&e)
            }
        }
    }
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
        /// Every uri handed to queue_add, in order — lets queue/album assert the
        /// per-track enqueue count.
        queued: RefCell<Vec<String>>,
        /// play/from: the (ids, offset) the endpoint handed to play_uris.
        last_play_from: RefCell<Option<(Vec<String>, u32)>>,
        /// play/context: the (context_uri, offset) handed to play_context.
        last_context: RefCell<Option<(String, u32)>>,
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
        // fio A2: when non-empty, now_playing() consumes this sequence front-
        // first (then falls back to the fields above) — lets a test script an
        // eventually-consistent player; `waits` counts settle_wait() pauses.
        now_seq: RefCell<Vec<Result<Playing, ApiError>>>,
        waits: RefCell<u32>,
    }
    fn fake(playing: Playing) -> Fake {
        Fake {
            playing,
            control_err: None,
            empty_queue: false,
            last: RefCell::new(None),
            last_seek: RefCell::new(None),
            last_queued: RefCell::new(None),
            queued: RefCell::new(Vec::new()),
            last_play_from: RefCell::new(None),
            last_context: RefCell::new(None),
            now_calls: RefCell::new(0),
            now_cache: RefCell::new(None),
            queue_calls: RefCell::new(0),
            queue_busts: RefCell::new(0),
            no_device: false,
            last_wake: RefCell::new(None),
            now_fails: None,
            stale_doc: None,
            now_seq: RefCell::new(Vec::new()),
            waits: RefCell::new(0),
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
            let mut seq = self.now_seq.borrow_mut();
            if !seq.is_empty() {
                return seq.remove(0);
            }
            if let Some(e) = &self.now_fails {
                return Err(e.clone());
            }
            Ok(self.playing.clone())
        }
        fn settle_wait(&self) {
            *self.waits.borrow_mut() += 1;
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
            // control_err lets a test arm a rate-limit (or other upstream error)
            // on the enqueue — how queue/album's mid-loop abort is exercised.
            if let Some(e) = &self.control_err {
                return Err(e.clone());
            }
            *self.last_queued.borrow_mut() = Some(uri.to_string());
            self.queued.borrow_mut().push(uri.to_string());
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
                artists: Some(Page {
                    items: vec![
                        Artist {
                            name: "Chico Buarque".into(),
                            uri: "spotify:artist:art1".into(),
                        },
                        // No uri -> no id -> must be filtered out of the doc.
                        Artist {
                            name: "sem uri".into(),
                            uri: String::new(),
                        },
                    ],
                }),
                albums: Some(Page {
                    items: vec![Album {
                        name: "Construção".into(),
                        uri: "spotify:album:al1".into(),
                    }],
                }),
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
        fn play_context(&self, c: &str, o: u32) -> Result<(), ApiError> {
            // Same failure knobs as play_uris: no_device (like wake) and
            // control_err (upstream/sentinel mapping).
            if self.no_device {
                return Err("no_device: 'gopher-spot' is not registered".into());
            }
            if let Some(e) = &self.control_err {
                return Err(e.clone());
            }
            *self.last_context.borrow_mut() = Some((c.to_string(), o));
            Ok(())
        }
        fn play_uris(&self, ids: &[String], offset: u32) -> Result<(), ApiError> {
            // Failure knobs mirror the endpoints this composes with: `no_device`
            // (like wake) and `control_err` (upstream/sentinel mapping).
            if self.no_device {
                return Err("no_device: 'gopher-spot' is not registered".into());
            }
            if let Some(e) = &self.control_err {
                return Err(e.clone());
            }
            *self.last_play_from.borrow_mut() = Some((ids.to_vec(), offset));
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
            Ok(AlbumDetail {
                name: "Construção".into(),
                uri: "spotify:album:al1".into(),
                artists: vec![
                    Artist {
                        name: "Chico Buarque".into(),
                        uri: "spotify:artist:art1".into(),
                    },
                    Artist {
                        name: "MPB4".into(),
                        uri: "spotify:artist:art2".into(),
                    },
                ],
                total: 11,
            })
        }
        fn album_tracks(&self, _id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            // Paginate honestly: two tracks on page 0, empty after — so a pager
            // (queue/album) terminates instead of looping on a fixed page.
            let items = if offset == 0 {
                vec![track("Deus Lhe Pague"), track("Cotidiano")]
            } else {
                Vec::new()
            };
            Ok(TracksPage {
                items,
                total: 2,
                offset,
            })
        }
        fn artist(&self, _id: &str) -> Result<Artist, ApiError> {
            unimplemented!()
        }
        fn artist_albums(&self, _id: &str, offset: u32) -> Result<AlbumsPage, ApiError> {
            Ok(AlbumsPage {
                items: vec![
                    Album {
                        name: "Construção".into(),
                        uri: "spotify:album:al1".into(),
                    },
                    Album {
                        name: "Chico 50 Anos".into(),
                        uri: "spotify:album:al2".into(),
                    },
                    // No uri -> no id -> must be filtered out of the doc.
                    Album {
                        name: "sem uri".into(),
                        uri: String::new(),
                    },
                ],
                total: 30,
                offset,
            })
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
        String::from_utf8(route(&args.path(), &args, Some(f), None, TS)).unwrap()
    }
    fn call_bytes(f: &Fake, selector: &str) -> Vec<u8> {
        let args = DcgiArgs::from_argv(&argv(selector));
        route(&args.path(), &args, Some(f), None, TS)
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
        let out = String::from_utf8(route(&args.path(), &args, None, None, TS)).unwrap();
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

    // ---- fio A: /stream ---------------------------------------------------------

    /// A fake media-plane source: canned facts (or a canned error), plus an
    /// in-memory document cache mirroring the Fake's now-cache slot.
    struct FakeStream {
        facts: Result<StreamFacts, String>,
        fetches: RefCell<u32>,
        cache: RefCell<Option<(i64, String)>>,
    }
    fn fake_stream(live: bool, listeners: u64) -> FakeStream {
        FakeStream {
            facts: Ok(StreamFacts { live, listeners }),
            fetches: RefCell::new(0),
            cache: RefCell::new(None),
        }
    }
    impl StreamSource for FakeStream {
        fn stream_facts(&self) -> Result<StreamFacts, String> {
            *self.fetches.borrow_mut() += 1;
            self.facts.clone()
        }
        fn cached_stream(&self, now_ms: i64) -> Option<String> {
            self.cache
                .borrow()
                .as_ref()
                .filter(|(exp, _)| now_ms < *exp)
                .map(|(_, d)| d.clone())
        }
        fn store_stream(&self, now_ms: i64, doc: &str) {
            *self.cache.borrow_mut() = Some((now_ms + 2_000, doc.to_string()));
        }
    }

    /// `/stream` at an explicit wall-clock. `api` is None on purpose in most
    /// tests: the endpoint must not depend on the Spotify OAuth Secret.
    fn stream_at(s: &FakeStream, now_ms: i64) -> String {
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/stream"));
        String::from_utf8(route(&args.path(), &args, None, Some(s), now_ms)).unwrap()
    }

    #[test]
    fn stream_reports_live_and_listeners() {
        let s = fake_stream(true, 2);
        let out = stream_at(&s, TS);
        assert_wire(&out);
        assert!(out.contains("api\t1\r\n"));
        assert!(out.contains("live\t1\r\n"));
        assert!(out.contains("listeners\t2\r\n"));
        assert!(out.contains(&format!("ts\t{TS}\r\n")));
    }

    #[test]
    fn stream_silence_fallback_is_live_zero() {
        let out = stream_at(&fake_stream(false, 0), TS);
        assert!(out.contains("live\t0\r\n"));
        assert!(out.contains("listeners\t0\r\n"));
    }

    #[test]
    fn stream_answers_without_the_spotify_api() {
        // Explicitly: no OAuth Secret (api None) must NOT turn /stream into the
        // "spotify api not configured" error — Icecast is its own state owner.
        let out = stream_at(&fake_stream(true, 0), TS);
        assert!(!out.contains("error\t"), "{out}");
    }

    #[test]
    fn stream_unreachable_icecast_is_upstream() {
        let s = FakeStream {
            facts: Err("icecast status fetch failed: timeout".into()),
            ..fake_stream(false, 0)
        };
        let out = stream_at(&s, TS);
        assert_wire(&out);
        assert!(out.contains("error\tupstream\r\n"), "{out}");
    }

    #[test]
    fn stream_without_a_source_is_upstream() {
        // The non-net build (or a future misconfig) has no fetcher at all.
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/stream"));
        let out = String::from_utf8(route(&args.path(), &args, None, None, TS)).unwrap();
        assert!(out.contains("error\tupstream\r\n"), "{out}");
    }

    #[test]
    fn stream_polls_within_ttl_hit_the_cache() {
        let s = fake_stream(true, 1);
        let a = stream_at(&s, TS);
        let b = stream_at(&s, TS + 1_500); // within the ~2s window
        assert_eq!(*s.fetches.borrow(), 1, "second poll must be a cache hit");
        assert_eq!(a, b, "same document, same ts");
        stream_at(&s, TS + 2_000); // window over -> refetch
        assert_eq!(*s.fetches.borrow(), 2);
    }

    #[test]
    fn stream_errors_are_never_cached() {
        let s = FakeStream {
            facts: Err("icecast status fetch failed: boom".into()),
            ..fake_stream(false, 0)
        };
        stream_at(&s, TS);
        stream_at(&s, TS + 100);
        assert_eq!(
            *s.fetches.borrow(),
            2,
            "an error document must not stick for the TTL"
        );
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

    #[test]
    fn queue_album_enqueues_every_track_and_returns_queue() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/queue/album?id=al1");
        // The fake album has two tracks — both enqueued.
        assert_eq!(f.queued.borrow().len(), 2, "both album tracks enqueued");
        // Caches busted and the fresh /queue snapshot returned (not /now).
        assert!(*f.queue_busts.borrow() >= 1, "queue cache busted");
        assert!(out.contains("queue_len\t2\r\n"), "{out}");
        assert!(!out.contains("state\t"), "returns /queue, not /now");
    }

    #[test]
    fn queue_album_bad_or_missing_id() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/queue/album").contains("error\tbad_query\r\n"));
        assert!(call(&f, "/spot/api/1/queue/album?id=").contains("error\tbad_query\r\n"));
        // Non-base62 id must not reach the upstream path (GS-02).
        assert!(call(&f, "/spot/api/1/queue/album?id=..%2Fme").contains("error\tnot_found\r\n"));
        assert!(f.queued.borrow().is_empty());
    }

    #[test]
    fn queue_album_rate_limited_before_any_add_surfaces_the_error() {
        // A cooldown armed before the first enqueue: nothing landed, so the
        // rate_limited sentinel surfaces (mapped by upstream()).
        let f = Fake {
            control_err: Some(format!("{} spotify cooldown", crate::spotify::RATE_LIMITED)),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/queue/album?id=al1");
        assert!(out.contains("error\trate_limited\r\n"), "{out}");
        assert!(f.queued.borrow().is_empty());
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
        String::from_utf8(route(&args.path(), &args, Some(f), None, now_ms)).unwrap()
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
        route(&args.path(), &args, Some(&f), None, TS + 100);
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
        assert!(upstream("spotify HTTP 500: rate_limited: nope").contains("error\tupstream\r\n"));
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
        assert_eq!(
            *f.queue_busts.borrow(),
            0,
            "non-queue commands must not bust"
        );
        // next/prev/queue_add/play_from do change it -> one bust each.
        call(&f, "/spot/api/1/next");
        assert_eq!(*f.queue_busts.borrow(), 1);
        call(&f, "/spot/api/1/prev");
        assert_eq!(*f.queue_busts.borrow(), 2);
        call(&f, "/spot/api/1/queue/add?spotify:track:xyz");
        assert_eq!(*f.queue_busts.borrow(), 3);
        call(&f, "/spot/api/1/play/from?ids=7hQJA50XrCWABAu5v6QZ4i");
        assert_eq!(*f.queue_busts.borrow(), 4);
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

    // ---- fio A2: settle-before-return ------------------------------------------

    /// The same playing snapshot with a different loaded track id.
    fn with_track_id(mut p: Playing, id: &str) -> Playing {
        if let Some(t) = p.item.as_mut() {
            t.id = Some(id.into());
        }
        p
    }
    /// The same snapshot with a different active device.
    fn with_device(mut p: Playing, name: &str, volume: Option<u32>) -> Playing {
        p.device = Some(Device {
            id: Some("d9".into()),
            name: name.into(),
            is_active: true,
            volume_percent: volume,
        });
        p
    }
    /// A stale `/now` doc carrying the pre-command track — what `pre_track_id`
    /// reads instead of paying an upstream call.
    fn stale_with_track(id: &str) -> Option<String> {
        Some(format!(
            "api\t1\r\nstate\tplaying\r\ntrack_id\t{id}\r\nts\t123\r\n"
        ))
    }

    #[test]
    fn doc_value_reads_the_wire_format() {
        let doc = "api\t1\r\nstate\tplaying\r\ntrack_id\tabc123\r\nts\t9\r\n";
        assert_eq!(doc_value(doc, "track_id"), Some("abc123"));
        assert_eq!(doc_value(doc, "state"), Some("playing"));
        assert_eq!(doc_value(doc, "volume"), None);
        assert_eq!(doc_value("", "track_id"), None);
    }

    #[test]
    fn settle_predicate_play_and_pause() {
        let playing = playing_track();
        let mut paused = playing_track();
        paused.is_playing = false;
        assert!(settled(&Intent::Play, &playing, 0));
        assert!(!settled(&Intent::Play, &paused, 0));
        assert!(
            !settled(&Intent::Play, &stopped(), 0),
            "no track loaded yet"
        );
        assert!(settled(&Intent::Pause, &paused, 0));
        assert!(!settled(&Intent::Pause, &playing, 0));
        assert!(!settled(&Intent::Pause, &stopped(), 0));
    }

    #[test]
    fn settle_predicate_skip() {
        let pre = Intent::Skip {
            pre_track_id: Some("abc123".into()),
        };
        // Same track -> the flip hasn't landed.
        assert!(!settled(&pre, &playing_track(), 0));
        // Track changed -> settled.
        assert!(settled(&pre, &with_track_id(playing_track(), "zzz999"), 0));
        // Skipped past the last track (stopped) -> settled.
        assert!(settled(&pre, &stopped(), 0));
        // An item with no id at all differs from a known pre id.
        let mut anon = playing_track();
        anon.item.as_mut().unwrap().id = None;
        assert!(settled(&pre, &anon, 0));
        // No pre-command fact -> nothing to compare, settle immediately.
        let blind = Intent::Skip { pre_track_id: None };
        assert!(settled(&blind, &playing_track(), 0));
    }

    #[test]
    fn settle_predicate_volume() {
        // playing_track()'s device reports 65.
        assert!(settled(&Intent::Volume(65), &playing_track(), 0));
        assert!(!settled(&Intent::Volume(70), &playing_track(), 0));
        // No device (nothing to read the volume from) -> not settled.
        assert!(!settled(&Intent::Volume(70), &stopped(), 0));
        // A device that doesn't report volume -> not settled.
        let mute = with_device(playing_track(), "gopher-spot", None);
        assert!(!settled(&Intent::Volume(70), &mute, 0));
    }

    #[test]
    fn settle_predicate_seek_is_a_window_not_equality() {
        let at = |ms: u64| {
            let mut p = playing_track();
            p.progress_ms = ms;
            p
        };
        let seek = Intent::Seek { target_ms: 60_000 };
        // Still at the pre-seek position -> not settled.
        assert!(!settled(&seek, &at(42_000), 0));
        // Landed exactly, or advanced a little while we waited -> settled.
        assert!(settled(&seek, &at(60_000), 0));
        assert!(settled(&seek, &at(61_000), 0)); // inside target + 0 + 1500
        assert!(settled(&seek, &at(62_400), 1_000)); // inside target + 1000 + 1500
                                                     // Way past the window -> that's not our seek reflecting.
        assert!(!settled(&seek, &at(70_000), 0));
        // The window widens with elapsed time (playback keeps advancing).
        assert!(!settled(&seek, &at(62_400), 0));
    }

    #[test]
    fn settle_predicate_wake() {
        let ours = playing_track(); // device gopher-spot, playing
        let phone = with_device(playing_track(), "iPhone", Some(40));
        assert!(settled(&Intent::Wake { play: false }, &ours, 0));
        assert!(!settled(&Intent::Wake { play: false }, &phone, 0));
        assert!(!settled(&Intent::Wake { play: false }, &stopped(), 0));
        // play=1 additionally demands the player actually playing.
        assert!(settled(&Intent::Wake { play: true }, &ours, 0));
        let mut ours_paused = playing_track();
        ours_paused.is_playing = false;
        assert!(!settled(&Intent::Wake { play: true }, &ours_paused, 0));
        assert!(settled(&Intent::Wake { play: false }, &ours_paused, 0));
    }

    #[test]
    fn settle_predicate_play_from() {
        let want = Intent::PlayFrom {
            track_id: "zzz999".into(),
        };
        assert!(settled(&want, &with_track_id(playing_track(), "zzz999"), 0));
        assert!(!settled(&want, &playing_track(), 0)); // still the old track
        assert!(!settled(&want, &stopped(), 0));
    }

    #[test]
    fn settle_requires_gopher_spot_active_not_a_transient_other_device() {
        // The "playing elsewhere" lie (fio A2 follow-up): Spotify's device field
        // lags the playback flip, so the first post-command poll can show the new
        // state on the PHONE. That must NOT settle — returning it renders `device
        // idle` and the client says "playing elsewhere" for a local track.
        let phone_new = with_device(with_track_id(playing_track(), "zzz999"), "iPhone", Some(40));
        let ours_new = with_track_id(playing_track(), "zzz999"); // gopher-spot active

        let skip = Intent::Skip {
            pre_track_id: Some("abc123".into()),
        };
        assert!(
            !settled(&skip, &phone_new, 0),
            "track flipped but on the phone"
        );
        assert!(settled(&skip, &ours_new, 0), "flipped on gopher-spot");

        assert!(!settled(
            &Intent::Play,
            &with_device(playing_track(), "iPhone", Some(40)),
            0
        ));
        assert!(settled(&Intent::Play, &playing_track(), 0));

        let from = Intent::PlayFrom {
            track_id: "zzz999".into(),
        };
        assert!(!settled(&from, &phone_new, 0));
        assert!(settled(&from, &ours_new, 0));

        let seek = Intent::Seek { target_ms: 42_000 };
        assert!(!settled(
            &seek,
            &with_device(playing_track(), "iPhone", Some(40)),
            0
        ));
        assert!(settled(&seek, &playing_track(), 0));
    }

    #[test]
    fn play_settles_only_once_the_device_lands_on_gopher_spot() {
        // End-to-end guard for the "playing elsewhere" report: the command lands,
        // but the first poll still names the phone. The settle keeps polling until
        // gopher-spot is the active device, so the reply is `device active`.
        let f = fake(playing_track());
        *f.now_seq.borrow_mut() = vec![
            Ok(with_device(playing_track(), "iPhone", Some(40))), // device field lags
            Ok(playing_track()),                                  // gopher-spot active
        ];
        let out = call(&f, "/spot/api/1/play");
        assert!(out.contains("device\tactive\r\n"), "must not lie: {out}");
        assert!(!out.contains("device\tidle\r\n"), "{out}");
        assert_eq!(*f.now_calls.borrow(), 2, "polled past the lagging device");
    }

    #[test]
    fn next_settles_when_the_track_flips() {
        let f = Fake {
            stale_doc: stale_with_track("abc123"),
            ..fake(playing_track())
        };
        // Spotify is eventually consistent: two stale echoes, then the flip.
        *f.now_seq.borrow_mut() = vec![
            Ok(playing_track()),
            Ok(playing_track()),
            Ok(with_track_id(playing_track(), "zzz999")),
        ];
        let out = call(&f, "/spot/api/1/next");
        assert!(out.contains("track_id\tzzz999\r\n"), "settled reply: {out}");
        assert_eq!(*f.now_calls.borrow(), 3, "stopped polling once settled");
        assert_eq!(*f.waits.borrow(), 2, "one pause before each retry");
        // ts is stamped with the settle clock (2 retries x 500ms), not the
        // request clock — the client interpolates from it.
        assert!(out.contains(&format!("ts\t{}\r\n", TS + 1_000)), "{out}");
    }

    #[test]
    fn settle_times_out_and_returns_the_latest_snapshot_anyway() {
        // The player never reflects the skip (the fake keeps serving the same
        // track): the settle exhausts its budget and the reply is the latest
        // snapshot — never an error (best-effort by contract).
        let f = Fake {
            stale_doc: stale_with_track("abc123"),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/next");
        assert!(!out.contains("error\t"), "{out}");
        assert!(out.contains("track_id\tabc123\r\n"), "latest echo: {out}");
        assert_eq!(*f.now_calls.borrow(), 4, "the full settle budget");
        assert_eq!(*f.waits.borrow(), 3);
    }

    #[test]
    fn skip_without_a_pre_fact_settles_on_the_first_poll() {
        // No stale snapshot to read the pre-command track from -> exactly the
        // pre-A2 behavior: one fetch, no waits.
        let f = fake(playing_track());
        call(&f, "/spot/api/1/next");
        assert_eq!(*f.now_calls.borrow(), 1);
        assert_eq!(*f.waits.borrow(), 0);
    }

    #[test]
    fn settle_aborts_when_a_rate_limit_arms_mid_flight() {
        // A 429 cooldown arming between polls must end the settle immediately
        // (never amplify a rate-limit event) and reply with the last snapshot.
        let f = Fake {
            stale_doc: stale_with_track("abc123"),
            ..fake(playing_track())
        };
        *f.now_seq.borrow_mut() = vec![
            Ok(playing_track()),
            Err("rate_limited: spotify HTTP 429 (retry after 5s)".into()),
        ];
        let out = call(&f, "/spot/api/1/next");
        assert!(!out.contains("error\t"), "{out}");
        assert!(out.contains("track_id\tabc123\r\n"));
        assert_eq!(*f.now_calls.borrow(), 2, "no third poll after the 429");
    }

    #[test]
    fn settle_first_poll_rate_limited_serves_the_stale_doc() {
        // The command landed but the very first settle poll hit a cooldown:
        // same degradation ladder as /now — the stale doc, verbatim.
        let stale = stale_with_track("abc123").unwrap();
        let f = Fake {
            stale_doc: Some(stale.clone()),
            ..fake(playing_track())
        };
        *f.now_seq.borrow_mut() = vec![Err(
            "rate_limited: spotify cooldown active (until ~9)".into()
        )];
        let out = call(&f, "/spot/api/1/next");
        assert_eq!(out, stale);
    }

    #[test]
    fn volume_settles_on_the_reported_value() {
        let f = fake(playing_track()); // device reports 65
        *f.now_seq.borrow_mut() = vec![
            Ok(playing_track()), // still 65
            Ok(with_device(playing_track(), "gopher-spot", Some(70))),
        ];
        let out = call(&f, "/spot/api/1/volume?70");
        assert!(out.contains("volume\t70\r\n"), "{out}");
        assert_eq!(*f.now_calls.borrow(), 2);
    }

    #[test]
    fn wake_settles_when_the_transfer_lands() {
        let f = fake(playing_track());
        *f.now_seq.borrow_mut() = vec![
            Ok(with_device(playing_track(), "iPhone", Some(40))), // pre-transfer echo
            Ok(playing_track()),                                  // gopher-spot active
        ];
        let out = call(&f, "/spot/api/1/wake?play=1");
        assert!(out.contains("device\tactive\r\n"), "{out}");
        assert_eq!(*f.now_calls.borrow(), 2);
        assert_eq!(*f.waits.borrow(), 1);
    }

    #[test]
    fn settled_reply_reseeds_the_now_cache() {
        // The snapshot the settle ends on becomes the fresh now_snapshot, so a
        // follower poll right after the command is a cache hit on POST-command
        // state (fio S3/2 synergy, now with the settled doc).
        let f = Fake {
            stale_doc: stale_with_track("abc123"),
            ..fake(playing_track())
        };
        *f.now_seq.borrow_mut() = vec![
            Ok(playing_track()),
            Ok(with_track_id(playing_track(), "zzz999")),
        ];
        let args = DcgiArgs::from_argv(&argv("/spot/api/1/next"));
        route(&args.path(), &args, Some(&f), None, TS);
        let fetches_after_command = *f.now_calls.borrow();
        let follow = now_at(&f, TS + 600);
        assert_eq!(
            *f.now_calls.borrow(),
            fetches_after_command,
            "follower must hit the reseeded cache"
        );
        assert!(follow.contains("track_id\tzzz999\r\n"), "{follow}");
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

    // ---- album-context feature: artists+albums in search, discography, play ----

    #[test]
    fn search_surfaces_artists_and_albums() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/search?q=chico");
        assert_wire(&out);
        // Tracks unchanged (old clients keep working).
        assert!(out.contains("result_len\t2\r\n"));
        assert!(out.contains("item.0.track\thit chico A\r\n"));
        // Artists: the id-less one is filtered, so 1 of 2.
        assert!(out.contains("artist_len\t1\r\n"), "{out}");
        assert!(out.contains("artist.0.id\tart1\r\n"), "{out}");
        assert!(out.contains("artist.0.name\tChico Buarque\r\n"), "{out}");
        assert!(
            !out.contains("sem uri"),
            "id-less ref must be dropped: {out}"
        );
        // Albums.
        assert!(out.contains("album_len\t1\r\n"), "{out}");
        assert!(out.contains("album.0.id\tal1\r\n"), "{out}");
        assert!(out.contains("album.0.name\tConstrução\r\n"), "{out}");
    }

    #[test]
    fn artist_albums_lists_the_discography_with_ids() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/artist/art1/albums");
        assert_wire(&out);
        assert!(out.contains("result_len\t2\r\n"), "id-less filtered: {out}");
        assert!(out.contains("total\t30\r\n"));
        assert!(out.contains("offset\t0\r\n"));
        assert!(out.contains("item.0.id\tal1\r\n"));
        assert!(out.contains("item.0.name\tConstrução\r\n"));
        assert!(out.contains("item.1.id\tal2\r\n"));
        assert!(!out.contains("sem uri"), "{out}");
    }

    #[test]
    fn artist_albums_paginates_via_offset() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/artist/art1/albums?offset=20");
        assert!(out.contains("offset\t20\r\n"), "{out}");
    }

    #[test]
    fn artist_albums_rejects_a_non_base62_id() {
        let f = fake(playing_track());
        // The /albums suffix still routes here; the id gate rejects it.
        let out = call(&f, "/spot/api/1/artist/..%2Fme/albums");
        assert!(out.contains("error\tnot_found\r\n"), "{out}");
    }

    #[test]
    fn album_lists_header_and_tracks() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/album/al1");
        assert_wire(&out);
        assert!(out.contains("name\tConstrução\r\n"));
        assert!(out.contains("artist\tChico Buarque, MPB4\r\n"));
        assert!(out.contains("total\t11\r\n"));
        assert!(out.contains("result_len\t2\r\n"));
        assert!(out.contains("item.0.track\tDeus Lhe Pague\r\n"));
        assert!(out.contains("item.1.track\tCotidiano\r\n"));
    }

    #[test]
    fn album_rejects_a_non_base62_id() {
        let f = fake(playing_track());
        let out = call(&f, "/spot/api/1/album/..%2Fme");
        assert!(out.contains("error\tnot_found\r\n"), "{out}");
    }

    #[test]
    fn play_context_plays_the_album_and_settles() {
        let f = fake(playing_track());
        let out = call(
            &f,
            "/spot/api/1/play/context?uri=spotify:album:al1&offset=0",
        );
        // The endpoint handed Spotify the album context...
        assert_eq!(
            *f.last_context.borrow(),
            Some(("spotify:album:al1".to_string(), 0))
        );
        // ...and replied with a settled /now (device active, playing) — never a lie.
        assert!(out.contains("state\tplaying\r\n"), "{out}");
        assert!(out.contains("device\tactive\r\n"), "{out}");
        // A new context invalidates the cached queue.
        assert!(*f.queue_busts.borrow() >= 1, "queue cache must be busted");
    }

    #[test]
    fn play_context_defaults_offset_to_zero() {
        let f = fake(playing_track());
        call(&f, "/spot/api/1/play/context?uri=spotify:artist:art1");
        assert_eq!(f.last_context.borrow().as_ref().unwrap().1, 0);
    }

    #[test]
    fn play_context_rejects_bad_or_missing_uri() {
        let f = fake(playing_track());
        // A single track is not a context.
        assert!(
            call(&f, "/spot/api/1/play/context?uri=spotify:track:abc123")
                .contains("error\tbad_uri\r\n")
        );
        // Garbage / dotted id in the uri.
        assert!(
            call(&f, "/spot/api/1/play/context?uri=spotify:album:..%2Fme")
                .contains("error\tbad_uri\r\n")
        );
        assert!(call(&f, "/spot/api/1/play/context?uri=nonsense").contains("error\tbad_uri\r\n"));
        // Missing uri.
        assert!(call(&f, "/spot/api/1/play/context").contains("error\tbad_query\r\n"));
        // Nothing reached the upstream.
        assert!(f.last_context.borrow().is_none());
    }

    #[test]
    fn play_context_no_device_is_no_device_error() {
        let f = Fake {
            no_device: true,
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/play/context?uri=spotify:album:al1");
        assert!(out.contains("error\tno_device\r\n"), "{out}");
    }

    #[test]
    fn play_context_upstream_failure_maps_via_sentinel() {
        let f = Fake {
            control_err: Some(format!("{} spotify cooldown", crate::spotify::RATE_LIMITED)),
            ..fake(playing_track())
        };
        let out = call(&f, "/spot/api/1/play/context?uri=spotify:album:al1");
        assert!(out.contains("error\trate_limited\r\n"), "{out}");
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

    // ---- play/from: native "play from here onward" ------------------------------

    /// Two well-formed 22-char base62 track ids (the spec's wire examples).
    const ID_A: &str = "7hQJA50XrCWABAu5v6QZ4i";
    const ID_B: &str = "22HMAUrbbYSj9NiPPlGumy";

    #[test]
    fn play_from_starts_list_and_returns_now_snapshot() {
        let f = fake(playing_track());
        let out = call(
            &f,
            &format!("/spot/api/1/play/from?ids={ID_A},{ID_B}&offset=1"),
        );
        assert_wire(&out);
        assert_eq!(
            *f.last_play_from.borrow(),
            Some((vec![ID_A.to_string(), ID_B.to_string()], 1))
        );
        assert!(!out.contains("error\t"), "{out}");
        assert!(out.contains("state\tplaying\r\n")); // a /now snapshot, per convention
        assert!(out.contains("ts\t"));
    }

    #[test]
    fn play_from_offset_defaults_to_zero() {
        let f = fake(playing_track());
        call(&f, &format!("/spot/api/1/play/from?ids={ID_A}"));
        assert_eq!(f.last_play_from.borrow().as_ref().unwrap().1, 0);
    }

    #[test]
    fn play_from_missing_or_empty_ids_is_bad_query() {
        let f = fake(playing_track());
        assert!(call(&f, "/spot/api/1/play/from").contains("error\tbad_query\r\n"));
        assert!(call(&f, "/spot/api/1/play/from?ids=").contains("error\tbad_query\r\n"));
        assert!(call(&f, "/spot/api/1/play/from?offset=0").contains("error\tbad_query\r\n"));
    }

    #[test]
    fn play_from_rejects_implausible_ids() {
        let f = fake(playing_track());
        let bad = [
            "short".to_string(),             // not 22 chars
            format!("spotify:track:{ID_A}"), // full uri, not a bare id
            format!("{ID_A},.."),            // GS-02: dotted segment
            format!("{ID_A},,{ID_B}"),       // empty segment
        ];
        for ids in &bad {
            let out = call(&f, &format!("/spot/api/1/play/from?ids={ids}"));
            assert!(out.contains("error\tbad_uri\r\n"), "{ids}: {out}");
        }
        assert!(
            f.last_play_from.borrow().is_none(),
            "a rejected list must never reach spotify"
        );
    }

    #[test]
    fn play_from_offset_out_of_range_is_bad_range() {
        let f = fake(playing_track());
        // ≥ K (the 1-item boundary), non-numeric, negative.
        for sel in [
            format!("/spot/api/1/play/from?ids={ID_A}&offset=1"),
            format!("/spot/api/1/play/from?ids={ID_A}&offset=3"),
            format!("/spot/api/1/play/from?ids={ID_A}&offset=abc"),
            format!("/spot/api/1/play/from?ids={ID_A}&offset=-1"),
        ] {
            assert!(call(&f, &sel).contains("error\tbad_range\r\n"), "{sel}");
        }
        assert!(f.last_play_from.borrow().is_none());
        // The last valid index is fine.
        let ok = call(
            &f,
            &format!("/spot/api/1/play/from?ids={ID_A},{ID_B}&offset=1"),
        );
        assert!(!ok.contains("error\t"), "{ok}");
    }

    #[test]
    fn play_from_caps_the_id_list_at_24() {
        let f = fake(playing_track());
        let ids24 = vec![ID_A; 24].join(",");
        assert!(!call(&f, &format!("/spot/api/1/play/from?ids={ids24}")).contains("error\t"));
        let ids25 = vec![ID_A; 25].join(",");
        assert!(call(&f, &format!("/spot/api/1/play/from?ids={ids25}"))
            .contains("error\tbad_range\r\n"));
    }

    #[test]
    fn play_from_busts_queue_and_now_caches() {
        let f = fake(playing_track());
        now_at(&f, TS); // caches /now (fetch #1)
        let args = DcgiArgs::from_argv(&argv(&format!("/spot/api/1/play/from?ids={ID_A}")));
        route(&args.path(), &args, Some(&f), None, TS + 100);
        assert_eq!(
            *f.queue_busts.borrow(),
            1,
            "a jump replaces the upcoming queue"
        );
        // The fake never flips to ID_A, so the reply settles by exhausting its
        // attempts: the initial poll + the full settle budget, all fresh.
        assert_eq!(
            *f.now_calls.borrow(),
            1 + 4,
            "reply must be freshly fetched (and settled)"
        );
    }

    #[test]
    fn play_from_no_device_is_no_device_error() {
        let f = Fake {
            no_device: true,
            ..fake(playing_track())
        };
        let out = call(&f, &format!("/spot/api/1/play/from?ids={ID_A}"));
        assert!(out.contains("error\tno_device\r\n"), "{out}");
    }

    #[test]
    fn play_from_upstream_failure_maps_via_sentinel() {
        let f = Fake {
            control_err: Some("rate_limited: spotify cooldown active (until ~9)".into()),
            ..fake(playing_track())
        };
        let out = call(&f, &format!("/spot/api/1/play/from?ids={ID_A}"));
        assert!(out.contains("error\trate_limited\r\n"), "{out}");
        let f2 = Fake {
            control_err: Some("spotify HTTP 500: boom".into()),
            ..fake(playing_track())
        };
        let out2 = call(&f2, &format!("/spot/api/1/play/from?ids={ID_A}"));
        assert!(out2.contains("error\tupstream\r\n"), "{out2}");
    }
}
