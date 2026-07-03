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
use crate::spotify::{Control, Playing, SpotifyApi};

/// The contract version, emitted as the leading `api` key on every response.
pub const API_VERSION: u32 = 1;

/// Route a `/spot/api/1/*` request to its tab-delimited text document. `now_ms`
/// is the dcgi's request wall-clock (unix epoch ms), stamped as `ts` so the
/// client can interpolate the progress bar between polls.
pub fn route(path: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>, now_ms: i64) -> String {
    // Only v1 exists. Everything else is a versioned 404 (a future /spot/api/2
    // would be routed here too).
    let sub = match path.strip_prefix("/spot/api/1") {
        Some(s) => s.trim_matches('/'),
        None => return error("not_found", "unknown api version"),
    };
    // No OAuth Secret configured -> the whole upstream is unavailable. Report it
    // in-contract rather than serving a human mock menu.
    let api = match api {
        Some(a) => a,
        None => return error("upstream", "spotify api not configured"),
    };
    match sub {
        "now" => snapshot_or_error(api, now_ms),
        "play" => command(api, now_ms, Control::Resume),
        "pause" => command(api, now_ms, Control::Pause),
        "next" => command(api, now_ms, Control::Next),
        "prev" => command(api, now_ms, Control::Prev),
        "volume" => volume(api, args, now_ms),
        "seek" => seek(api, args, now_ms),
        other => error("not_found", &format!("unknown endpoint: {other}")),
    }
}

/// `/volume?<0-100>`: continuous. Out of range (or non-integer) -> `bad_range`.
fn volume(api: &dyn SpotifyApi, args: &DcgiArgs, now_ms: i64) -> String {
    match args.raw_arg().trim().parse::<i64>() {
        Ok(v) if (0..=100).contains(&v) => match api.control(Control::Volume(v as u8)) {
            Ok(()) => snapshot_or_error(api, now_ms),
            Err(e) => error("upstream", &e),
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
        Err(e) => return error("upstream", &e),
    };
    let duration = match playing.item.as_ref().map(|t| t.duration_ms) {
        Some(d) if d > 0 => d,
        _ => return error("no_track", "nothing playing to seek"),
    };
    match api.seek(pos.min(duration)) {
        Ok(()) => snapshot_or_error(api, now_ms),
        Err(e) => error("upstream", &e),
    }
}

/// Run a play/pause/next/prev command, then reply with a fresh snapshot. For the
/// idempotent pair (`play`/`pause`), Spotify 403s "Restriction violated" when the
/// player is already in the requested state — swallow that and return the
/// snapshot, so `play` while playing is a no-op success (contract rule).
fn command(api: &dyn SpotifyApi, now_ms: i64, cmd: Control) -> String {
    match api.control(cmd) {
        Ok(()) => snapshot_or_error(api, now_ms),
        Err(_) if already_in_state(api, cmd) => snapshot_or_error(api, now_ms),
        Err(e) => error("upstream", &e),
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

/// Fetch the current state and render a `/now` snapshot. Queue length is
/// best-effort (never blocks the snapshot on it), mirroring the human Now Playing.
fn snapshot_or_error(api: &dyn SpotifyApi, now_ms: i64) -> String {
    let playing = match api.now_playing() {
        Ok(p) => p,
        Err(e) => return error("upstream", &e),
    };
    let queue_len = api.queue().map(|q| q.len()).unwrap_or(0);
    snapshot(&playing, queue_len, now_ms)
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
        }
        if let Some(id) = &t.id {
            kv(&mut out, "track_id", id);
        }
        kv(&mut out, "position_ms", &p.progress_ms.to_string());
        kv(&mut out, "duration_ms", &t.duration_ms.to_string());
    }
    if let Some(vol) = p.device.as_ref().and_then(|d| d.volume_percent) {
        kv(&mut out, "volume", &vol.to_string());
    }
    kv(&mut out, "queue_len", &queue_len.to_string());
    kv(&mut out, "ts", &now_ms.to_string());
    out
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
    use crate::spotify::{Album, ApiError, Artist, Device, SearchResults, Track};
    use crate::spotify::{AlbumDetail, AlbumsPage, PlaylistsPage, TracksPage};
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
                uri: String::new(),
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
        control_fails: bool,
        last: RefCell<Option<Control>>,
        last_seek: RefCell<Option<u64>>,
    }
    fn fake(playing: Playing) -> Fake {
        Fake {
            playing,
            control_fails: false,
            last: RefCell::new(None),
            last_seek: RefCell::new(None),
        }
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
            Ok(self.playing.clone())
        }
        fn queue(&self) -> Result<Vec<Track>, ApiError> {
            Ok(vec![track("Deus lhe Pague"), track("Cotidiano")])
        }
        fn control(&self, cmd: Control) -> Result<(), ApiError> {
            *self.last.borrow_mut() = Some(cmd);
            if self.control_fails {
                Err("spotify HTTP 403: Restriction violated".into())
            } else {
                Ok(())
            }
        }
        fn seek(&self, position_ms: u64) -> Result<(), ApiError> {
            *self.last_seek.borrow_mut() = Some(position_ms);
            Ok(())
        }
        fn search(&self, _q: &str) -> Result<SearchResults, ApiError> {
            unimplemented!()
        }
        fn track(&self, _id: &str) -> Result<Track, ApiError> {
            unimplemented!()
        }
        fn play(&self, _uri: &str) -> Result<(), ApiError> {
            unimplemented!()
        }
        fn playlists(&self, _o: u32) -> Result<PlaylistsPage, ApiError> {
            unimplemented!()
        }
        fn playlist_tracks(&self, _id: &str, _o: u32) -> Result<TracksPage, ApiError> {
            unimplemented!()
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

    fn call(f: &Fake, selector: &str) -> String {
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
        assert!(out.contains("track_id\tabc123\r\n"));
        assert!(out.contains("position_ms\t42000\r\n"));
        assert!(out.contains("duration_ms\t380000\r\n"));
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
            control_fails: true,
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
            control_fails: true,
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
        let out = route(&args.path(), &args, None, TS);
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
}
