//! The dynamic `/spot/*` entry point. geomyidae runs `spot/index.dcgi` for any
//! non-existent `/spot/...` selector, calling it
//!   index.dcgi $search $arguments $host $port $traversal $selector
//! and interpreting stdout as a gophermap (`.gph`). We route on the selector and
//! emit menus via `gopher-core`.
//!
//! Routing takes an `Option<&dyn SpotifyApi>`: `Some` on the live path (Fio C),
//! `None` when the OAuth Secret is absent, which degrades gracefully to the Fio B
//! mock menus. This keeps `route` testable with a fake API — no network.

use gopher_core::{info, link, render_menu_index, Entry, ItemKind};

use crate::menu::{self, clip};
use crate::spotify::{
    Control, Playing, PlaylistsPage, SearchResults, SpotifyApi, Track, TracksPage, PAGE_SIZE,
};

/// The six arguments geomyidae hands a dcgi, in its documented order
/// (`$search $arguments $host $port $traversal $selector`).
#[derive(Debug, Clone, Default)]
pub struct DcgiArgs {
    /// argv[1] — the type-7 search term (after a TAB). Carries the query for
    /// `/spot/search`.
    pub search: String,
    /// argv[2] — the query string after `?` in the selector (e.g. `uri=...`).
    pub arguments: String,
    /// argv[3] / argv[4] — the SERVER's host/port (what geomyidae advertises).
    pub host: String,
    pub port: String,
    /// argv[5] — the unreachable path portion. Kept for completeness.
    pub traversal: String,
    /// argv[6] — the full original request selector (`/spot/now`).
    pub selector: String,
}

impl DcgiArgs {
    /// Parse argv *excluding* the program name and the `dcgi` subcommand
    /// (i.e. `std::env::args()[2..]`).
    pub fn from_argv(rest: &[String]) -> DcgiArgs {
        let get = |i: usize| rest.get(i).cloned().unwrap_or_default();
        DcgiArgs {
            search: get(0),
            arguments: get(1),
            host: get(2),
            port: get(3),
            traversal: get(4),
            selector: get(5),
        }
    }

    /// The route path: the selector with any `?query`/`\tsearch` suffix stripped
    /// and a trailing slash normalized off (except the bare root).
    pub fn path(&self) -> String {
        let raw = if self.selector.is_empty() {
            &self.traversal
        } else {
            &self.selector
        };
        let p = raw.split(['?', '\t']).next().unwrap_or(raw);
        if p.len() > 1 && p.ends_with('/') {
            p.trim_end_matches('/').to_string()
        } else {
            p.to_string()
        }
    }

    /// Look up a `key=value` query parameter, from `arguments` first (geomyidae's
    /// post-`?` field) then any `?...` on the selector. Percent-decoded.
    pub fn query(&self, key: &str) -> Option<String> {
        let from_sel = self.selector.split_once('?').map(|(_, q)| q).unwrap_or("");
        for src in [self.arguments.as_str(), from_sel] {
            for pair in src.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    if k == key {
                        return Some(urldecode(v));
                    }
                }
            }
        }
        None
    }
}

/// Route a request to its gophermap. `api` is `Some` on the live path, `None` for
/// the offline mock (no OAuth Secret configured).
pub fn route(args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let path = args.path();
    match path.as_str() {
        // The section root serves the same menu as the baked /srv/index.gph.
        "" | "/" | "/spot" => menu::root_gph(),
        "/spot/now" => match api {
            Some(a) => match a.now_playing() {
                Ok(p) => page(now_entries(&p)),
                Err(e) => page(error_entries(&e)),
            },
            None => mock("Now Playing", "(mock) nada tocando -- configure o Secret OAuth"),
        },
        "/spot/search" => search(args, api),
        "/spot/control" => page(control_menu()),
        p if p.starts_with("/spot/control/") => control(&p["/spot/control/".len()..], api),
        p if p.starts_with("/spot/track/") => track(&p["/spot/track/".len()..], api),
        "/spot/play" => play(args, api),
        "/spot/playlists" => playlists(args, api),
        p if p.starts_with("/spot/playlists/") => {
            playlist(&p["/spot/playlists/".len()..], args, api)
        }
        p if p.starts_with("/spot/") => mock("Em construcao", &format!("rota {p} ainda nao implementada")),
        p => page(not_found_entries(p)),
    }
}

/// Render a `.gph` from an entry list.
fn page(entries: Vec<Entry>) -> String {
    render_menu_index(&entries)
}

// ---- endpoint handlers -----------------------------------------------------

fn search(args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let query = args.search.trim();
    if query.is_empty() {
        return page(vec![
            info("Buscar"),
            info(""),
            info("Selecione 'Buscar' e digite um termo (busca tipo-7)."),
            info(""),
            link(ItemKind::Menu, "Voltar ao menu", "/"),
        ]);
    }
    match api {
        Some(a) => match a.search(query) {
            Ok(r) => page(search_entries(query, &r)),
            Err(e) => page(error_entries(&e)),
        },
        None => search_mock(query),
    }
}

fn track(id: &str, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    match api {
        Some(a) => match a.track(id) {
            Ok(t) => page(track_entries(&t)),
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Faixa", &format!("(mock) detalhe da faixa {id} -- Fio C")),
    }
}

fn play(args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let uri = match args.query("uri") {
        Some(u) if !u.is_empty() => u,
        _ => return page(error_entries("play sem ?uri=")),
    };
    match api {
        Some(a) => match a.play(&uri) {
            Ok(()) => page(vec![
                info("Mandando tocar no gopher-spot..."),
                info(clip(&uri)),
                info(""),
                info("(abra o MacAST no bookmark do stream de audio)"),
                info(""),
                link(ItemKind::Menu, "Now Playing", "/spot/now"),
                link(ItemKind::Menu, "Voltar ao menu", "/"),
            ]),
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Tocar", &format!("(mock) tocaria {uri} -- Fio C")),
    }
}

fn playlists(args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let offset = args.query("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    match api {
        Some(a) => match a.playlists(offset) {
            Ok(p) => page(playlists_entries(&p)),
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Minhas playlists", "(mock) sem playlists -- configure o Secret OAuth"),
    }
}

fn playlist(id: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    let offset = args.query("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    match api {
        Some(a) => match a.playlist_tracks(id, offset) {
            Ok(t) => page(playlist_tracks_entries(id, &t)),
            // Spotify 403/404s editorial & algorithmic playlists (Discover Weekly,
            // Daily Mix, Release Radar…) and other users' private ones — since the
            // Nov-2024 Web API dev-mode restriction, apps can't read them. These
            // still appear in /v1/me/playlists, so the user can click into one.
            // Show a plain note instead of a raw "spotify HTTP 403".
            Err(e) if e.contains("HTTP 403") || e.contains("HTTP 404") => {
                page(playlist_unavailable_entries())
            }
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Playlist", &format!("(mock) faixas da playlist {id} -- Fio D")),
    }
}

fn control(sub: &str, api: Option<&dyn SpotifyApi>) -> String {
    let sub = sub.trim_end_matches('/');
    let (cmd, label) = if let Some(n) = sub.strip_prefix("vol/") {
        match n.parse::<u8>() {
            Ok(pct) => (Control::Volume(pct), format!("volume {pct}%")),
            Err(_) => return page(error_entries(&format!("volume invalido: {n}"))),
        }
    } else {
        match sub {
            "play" => (Control::Resume, "tocando".to_string()),
            "pause" => (Control::Pause, "pausado".to_string()),
            "next" => (Control::Next, "proxima".to_string()),
            "prev" => (Control::Prev, "anterior".to_string()),
            other => return page(not_found_entries(&format!("/spot/control/{other}"))),
        }
    };
    match api {
        Some(a) => match a.control(cmd) {
            // Back to Now Playing (controls live there now), not the old submenu.
            Ok(()) => page(vec![
                info(format!("ok: {label}")),
                info(""),
                link(ItemKind::Menu, "Now Playing", "/spot/now"),
                link(ItemKind::Menu, "Voltar ao menu", "/"),
            ]),
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Controle", &format!("(mock) {label} -- Fio C")),
    }
}

// ---- entry builders --------------------------------------------------------

fn now_entries(p: &Playing) -> Vec<Entry> {
    let mut e = vec![info("Now Playing"), info("")];
    match &p.item {
        Some(t) => {
            e.push(info(clip(&t.name)));
            e.push(info(clip(&format!("por {}", t.artist_line()))));
            if let Some(al) = &t.album {
                e.push(info(clip(&format!("album: {}", al.name))));
            }
            e.push(info(if p.is_playing { "[tocando]" } else { "[pausado]" }));
            // If the active device isn't gopher-spot, the audio stream (which only
            // carries librespot's output) is NOT what this menu shows — warn, so a
            // stale/other-device track doesn't look like what you hear.
            if let Some(dev) = &p.device {
                if dev.name != "gopher-spot" {
                    e.push(info(clip(&format!("(tocando em {}, nao no", dev.name))));
                    e.push(info(" gopher-spot -- transfira o playback)"));
                }
            }
            e.push(info(""));
            // Controls inline (not behind a submenu) — the menu re-renders on each
            // fetch, so the play/pause toggle always offers the opposite of the
            // current state.
            if p.is_playing {
                e.push(link(ItemKind::Menu, "Pausar", "/spot/control/pause"));
            } else {
                e.push(link(ItemKind::Menu, "Tocar", "/spot/control/play"));
            }
            e.push(link(ItemKind::Menu, "<< Anterior", "/spot/control/prev"));
            e.push(link(ItemKind::Menu, ">> Proxima", "/spot/control/next"));
            e.push(link(ItemKind::Menu, "Volume 30%", "/spot/control/vol/30"));
            e.push(link(ItemKind::Menu, "Volume 70%", "/spot/control/vol/70"));
            e.push(link(ItemKind::Menu, "Volume 100%", "/spot/control/vol/100"));
        }
        None => {
            e.push(info("Nada tocando agora."));
            e.push(info("Transfira o playback pro device gopher-spot."));
        }
    }
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
    e
}

fn search_entries(query: &str, r: &SearchResults) -> Vec<Entry> {
    let mut e = vec![info(clip(&format!("Resultados para: {query}"))), info("")];
    let tracks = r.tracks.as_ref().map(|p| p.items.as_slice()).unwrap_or(&[]);
    if tracks.is_empty() {
        e.push(info("Nenhuma faixa encontrada."));
    } else {
        e.push(info("Faixas:"));
        for t in tracks {
            if let Some(id) = &t.id {
                let disp = clip(&format!("{} - {}", t.name, t.artist_line()));
                e.push(link(ItemKind::Menu, disp, format!("/spot/track/{id}")));
            }
        }
    }
    // Artists / albums are shown as context (not playable in Fio C).
    if let Some(artists) = r.artists.as_ref().filter(|p| !p.items.is_empty()) {
        e.push(info(""));
        e.push(info("Artistas:"));
        for a in artists.items.iter().take(5) {
            e.push(info(clip(&format!("  {}", a.name))));
        }
    }
    if let Some(albums) = r.albums.as_ref().filter(|p| !p.items.is_empty()) {
        e.push(info(""));
        e.push(info("Albuns:"));
        for a in albums.items.iter().take(5) {
            e.push(info(clip(&format!("  {}", a.name))));
        }
    }
    e.push(info(""));
    e.push(link(ItemKind::Search, "Buscar de novo", "/spot/search"));
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
    e
}

fn track_entries(t: &Track) -> Vec<Entry> {
    let mut e = vec![
        info(clip(&t.name)),
        info(clip(&format!("por {}", t.artist_line()))),
    ];
    if let Some(al) = &t.album {
        e.push(info(clip(&format!("album: {}", al.name))));
    }
    if t.duration_ms > 0 {
        let secs = t.duration_ms / 1000;
        e.push(info(format!("duracao: {}:{:02}", secs / 60, secs % 60)));
    }
    e.push(info(""));
    // ASCII ">>" rather than the U+25B6 triangle (not in MacRoman).
    if !t.uri.is_empty() {
        e.push(link(ItemKind::Menu, ">> Tocar agora", format!("/spot/play?uri={}", t.uri)));
    }
    e.push(link(ItemKind::Menu, "Controles", "/spot/control"));
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
    e
}

fn playlists_entries(p: &PlaylistsPage) -> Vec<Entry> {
    let mut e = vec![info("Minhas playlists"), info("")];
    if p.items.is_empty() {
        e.push(info("Nenhuma playlist."));
    } else {
        for pl in &p.items {
            if let Some(id) = &pl.id {
                e.push(link(ItemKind::Menu, clip(&pl.name), format!("/spot/playlists/{id}")));
            }
        }
    }
    append_pager(&mut e, "/spot/playlists", p.offset, p.items.len(), p.total);
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
    e
}

fn playlist_tracks_entries(id: &str, t: &TracksPage) -> Vec<Entry> {
    let mut e = vec![info("Faixas da playlist"), info("")];
    if t.items.is_empty() {
        e.push(info("Playlist vazia (ou fim da lista)."));
    } else {
        for track in &t.items {
            if let Some(tid) = &track.id {
                let disp = clip(&format!("{} - {}", track.name, track.artist_line()));
                e.push(link(ItemKind::Menu, disp, format!("/spot/track/{tid}")));
            }
        }
    }
    append_pager(&mut e, &format!("/spot/playlists/{id}"), t.offset, t.items.len(), t.total);
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
    e
}

/// Append "Pagina anterior" / "Proxima pagina" links (20/page) when the offset
/// window leaves items on either side. `base` is the selector to re-request with
/// a new `?offset=`.
fn append_pager(e: &mut Vec<Entry>, base: &str, offset: u32, shown: usize, total: u32) {
    let has_prev = offset > 0;
    let has_next = offset + (shown as u32) < total;
    if has_prev || has_next {
        e.push(info(""));
    }
    if has_prev {
        let prev = offset.saturating_sub(PAGE_SIZE);
        e.push(link(ItemKind::Menu, "<< Pagina anterior", format!("{base}?offset={prev}")));
    }
    if has_next {
        let next = offset + PAGE_SIZE;
        e.push(link(ItemKind::Menu, ">> Proxima pagina", format!("{base}?offset={next}")));
    }
}

fn control_menu() -> Vec<Entry> {
    vec![
        info("Controles"),
        info(""),
        link(ItemKind::Menu, "Tocar", "/spot/control/play"),
        link(ItemKind::Menu, "Pause", "/spot/control/pause"),
        link(ItemKind::Menu, "Proxima", "/spot/control/next"),
        link(ItemKind::Menu, "Anterior", "/spot/control/prev"),
        link(ItemKind::Menu, "Volume 30%", "/spot/control/vol/30"),
        link(ItemKind::Menu, "Volume 70%", "/spot/control/vol/70"),
        link(ItemKind::Menu, "Volume 100%", "/spot/control/vol/100"),
        info(""),
        link(ItemKind::Menu, "Now Playing", "/spot/now"),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ]
}

fn playlist_unavailable_entries() -> Vec<Entry> {
    vec![
        info("Playlist indisponivel"),
        info(""),
        info("O Spotify nao permite ler esta playlist"),
        info("pela Web API (editorial/algoritmica, ou"),
        info("privada de outro usuario)."),
        info(""),
        link(ItemKind::Menu, "Minhas playlists", "/spot/playlists"),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ]
}

fn error_entries(msg: &str) -> Vec<Entry> {
    vec![
        info("Erro"),
        info(""),
        info(clip(msg)),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ]
}

fn not_found_entries(path: &str) -> Vec<Entry> {
    vec![
        info("404 -- selector desconhecido"),
        info(clip(&format!("  {path}"))),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ]
}

// ---- mock helpers (offline / no-Secret path) -------------------------------

fn mock(title: &str, body: &str) -> String {
    page(vec![
        info(title),
        info(""),
        info(body),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

fn search_mock(query: &str) -> String {
    page(vec![
        info("Buscar"),
        info(""),
        info(clip(&format!("(mock) buscaria por: {query} -- configure o Secret OAuth"))),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

/// Minimal percent-decode for query values (`%XX` and `+` -> space).
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(h as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::{Album, ApiError, Artist, Page, Playlist};

    fn argv(search: &str, selector: &str) -> Vec<String> {
        let (_sel, arguments) = selector.split_once('?').unwrap_or((selector, ""));
        vec![
            search.into(),
            arguments.into(),
            "10.0.100.9".into(),
            "70".into(),
            "".into(),
            selector.into(),
        ]
    }

    // A fake API with canned responses, to test routing/render without network.
    struct Fake {
        playing: Playing,
    }
    impl SpotifyApi for Fake {
        fn now_playing(&self) -> Result<Playing, ApiError> {
            Ok(self.playing.clone())
        }
        fn search(&self, q: &str) -> Result<SearchResults, ApiError> {
            Ok(SearchResults {
                tracks: Some(Page {
                    items: vec![Track {
                        name: format!("Faixa {q}"),
                        artists: vec![Artist { name: "Chico".into() }],
                        album: Some(Album { name: "Al".into() }),
                        id: Some("tid".into()),
                        uri: "spotify:track:tid".into(),
                        duration_ms: 0,
                    }],
                }),
                artists: None,
                albums: None,
            })
        }
        fn track(&self, id: &str) -> Result<Track, ApiError> {
            Ok(Track {
                name: format!("Track {id}"),
                artists: vec![Artist { name: "Chico".into() }],
                album: Some(Album { name: "Al".into() }),
                id: Some(id.into()),
                uri: format!("spotify:track:{id}"),
                duration_ms: 380000,
            })
        }
        fn play(&self, _uri: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn control(&self, _cmd: Control) -> Result<(), ApiError> {
            Ok(())
        }
        fn playlists(&self, offset: u32) -> Result<PlaylistsPage, ApiError> {
            // 25 playlists total, 20 per page: page 0 has 20, page 20 has 5.
            let total = 25u32;
            let n = (total.saturating_sub(offset)).min(20);
            let items = (0..n)
                .map(|i| Playlist {
                    id: Some(format!("pl{}", offset + i)),
                    name: format!("Playlist {}", offset + i),
                })
                .collect();
            Ok(PlaylistsPage { items, total, offset })
        }
        fn playlist_tracks(&self, _id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            let items = vec![Track {
                name: "Faixa X".into(),
                artists: vec![Artist { name: "Chico".into() }],
                album: Some(Album { name: "Al".into() }),
                id: Some("tx".into()),
                uri: "spotify:track:tx".into(),
                duration_ms: 0,
            }];
            Ok(TracksPage { items, total: 1, offset })
        }
    }

    fn fake() -> Fake {
        Fake {
            playing: Playing {
                is_playing: true,
                progress_ms: 1000,
                item: Some(Track {
                    name: "Construção".into(),
                    artists: vec![Artist { name: "Chico Buarque".into() }],
                    album: Some(Album { name: "Construção".into() }),
                    id: Some("abc".into()),
                    uri: "spotify:track:abc".into(),
                    duration_ms: 380000,
                }),
                device: None,
            },
        }
    }

    fn r(search: &str, selector: &str, api: Option<&dyn SpotifyApi>) -> String {
        route(&DcgiArgs::from_argv(&argv(search, selector)), api)
    }

    #[test]
    fn now_playing_renders_track() {
        let f = fake();
        let out = r("", "/spot/now", Some(&f));
        assert!(out.contains("Construção"));
        assert!(out.contains("Chico Buarque"));
        assert!(out.contains("[tocando]"));
    }

    #[test]
    fn search_lists_track_links() {
        let f = fake();
        let out = r("chico", "/spot/search", Some(&f));
        assert!(out.contains("[1|Faixa chico - Chico|/spot/track/tid|server|port]"));
    }

    #[test]
    fn track_has_play_link_with_uri() {
        let f = fake();
        let out = r("", "/spot/track/xyz", Some(&f));
        assert!(out.contains("[1|>> Tocar agora|/spot/play?uri=spotify:track:xyz|server|port]"));
    }

    #[test]
    fn play_parses_uri_from_query() {
        let f = fake();
        let out = r("", "/spot/play?uri=spotify:track:abc", Some(&f));
        assert!(out.contains("Mandando tocar"));
        assert!(out.contains("spotify:track:abc"));
    }

    #[test]
    fn control_volume_parses_percent() {
        let f = fake();
        assert!(r("", "/spot/control/vol/70", Some(&f)).contains("ok: volume 70%"));
        assert!(r("", "/spot/control/pause", Some(&f)).contains("ok: pausado"));
    }

    #[test]
    fn control_bad_volume_errors() {
        let f = fake();
        assert!(r("", "/spot/control/vol/abc", Some(&f)).contains("Erro"));
    }

    #[test]
    fn none_api_falls_back_to_mock() {
        assert!(r("", "/spot/now", None).contains("(mock)"));
        assert!(r("chico", "/spot/search", None).contains("(mock)"));
    }

    #[test]
    fn root_and_unknown_still_work() {
        assert!(r("", "/spot", None).contains("[7|Buscar|/spot/search|server|port]"));
        assert!(r("", "/bogus", None).contains("404"));
    }

    #[test]
    fn every_route_is_a_tabless_gophermap() {
        let f = fake();
        for sel in [
            "/spot", "/spot/now", "/spot/search", "/spot/control", "/spot/control/next",
            "/spot/track/abc", "/spot/play?uri=spotify:track:abc",
            "/spot/playlists", "/spot/playlists?offset=20", "/spot/playlists/pl0",
        ] {
            let out = r("q", sel, Some(&f));
            assert!(!out.contains('\t'), "tabs in {sel}");
            for line in out.lines() {
                if line.starts_with('[') {
                    assert!(line.ends_with(']'), "malformed link in {sel}: {line}");
                }
            }
        }
    }

    #[test]
    fn playlists_list_links_and_pages() {
        let f = fake();
        let out = r("", "/spot/playlists", Some(&f));
        assert!(out.contains("[1|Playlist 0|/spot/playlists/pl0|server|port]"));
        // 25 total, page 0 -> next page link, no prev
        assert!(out.contains("[1|>> Proxima pagina|/spot/playlists?offset=20|server|port]"));
        assert!(!out.contains("Pagina anterior"));
        // second page -> prev link, no next (5 left)
        let out2 = r("", "/spot/playlists?offset=20", Some(&f));
        assert!(out2.contains("[1|<< Pagina anterior|/spot/playlists?offset=0|server|port]"));
        assert!(!out2.contains("Proxima pagina"));
    }

    #[test]
    fn playlist_tracks_link_to_track_detail() {
        let f = fake();
        let out = r("", "/spot/playlists/pl0", Some(&f));
        assert!(out.contains("[1|Faixa X - Chico|/spot/track/tx|server|port]"));
    }

    #[test]
    fn query_decode_handles_percent_and_plus() {
        let a = DcgiArgs::from_argv(&argv("", "/spot/play?uri=spotify%3Atrack%3Aabc"));
        assert_eq!(a.query("uri"), Some("spotify:track:abc".into()));
    }
}
