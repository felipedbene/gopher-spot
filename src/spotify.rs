//! The Spotify Web API surface.
//!
//! [`SpotifyApi`] is the trait the dcgi routes against, so rendering stays
//! testable with a fake (no network). The real [`Client`] (behind the `net`
//! feature) is a BLOCKING ureq client — right for a per-request dcgi — that
//! renews the access token from the refresh token and caches it (plus search /
//! devices results) on disk via [`crate::cache`].
//!
//! librespot exposes no local control API, so all playback control goes through
//! the Web API against the `gopher-spot` Connect device.

use serde::Deserialize;

/// A playback control command (`/spot/control/*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Resume,
    Pause,
    Next,
    Prev,
    Volume(u8),
}

/// Errors are surfaced to the user as a small gopher menu, so a message is enough.
pub type ApiError = String;

// ---- Response models (serde; always compiled so render/tests work offline) ----

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub name: String,
    /// `spotify:artist:…` — playing it as a context starts the artist's top
    /// tracks / radio. Empty if the API omitted it.
    #[serde(default)]
    pub uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub name: String,
    /// `spotify:album:…` — playing it as a context plays the whole album.
    #[serde(default)]
    pub uri: String,
}

/// One entry of an album's `images` array (`GET /v1/albums/{id}`). Spotify serves
/// a few fixed sizes (canonically 640/300/64 px, largest first) from its CDN.
/// `height`/`width` can be absent, so they're optional; the cover picker treats a
/// missing dimension as 0 (smaller than any request).
#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub url: String,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
}

/// A track (subset we render). `uri` drives playback; `id` builds detail links.
#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub name: String,
    #[serde(default)]
    pub artists: Vec<Artist>,
    pub album: Option<Album>,
    pub id: Option<String>,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub duration_ms: u64,
}

impl Track {
    /// Artist names joined with `, ` (empty string if none).
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `GET /v1/me/player/currently-playing` (subset). `item` is null when nothing
/// is playing or the current item isn't a track (e.g. a podcast episode).
#[derive(Debug, Clone, Deserialize)]
pub struct Playing {
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub progress_ms: u64,
    pub item: Option<Track>,
    /// The active device (from `/v1/me/player`). Lets the menu tell whether the
    /// gopher-spot/librespot device — the one the audio stream carries — is what's
    /// actually playing, vs. some other device (phone/desktop) on the account.
    #[serde(default)]
    pub device: Option<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
}

/// `GET /v1/search` (subset): the three result kinds the root menu offers.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub tracks: Option<Page<Track>>,
    #[serde(default)]
    pub artists: Option<Page<Artist>>,
    #[serde(default)]
    pub albums: Option<Page<Album>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_active: bool,
    /// The device's current volume (0-100). Absent for devices that don't report
    /// it (e.g. some Connect endpoints); surfaced as the API `volume` key.
    #[serde(default)]
    pub volume_percent: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: Option<String>,
    pub name: String,
}

/// How many items per page (PROMPT: 20/página).
pub const PAGE_SIZE: u32 = 20;

/// One page of playlists (`/v1/me/playlists`), with the offset that produced it
/// and the grand total, so the renderer can draw prev/next links.
#[derive(Debug, Clone)]
pub struct PlaylistsPage {
    pub items: Vec<Playlist>,
    pub total: u32,
    pub offset: u32,
}

/// One page of tracks (search-less: a playlist's tracks).
#[derive(Debug, Clone)]
pub struct TracksPage {
    pub items: Vec<Track>,
    pub total: u32,
    pub offset: u32,
}

/// Album header for `/spot/album/{id}` (from `/v1/albums/{id}`). The track list
/// is fetched/paged separately via `album_tracks`.
#[derive(Debug, Clone)]
pub struct AlbumDetail {
    pub name: String,
    pub uri: String,
    pub artists: Vec<Artist>,
    pub total: u32,
}

/// One page of albums (an artist's discography), mirroring [`PlaylistsPage`].
#[derive(Debug, Clone)]
pub struct AlbumsPage {
    pub items: Vec<Album>,
    pub total: u32,
    pub offset: u32,
}

/// The pixel dimension of an image (height, else width, else 0 when Spotify
/// omits both).
fn image_dim(i: &Image) -> u32 {
    i.height.or(i.width).unwrap_or(0)
}

/// Pick the album image to serve for a requested size: the smallest image at
/// least as large as `want_px` (so we never upscale a thumbnail), falling back to
/// the largest available when every image is smaller than the request. `None`
/// only when there are no images at all.
pub fn pick_image(images: &[Image], want_px: u32) -> Option<&Image> {
    images
        .iter()
        .filter(|i| image_dim(i) >= want_px)
        .min_by_key(|i| image_dim(i))
        .or_else(|| images.iter().max_by_key(|i| image_dim(i)))
}

/// Extract the trailing id from a `spotify:kind:ID` uri. `None` for empty or
/// malformed input (so callers fall back to a plain, non-clickable line).
pub fn id_from_uri(uri: &str) -> Option<&str> {
    let id = uri.rsplit(':').next()?;
    if id.is_empty() || id == uri {
        None
    } else {
        Some(id)
    }
}

/// The API operations the dcgi needs. Implemented by the real [`Client`] and by
/// test fakes.
pub trait SpotifyApi {
    fn now_playing(&self) -> Result<Playing, ApiError>;
    /// The upcoming queue (`/v1/me/player/queue`). Empty when nothing is queued —
    /// which is what "no more tracks left in queue" looks like to the user.
    fn queue(&self) -> Result<Vec<Track>, ApiError>;
    /// Enqueue a track uri (`POST /v1/me/player/queue`). The caller validates the
    /// uri shape; this just issues the command against the gopher-spot device.
    fn queue_add(&self, uri: &str) -> Result<(), ApiError>;
    /// The album cover JPEG for `album_id` at the size closest to `want_px`
    /// (smallest image ≥ requested, else the largest available). Bytes are cached
    /// on disk (covers are immutable). Errors carry `HTTP 404` for an unknown
    /// album and `no cover` when the album has no images — the API layer maps both
    /// to `not_found`.
    fn album_cover(&self, album_id: &str, want_px: u32) -> Result<Vec<u8>, ApiError>;
    fn search(&self, query: &str) -> Result<SearchResults, ApiError>;
    fn track(&self, id: &str) -> Result<Track, ApiError>;
    /// Start playback of a URI on the `gopher-spot` device.
    fn play(&self, uri: &str) -> Result<(), ApiError>;
    /// Transfer playback to the `gopher-spot` device (`PUT /v1/me/player`). `play`
    /// resumes playback on transfer; `false` transfers without changing the
    /// play/pause state. If the `gopher-spot` device isn't registered (librespot
    /// down), the error message carries `no_device` so the API maps it to that
    /// code rather than a generic `upstream`.
    fn wake(&self, play: bool) -> Result<(), ApiError>;
    fn control(&self, cmd: Control) -> Result<(), ApiError>;
    /// Seek the current track to `position_ms` (`PUT /v1/me/player/seek`). The
    /// caller clamps to the track duration; this just issues the command.
    fn seek(&self, position_ms: u64) -> Result<(), ApiError>;
    /// The user's playlists, paginated (offset in items).
    fn playlists(&self, offset: u32) -> Result<PlaylistsPage, ApiError>;
    /// A playlist's tracks, paginated.
    fn playlist_tracks(&self, id: &str, offset: u32) -> Result<TracksPage, ApiError>;
    /// Album header (title, artists, track count) for a detail page.
    fn album(&self, id: &str) -> Result<AlbumDetail, ApiError>;
    /// An album's tracks, paginated.
    fn album_tracks(&self, id: &str, offset: u32) -> Result<TracksPage, ApiError>;
    /// Artist header (name, uri).
    fn artist(&self, id: &str) -> Result<Artist, ApiError>;
    /// An artist's albums (discography), paginated.
    fn artist_albums(&self, id: &str, offset: u32) -> Result<AlbumsPage, ApiError>;
    /// An artist's top tracks (market inferred from the token).
    fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>, ApiError>;

    // ---- /now micro-cache (fio S3/2) --------------------------------------
    // A rendered `/now` document served from a ~1s TTL cache: a burst of polls
    // collapses to one upstream fetch, and each poll in the window gets the same
    // document (same `ts`) so the client interpolates the delta. The default
    // no-ops disable caching (test fakes that don't exercise it, and the offline
    // path); the real Client backs them with the on-disk TTL cache, keyed on the
    // request wall-clock in ms.

    /// The cached `/now` document if one was stored < ~1s before `now_ms`.
    fn cached_now(&self, _now_ms: i64) -> Option<String> {
        None
    }
    /// Store the just-rendered `/now` `doc`, stamped at `now_ms`, for the TTL.
    fn store_now(&self, _now_ms: i64, _doc: &str) {}
    /// Drop any cached `/now` so the next poll re-fetches. Commands call this so a
    /// state change is never masked by a stale cached snapshot.
    fn invalidate_now_cache(&self) {}
}

// ---- The real blocking client (net feature) --------------------------------

#[cfg(feature = "net")]
pub use net::Client;

#[cfg(feature = "net")]
mod net {
    use super::*;
    use crate::cache;
    use std::io::Read;
    use std::path::PathBuf;

    const API: &str = "https://api.spotify.com";
    const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
    const DEVICE_NAME: &str = "gopher-spot";
    const SEARCH_TTL: i64 = 300; // 5 min
                                 // Spotify's /v1/search rejects limit=20 with 400 "Invalid limit" (the docs
                                 // still say 0-50, but 20 empirically 400s and 10 works — an API quirk). 10 is
                                 // plenty for a Gopher menu.
    const SEARCH_LIMIT: u32 = 10;
    const DEVICES_TTL: i64 = 30; // 30 s
    const PLAYLISTS_TTL: i64 = 60; // 60 s
    const CATALOG_TTL: i64 = 86_400; // 24h — albums/artists are effectively static
    const HTTP_TIMEOUT_SECS: u64 = 10;
    // /now micro-cache window (fio S3/2). Kept in MILLISECONDS: the `now_snapshot`
    // entry is written and read with the request wall-clock in ms (not the seconds
    // clock the other keys use), so the cache module's opaque expiry comparison
    // gives us a true ~1s window. Short enough that the client's ts-interpolation
    // fully absorbs the staleness; long enough to fold a poll burst into one call.
    const NOW_CACHE_TTL_MS: i64 = 1_000;

    #[derive(Deserialize)]
    struct RawPlaylists {
        #[serde(default = "Vec::new")]
        items: Vec<Playlist>,
        #[serde(default)]
        total: u32,
    }

    // /v1/albums/{id} inlines the first page of (simplified) tracks + a total.
    #[derive(Deserialize)]
    struct RawAlbumTracks {
        #[serde(default = "Vec::new")]
        items: Vec<Track>,
        #[serde(default)]
        total: u32,
    }

    #[derive(Deserialize)]
    struct RawAlbum {
        name: String,
        #[serde(default)]
        uri: String,
        #[serde(default)]
        artists: Vec<Artist>,
        // The cover images (fio S2). Parsed from the SAME cached album body the
        // human album page fetches, so a cover costs no extra Spotify call.
        #[serde(default = "Vec::new")]
        images: Vec<Image>,
        tracks: RawAlbumTracks,
    }

    #[derive(Deserialize)]
    struct RawArtistAlbums {
        #[serde(default = "Vec::new")]
        items: Vec<Album>,
        #[serde(default)]
        total: u32,
    }

    #[derive(Deserialize)]
    struct RawTopTracks {
        #[serde(default = "Vec::new")]
        tracks: Vec<Track>,
    }

    #[derive(Deserialize)]
    struct RawPlItem {
        track: Option<Track>,
    }

    #[derive(Deserialize)]
    struct RawPlTracks {
        #[serde(default = "Vec::new")]
        items: Vec<RawPlItem>,
        #[serde(default)]
        total: u32,
    }

    /// A configured Web API client. Cheap to build per request (the dcgi is one
    /// process per request); the disk cache carries the token + results across
    /// invocations.
    pub struct Client {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        state_dir: PathBuf,
        now_unix: i64,
        agent: ureq::Agent,
    }

    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
        #[serde(default)]
        expires_in: i64,
    }

    #[derive(Deserialize)]
    struct Devices {
        #[serde(default = "Vec::new")]
        devices: Vec<Device>,
    }

    impl Client {
        /// Build from the OAuth env (the Secret): `SPOTIFY_CLIENT_ID`,
        /// `SPOTIFY_CLIENT_SECRET`, `SPOTIFY_REFRESH_TOKEN`. Returns `None` if any
        /// is missing, so the dcgi falls back to the offline mock menus.
        pub fn from_env(now_unix: i64, state_dir: PathBuf) -> Option<Client> {
            let client_id = non_empty("SPOTIFY_CLIENT_ID")?;
            let client_secret = non_empty("SPOTIFY_CLIENT_SECRET")?;
            let refresh_token = non_empty("SPOTIFY_REFRESH_TOKEN")?;
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
                .build();
            Some(Client {
                client_id,
                client_secret,
                refresh_token,
                state_dir,
                now_unix,
                agent,
            })
        }

        /// A valid bearer token: the disk-cached one until it nears expiry, else a
        /// fresh refresh (cached with `expires_in - 60s` slack).
        fn access_token(&self) -> Result<String, ApiError> {
            if let Some(t) = cache::get(&self.state_dir, "access_token", self.now_unix) {
                return Ok(t);
            }
            let resp = self
                .agent
                .post(TOKEN_URL)
                .send_form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &self.refresh_token),
                    ("client_id", &self.client_id),
                    ("client_secret", &self.client_secret),
                ])
                .map_err(|e| format!("token refresh failed: {e}"))?;
            let tok: TokenResp = resp
                .into_json()
                .map_err(|e| format!("token parse failed: {e}"))?;
            let ttl = (tok.expires_in - 60).max(30);
            cache::put(
                &self.state_dir,
                "access_token",
                self.now_unix,
                ttl,
                &tok.access_token,
            );
            Ok(tok.access_token)
        }

        fn get(&self, path: &str) -> Result<ureq::Response, ApiError> {
            let token = self.access_token()?;
            self.agent
                .get(&format!("{API}{path}"))
                .set("Authorization", &format!("Bearer {token}"))
                .call()
                .map_err(api_err)
        }

        /// A body-less state change (play/pause/next/prev/volume). Spotify returns
        /// 202/204 with no body on success.
        fn command(&self, method: &str, path: &str) -> Result<(), ApiError> {
            let token = self.access_token()?;
            let req = self
                .agent
                .request(method, &format!("{API}{path}"))
                .set("Authorization", &format!("Bearer {token}"));
            // Spotify rejects a body-less POST/PUT with 411 Length Required, so every
            // command must send an explicit body to carry a Content-Length. PUT
            // play/pause/volume want a JSON object; POST next/previous take an empty
            // body — but it must still be sent as `""` (Content-Length: 0), NOT via
            // `.call()`, which omits the header entirely and triggers the 411.
            let res = if method == "PUT" {
                req.set("Content-Type", "application/json")
                    .send_string("{}")
            } else {
                req.send_string("")
            };
            res.map(|_| ()).map_err(api_err)
        }

        /// Download raw bytes from a full URL (a Spotify CDN image — public, so no
        /// Authorization header). Reads the whole body into memory; cover JPEGs are
        /// a few tens of KB.
        fn download(&self, url: &str) -> Result<Vec<u8>, ApiError> {
            let resp = self.agent.get(url).call().map_err(api_err)?;
            let mut buf = Vec::new();
            resp.into_reader()
                .read_to_end(&mut buf)
                .map_err(|e| format!("cover download read failed: {e}"))?;
            Ok(buf)
        }

        /// GET `path` with the disk cache in front (TTL seconds), returning the
        /// raw JSON body.
        fn get_cached(&self, key: &str, ttl: i64, path: &str) -> Result<String, ApiError> {
            if let Some(c) = cache::get(&self.state_dir, key, self.now_unix) {
                return Ok(c);
            }
            let s = self.get(path)?.into_string().map_err(|e| e.to_string())?;
            cache::put(&self.state_dir, key, self.now_unix, ttl, &s);
            Ok(s)
        }

        /// The `gopher-spot` device id (cached 30s). Falls back to the active
        /// device, then the first one; errors if the account has no devices.
        fn device_id(&self) -> Result<String, ApiError> {
            let body = if let Some(c) = cache::get(&self.state_dir, "devices", self.now_unix) {
                c
            } else {
                let s = self
                    .get("/v1/me/player/devices")?
                    .into_string()
                    .map_err(|e| e.to_string())?;
                cache::put(&self.state_dir, "devices", self.now_unix, DEVICES_TTL, &s);
                s
            };
            let devices: Devices = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            pick_device(&devices.devices)
        }

        /// The `gopher-spot` device id, fetched FRESH (uncached) — wake decides
        /// registration from it, and a 30 s-stale device list could report a
        /// librespot that has since gone down (or miss one that just came up). The
        /// fresh body reseeds the `devices` cache the play path reuses. Returns a
        /// `no_device`-tagged error when the device isn't registered.
        fn gopher_device_id_fresh(&self) -> Result<String, ApiError> {
            let body = self
                .get("/v1/me/player/devices")?
                .into_string()
                .map_err(|e| e.to_string())?;
            cache::put(&self.state_dir, "devices", self.now_unix, DEVICES_TTL, &body);
            let devices: Devices = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            devices
                .devices
                .iter()
                .find(|d| d.name == DEVICE_NAME)
                .and_then(|d| d.id.clone())
                .ok_or_else(|| format!("no_device: '{DEVICE_NAME}' is not registered"))
        }
    }

    impl SpotifyApi for Client {
        fn now_playing(&self) -> Result<Playing, ApiError> {
            // /v1/me/player (not .../currently-playing) so we also get `device`,
            // to tell whether the gopher-spot device — the one the stream carries —
            // is actually the active one. 204 No Content == no active device.
            let resp = self.get("/v1/me/player")?;
            if resp.status() == 204 {
                return Ok(Playing {
                    is_playing: false,
                    progress_ms: 0,
                    item: None,
                    device: None,
                });
            }
            resp.into_json()
                .map_err(|e| format!("now-playing parse failed: {e}"))
        }

        fn queue(&self) -> Result<Vec<Track>, ApiError> {
            let resp = self.get("/v1/me/player/queue")?;
            if resp.status() == 204 {
                return Ok(Vec::new());
            }
            #[derive(Deserialize)]
            struct RawQueue {
                #[serde(default = "Vec::new")]
                queue: Vec<Track>,
            }
            let q: RawQueue = resp
                .into_json()
                .map_err(|e| format!("queue parse failed: {e}"))?;
            Ok(q.queue)
        }

        fn queue_add(&self, uri: &str) -> Result<(), ApiError> {
            // Target the gopher-spot device explicitly (like `play`) so the item
            // lands on the librespot player the audio stream carries. A body-less
            // POST 411s (see `command`), so route through `command` which sends an
            // explicit empty body.
            let device = self.device_id()?;
            self.command(
                "POST",
                &format!(
                    "/v1/me/player/queue?uri={}&device_id={device}",
                    urlencode(uri)
                ),
            )
        }

        fn album_cover(&self, album_id: &str, want_px: u32) -> Result<Vec<u8>, ApiError> {
            // Reuse the album JSON the human album page already caches (24h) — the
            // cover URLs live there, so no extra catalog call.
            let body = self.get_cached(
                &format!("album:{album_id}"),
                CATALOG_TTL,
                &format!("/v1/albums/{album_id}"),
            )?;
            let raw: RawAlbum = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let img = pick_image(&raw.images, want_px)
                .ok_or_else(|| format!("no cover image for album {album_id}"))?;
            // Key the byte cache by the CHOSEN pixel size, not the request: two
            // requested sizes that resolve to the same CDN image share one entry, so
            // we download each distinct cover at most once.
            let picked = image_dim(img);
            let ckey = format!("cover:{album_id}:{picked}");
            if let Some(b) = cache::get_bytes(&self.state_dir, &ckey, self.now_unix) {
                return Ok(b);
            }
            // A cache miss is the only path that hits Spotify's CDN. Do NOT log it to
            // stderr: geomyidae's handlecgi splices the child's stderr into the
            // client socket, so any stray byte here would prepend to and corrupt the
            // JPEG. The cache is observable without a log — via the file that appears
            // under the state dir and the miss-vs-hit latency gap.
            let bytes = self.download(&img.url)?;
            cache::put_bytes(&self.state_dir, &ckey, self.now_unix, CATALOG_TTL, &bytes);
            Ok(bytes)
        }

        fn search(&self, query: &str) -> Result<SearchResults, ApiError> {
            let key = format!("search:{query}");
            if let Some(c) = cache::get(&self.state_dir, &key, self.now_unix) {
                return serde_json::from_str(&c).map_err(|e| e.to_string());
            }
            let path = format!(
                "/v1/search?type=track,album,artist&limit={SEARCH_LIMIT}&q={}",
                urlencode(query)
            );
            let body = self.get(&path)?.into_string().map_err(|e| e.to_string())?;
            cache::put(&self.state_dir, &key, self.now_unix, SEARCH_TTL, &body);
            serde_json::from_str(&body).map_err(|e| format!("search parse failed: {e}"))
        }

        fn track(&self, id: &str) -> Result<Track, ApiError> {
            self.get(&format!("/v1/tracks/{id}"))?
                .into_json()
                .map_err(|e| format!("track parse failed: {e}"))
        }

        fn play(&self, uri: &str) -> Result<(), ApiError> {
            let device = self.device_id()?;
            let token = self.access_token()?;
            // A single track plays via `uris`; an album/artist/playlist is a
            // *context* (`context_uri`) so it plays the whole thing, not one song.
            let body = if uri.starts_with("spotify:track:") {
                serde_json::json!({ "uris": [uri] })
            } else {
                serde_json::json!({ "context_uri": uri })
            }
            .to_string();
            self.agent
                .put(&format!("{API}/v1/me/player/play?device_id={device}"))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&body)
                .map(|_| ())
                .map_err(api_err)
        }

        fn wake(&self, play: bool) -> Result<(), ApiError> {
            // Transfer playback to gopher-spot. device_ids selects the target; the
            // `play` boolean is the Web API's native "resume on transfer" flag.
            let device = self.gopher_device_id_fresh()?;
            let token = self.access_token()?;
            let body = serde_json::json!({ "device_ids": [device], "play": play }).to_string();
            self.agent
                .put(&format!("{API}/v1/me/player"))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&body)
                .map(|_| ())
                .map_err(api_err)
        }

        fn control(&self, cmd: Control) -> Result<(), ApiError> {
            match cmd {
                // Resume with no `uris`: PUT play just un-pauses the current track.
                Control::Resume => self.command("PUT", "/v1/me/player/play"),
                Control::Pause => self.command("PUT", "/v1/me/player/pause"),
                Control::Next => self.command("POST", "/v1/me/player/next"),
                Control::Prev => self.command("POST", "/v1/me/player/previous"),
                Control::Volume(pct) => {
                    let pct = pct.min(100);
                    self.command("PUT", &format!("/v1/me/player/volume?volume_percent={pct}"))
                }
            }
        }

        fn seek(&self, position_ms: u64) -> Result<(), ApiError> {
            self.command(
                "PUT",
                &format!("/v1/me/player/seek?position_ms={position_ms}"),
            )
        }

        fn playlists(&self, offset: u32) -> Result<PlaylistsPage, ApiError> {
            let body = self.get_cached(
                &format!("playlists:{offset}"),
                PLAYLISTS_TTL,
                &format!("/v1/me/playlists?limit={PAGE_SIZE}&offset={offset}"),
            )?;
            let raw: RawPlaylists = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(PlaylistsPage {
                items: raw.items,
                total: raw.total,
                offset,
            })
        }

        fn playlist_tracks(&self, id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            let body = self.get_cached(
                &format!("pltracks:{id}:{offset}"),
                PLAYLISTS_TTL,
                &format!("/v1/playlists/{id}/tracks?limit={PAGE_SIZE}&offset={offset}"),
            )?;
            let raw: RawPlTracks = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let items = raw.items.into_iter().filter_map(|i| i.track).collect();
            Ok(TracksPage {
                items,
                total: raw.total,
                offset,
            })
        }

        fn album(&self, id: &str) -> Result<AlbumDetail, ApiError> {
            let body = self.get_cached(
                &format!("album:{id}"),
                CATALOG_TTL,
                &format!("/v1/albums/{id}"),
            )?;
            let r: RawAlbum = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(AlbumDetail {
                name: r.name,
                uri: r.uri,
                artists: r.artists,
                total: r.tracks.total,
            })
        }

        fn album_tracks(&self, id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            let body = self.get_cached(
                &format!("albumtracks:{id}:{offset}"),
                CATALOG_TTL,
                &format!("/v1/albums/{id}/tracks?limit={PAGE_SIZE}&offset={offset}"),
            )?;
            let r: RawAlbumTracks = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(TracksPage {
                items: r.items,
                total: r.total,
                offset,
            })
        }

        fn artist(&self, id: &str) -> Result<Artist, ApiError> {
            let body = self.get_cached(
                &format!("artist:{id}"),
                CATALOG_TTL,
                &format!("/v1/artists/{id}"),
            )?;
            serde_json::from_str(&body).map_err(|e| format!("artist parse failed: {e}"))
        }

        fn artist_albums(&self, id: &str, offset: u32) -> Result<AlbumsPage, ApiError> {
            let body = self.get_cached(
                &format!("artistalbums:{id}:{offset}"),
                CATALOG_TTL,
                &format!(
                    "/v1/artists/{id}/albums?include_groups=album,single&limit={PAGE_SIZE}&offset={offset}"
                ),
            )?;
            let r: RawArtistAlbums = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(AlbumsPage {
                items: r.items,
                total: r.total,
                offset,
            })
        }

        fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>, ApiError> {
            // market=from_token: Spotify infers the market from the user token, so
            // we don't need to store the account country.
            let body = self.get_cached(
                &format!("artisttop:{id}"),
                CATALOG_TTL,
                &format!("/v1/artists/{id}/top-tracks?market=from_token"),
            )?;
            let r: RawTopTracks = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(r.tracks)
        }

        // ---- /now micro-cache (fio S3/2) ----------------------------------
        // Backed by the on-disk TTL cache under a fixed `now_snapshot` key, but
        // clocked in MS (we pass now_ms where the module wants a "now"), so the
        // TTL is a real ~1s window regardless of the seconds clock the token/
        // search/catalog entries use. Per-replica, like every other cache entry.
        fn cached_now(&self, now_ms: i64) -> Option<String> {
            cache::get(&self.state_dir, "now_snapshot", now_ms)
        }
        fn store_now(&self, now_ms: i64, doc: &str) {
            cache::put(
                &self.state_dir,
                "now_snapshot",
                now_ms,
                NOW_CACHE_TTL_MS,
                doc,
            );
        }
        fn invalidate_now_cache(&self) {
            cache::remove(&self.state_dir, "now_snapshot");
        }
    }

    fn non_empty(var: &str) -> Option<String> {
        std::env::var(var).ok().filter(|v| !v.is_empty())
    }

    /// Map a ureq error to a user message, unwrapping HTTP status responses (a
    /// 401/403/404 from Spotify) into their status line rather than a generic
    /// transport error.
    fn api_err(e: ureq::Error) -> ApiError {
        match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(160).collect();
                format!("spotify HTTP {code}: {snippet}")
            }
            ureq::Error::Transport(t) => format!("spotify transport error: {t}"),
        }
    }

    /// Minimal percent-encoding for a search query (RFC 3986 unreserved kept).
    fn urlencode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// Prefer the `gopher-spot` device, then the active one, then the first with
    /// an id.
    fn pick_device(devices: &[Device]) -> Result<String, ApiError> {
        devices
            .iter()
            .find(|d| d.name == DEVICE_NAME)
            .and_then(|d| d.id.clone())
            .or_else(|| {
                devices
                    .iter()
                    .find(|d| d.is_active)
                    .and_then(|d| d.id.clone())
            })
            .or_else(|| devices.iter().find_map(|d| d.id.clone()))
            .ok_or_else(|| {
                format!("nenhum device ativo (abra o Spotify no device '{DEVICE_NAME}')")
            })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn url_encoding_escapes_spaces_and_accents() {
            assert_eq!(urlencode("chico buarque"), "chico%20buarque");
            assert_eq!(urlencode("construção"), "constru%C3%A7%C3%A3o");
        }

        fn dev(name: &str, id: Option<&str>, active: bool) -> Device {
            Device {
                id: id.map(String::from),
                name: name.into(),
                is_active: active,
                volume_percent: None,
            }
        }

        #[test]
        fn device_pick_prefers_gopher_spot() {
            let ds = vec![
                dev("iPhone", Some("aa"), true),
                dev("gopher-spot", Some("bb"), false),
            ];
            assert_eq!(pick_device(&ds).unwrap(), "bb");
        }

        #[test]
        fn device_pick_falls_back_to_active_then_first() {
            let active = vec![
                dev("iPhone", Some("aa"), true),
                dev("Echo", Some("cc"), false),
            ];
            assert_eq!(pick_device(&active).unwrap(), "aa");
            let first = vec![dev("Echo", Some("cc"), false)];
            assert_eq!(pick_device(&first).unwrap(), "cc");
            assert!(pick_device(&[]).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currently_playing_parses_and_joins_artists() {
        let j = r#"{
            "is_playing": true, "progress_ms": 42000,
            "item": {
                "name": "Construção",
                "artists": [{"name": "Chico Buarque"}],
                "album": {"name": "Construção"},
                "id": "abc123", "uri": "spotify:track:abc123", "duration_ms": 380000
            }
        }"#;
        let p: Playing = serde_json::from_str(j).unwrap();
        assert!(p.is_playing);
        let t = p.item.unwrap();
        assert_eq!(t.name, "Construção");
        assert_eq!(t.artist_line(), "Chico Buarque");
        assert_eq!(t.uri, "spotify:track:abc123");
    }

    #[test]
    fn nothing_playing_when_item_null() {
        let p: Playing = serde_json::from_str(r#"{"is_playing": false, "item": null}"#).unwrap();
        assert!(p.item.is_none());
    }

    #[test]
    fn id_from_uri_extracts_or_none() {
        assert_eq!(id_from_uri("spotify:album:qd"), Some("qd"));
        assert_eq!(id_from_uri("spotify:artist:sm"), Some("sm"));
        assert_eq!(id_from_uri(""), None);
        assert_eq!(id_from_uri("nope"), None);
        assert_eq!(id_from_uri("spotify:album:"), None);
    }

    #[test]
    fn pick_image_prefers_smallest_ge_then_largest() {
        let imgs = vec![
            Image { url: "big".into(), height: Some(640), width: Some(640) },
            Image { url: "mid".into(), height: Some(300), width: Some(300) },
            Image { url: "sm".into(), height: Some(64), width: Some(64) },
        ];
        // exact / smallest-at-least matches
        assert_eq!(pick_image(&imgs, 64).unwrap().url, "sm");
        assert_eq!(pick_image(&imgs, 300).unwrap().url, "mid");
        assert_eq!(pick_image(&imgs, 640).unwrap().url, "big");
        // between sizes rounds UP to the next available
        assert_eq!(pick_image(&imgs, 65).unwrap().url, "mid");
        // larger than everything -> largest available (fallback)
        assert_eq!(pick_image(&imgs, 1000).unwrap().url, "big");
        // no images -> None
        assert!(pick_image(&[], 64).is_none());
    }

    #[test]
    fn search_results_parse_tracks() {
        let j = r#"{"tracks":{"items":[
            {"name":"A","artists":[{"name":"X"},{"name":"Y"}],"album":{"name":"Al"},"id":"1","uri":"spotify:track:1"}
        ]}}"#;
        let r: SearchResults = serde_json::from_str(j).unwrap();
        let tracks = r.tracks.unwrap().items;
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].artist_line(), "X, Y");
    }
}
