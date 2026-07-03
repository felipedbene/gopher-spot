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
    id_from_uri, AlbumDetail, AlbumsPage, Artist, Control, Playing, PlaylistsPage, SearchResults,
    SpotifyApi, Track, TracksPage, PAGE_SIZE,
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

    /// The raw argument after `?` (geomyidae's argv[2]), for the API endpoints
    /// that take a bare value rather than `key=value` (`/spot/api/1/volume?70`).
    /// Falls back to any `?...` on the selector, trimmed of a `\t`search suffix.
    pub fn raw_arg(&self) -> String {
        if !self.arguments.is_empty() {
            return self.arguments.clone();
        }
        self.selector
            .split_once('?')
            .map(|(_, q)| q.split('\t').next().unwrap_or(q).to_string())
            .unwrap_or_default()
    }
}

/// Route a request to its response. Human `/spot/*` selectors render gophermaps;
/// `/spot/api/*` renders the machine API's tab-delimited type-0 text (raw UTF-8,
/// emitted without the Latin-1 transcode — see `main.rs`). `api` is `Some` on the
/// live path, `None` for the offline mock (no OAuth Secret). `now_ms` is the
/// request wall-clock (unix ms), used as the API snapshot `ts`.
pub fn route(args: &DcgiArgs, api: Option<&dyn SpotifyApi>, now_ms: i64) -> String {
    let path = args.path();
    // The machine API is a separate, frozen contract (fio S1/S2) served RAW (it
    // can return binary cover bytes), so `main.rs` dispatches `/spot/api/*`
    // straight to `api::route` (-> Vec<u8>) and never reaches this gophermap
    // router. `now_ms` is still threaded through for the human menus below.
    let _ = now_ms;
    match path.as_str() {
        // The section root serves the same menu as the baked /srv/index.gph.
        "" | "/" | "/spot" => menu::root_gph(),
        "/spot/now" => match api {
            Some(a) => match a.now_playing() {
                // Queue is best-effort context, never block Now Playing on it.
                Ok(p) => {
                    let next = a.queue().ok().and_then(|q| q.into_iter().next());
                    let mut out = page(now_entries(&p, next.as_ref()));
                    // [SND]: a type-s (sound) item pointing at the audio stream .pls,
                    // so the client (MacAST) can (re)open audio straight from Now
                    // Playing. gopher-core's Entry has no Sound kind, so append the
                    // type-s line to the rendered gophermap exactly like root_gph.
                    out.push_str(&menu::sound_line(
                        "[SND] Ouvir agora (MacAST)",
                        "/spot/stream.pls",
                    ));
                    out
                }
                Err(e) => page(error_entries(&e)),
            },
            None => mock(
                "Now Playing",
                "(mock) nada tocando -- configure o Secret OAuth",
            ),
        },
        "/spot/search" => search(args, api),
        "/spot/control" => page(control_menu()),
        p if p.starts_with("/spot/control/") => control(&p["/spot/control/".len()..], api),
        p if p.starts_with("/spot/track/") => track(&p["/spot/track/".len()..], api),
        p if p.starts_with("/spot/album/") => album(&p["/spot/album/".len()..], args, api),
        p if p.starts_with("/spot/artist/") => {
            let rest = &p["/spot/artist/".len()..];
            match rest.strip_suffix("/albums") {
                Some(id) => artist_albums(id, args, api),
                None => artist(rest, args, api),
            }
        }
        "/spot/play" => play(args, api),
        "/spot/playlists" => playlists(args, api),
        p if p.starts_with("/spot/playlists/") => {
            playlist(&p["/spot/playlists/".len()..], args, api)
        }
        p if p.starts_with("/spot/") => {
            mock("Em construcao", &format!("rota {p} ainda nao implementada"))
        }
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

fn album(id: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match api {
        // Header + track page: two cached GETs (header is near-free after warm-up),
        // which also paginates albums with >50 tracks correctly.
        Some(a) => match (a.album(id), a.album_tracks(id, offset)) {
            (Ok(al), Ok(t)) => page(album_entries(id, &al, &t)),
            (Err(e), _) | (_, Err(e)) => page(error_entries(&e)),
        },
        None => mock("Album", &format!("(mock) album {id} -- Fio C")),
    }
}

fn artist(id: &str, _args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    match api {
        // Top tracks is BEST-EFFORT: Spotify 403s /v1/artists/{id}/top-tracks for
        // apps without extended quota (the Nov-2024 Web API restriction — the same
        // one that blocks editorial playlists, handled in `playlist`). The artist
        // header and full discography (/v1/artists/{id} and .../albums) still 200,
        // so we must not let a top-tracks 403 sink the whole page. Fetch it, but on
        // any error just render an empty "Populares" section.
        Some(a) => match a.artist(id) {
            Ok(ar) => {
                let top = a.artist_top_tracks(id).unwrap_or_default();
                page(artist_entries(id, &ar, &top))
            }
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Artista", &format!("(mock) artista {id} -- Fio C")),
    }
}

fn artist_albums(id: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match api {
        Some(a) => match a.artist_albums(id, offset) {
            Ok(p) => page(artist_albums_entries(id, &p)),
            Err(e) => page(error_entries(&e)),
        },
        None => mock("Discografia", &format!("(mock) albuns do artista {id}")),
    }
}

fn play(args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    // Context play (fio S3/5): ?context_uri=<album|playlist|artist uri>&offset=<i>
    // starts track i WITHIN that context, so next/prev follow the album/playlist
    // order instead of the autoplay radio. Additive — the ?uri= single-track path
    // below (what fio 9 already uses) is untouched.
    if let Some(ctx) = args.query("context_uri").filter(|u| !u.is_empty()) {
        let offset = args
            .query("offset")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        return match api {
            Some(a) => match a.play_context(&ctx, offset) {
                Ok(()) => page(vec![
                    info("Tocando contexto no gopher-spot..."),
                    info(clip(&format!("{ctx} (faixa {offset})"))),
                    info(""),
                    info("(next/prev seguem a ordem do album/playlist)"),
                    info(""),
                    link(ItemKind::Menu, "Now Playing", "/spot/now"),
                    link(ItemKind::Menu, "Voltar ao menu", "/"),
                ]),
                Err(e) => page(error_entries(&e)),
            },
            None => mock(
                "Tocar",
                &format!("(mock) tocaria contexto {ctx} @ {offset}"),
            ),
        };
    }
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
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match api {
        Some(a) => match a.playlists(offset) {
            Ok(p) => page(playlists_entries(&p)),
            Err(e) => page(error_entries(&e)),
        },
        None => mock(
            "Minhas playlists",
            "(mock) sem playlists -- configure o Secret OAuth",
        ),
    }
}

fn playlist(id: &str, args: &DcgiArgs, api: Option<&dyn SpotifyApi>) -> String {
    let id = id.trim_end_matches('/');
    let offset = args
        .query("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
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
        None => mock(
            "Playlist",
            &format!("(mock) faixas da playlist {id} -- Fio D"),
        ),
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

fn now_entries(p: &Playing, next: Option<&Track>) -> Vec<Entry> {
    let mut e = vec![info("Now Playing"), info("")];
    match &p.item {
        Some(t) => {
            e.push(info(clip(&t.name)));
            // Artist(s) and album are now clickable — jump straight to their pages
            // instead of bouncing back to search.
            push_artist_links(&mut e, &t.artists, &t.artist_line());
            push_album_link(&mut e, t.album.as_ref());
            e.push(info(if p.is_playing {
                "[tocando]"
            } else {
                "[pausado]"
            }));
            // Surface the queue so "no more tracks left in queue" isn't invisible.
            // With autoplay on, an empty queue means the radio will pick the next
            // song; say so instead of leaving the user guessing. Link the next
            // track to its detail page when we know its id.
            match next {
                Some(n) => match &n.id {
                    Some(nid) => e.push(link(
                        ItemKind::Menu,
                        clip(&format!("proxima: {}", n.name)),
                        format!("/spot/track/{nid}"),
                    )),
                    None => e.push(info(clip(&format!("proxima: {}", n.name)))),
                },
                None => e.push(info("fila vazia (radio automatico)")),
            }
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
    hub_footer(&mut e);
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
    // Artists / albums open their DETAIL page (which leads with a "Tocar tudo"),
    // so search is a real entry point into the browsable graph rather than a
    // play-and-done. Fall back to a plain line if the API omitted the uri.
    if let Some(artists) = r.artists.as_ref().filter(|p| !p.items.is_empty()) {
        e.push(info(""));
        e.push(info("Artistas:"));
        for a in artists.items.iter().take(5) {
            match id_from_uri(&a.uri) {
                Some(aid) => e.push(link(
                    ItemKind::Menu,
                    clip(&a.name),
                    format!("/spot/artist/{aid}"),
                )),
                None => e.push(info(clip(&format!("  {}", a.name)))),
            }
        }
    }
    if let Some(albums) = r.albums.as_ref().filter(|p| !p.items.is_empty()) {
        e.push(info(""));
        e.push(info("Albuns:"));
        for a in albums.items.iter().take(5) {
            match id_from_uri(&a.uri) {
                Some(aid) => e.push(link(
                    ItemKind::Menu,
                    clip(&a.name),
                    format!("/spot/album/{aid}"),
                )),
                None => e.push(info(clip(&format!("  {}", a.name)))),
            }
        }
    }
    e.push(info(""));
    e.push(link(ItemKind::Search, "Buscar de novo", "/spot/search"));
    hub_footer(&mut e);
    e
}

fn track_entries(t: &Track) -> Vec<Entry> {
    let mut e = vec![info(clip(&t.name))];
    push_artist_links(&mut e, &t.artists, &t.artist_line());
    push_album_link(&mut e, t.album.as_ref());
    if t.duration_ms > 0 {
        let secs = t.duration_ms / 1000;
        e.push(info(format!("duracao: {}:{:02}", secs / 60, secs % 60)));
    }
    e.push(info(""));
    // ASCII ">>" rather than the U+25B6 triangle (not in MacRoman).
    if !t.uri.is_empty() {
        e.push(link(
            ItemKind::Menu,
            ">> Tocar agora",
            format!("/spot/play?uri={}", t.uri),
        ));
    }
    e.push(link(ItemKind::Menu, "Controles", "/spot/control"));
    hub_footer(&mut e);
    e
}

/// Floodgap-style navigation hub appended to every page so the user is never a
/// dead end: Now Playing / Buscar / Menu, led by a blank spacer.
fn hub_footer(e: &mut Vec<Entry>) {
    e.push(info(""));
    e.push(link(ItemKind::Menu, "Now Playing", "/spot/now"));
    e.push(link(ItemKind::Search, "Buscar", "/spot/search"));
    e.push(link(ItemKind::Menu, "Voltar ao menu", "/"));
}

/// One clickable "por <artist>" line per artist that carries a uri; if NONE do,
/// fall back to the joined plain line so we never regress to a broken link.
fn push_artist_links(e: &mut Vec<Entry>, artists: &[Artist], joined: &str) {
    let any = artists.iter().any(|a| id_from_uri(&a.uri).is_some());
    if !any {
        e.push(info(clip(&format!("por {joined}"))));
        return;
    }
    for ar in artists {
        match id_from_uri(&ar.uri) {
            Some(aid) => e.push(link(
                ItemKind::Menu,
                clip(&format!("por {}", ar.name)),
                format!("/spot/artist/{aid}"),
            )),
            None => e.push(info(clip(&format!("por {}", ar.name)))),
        }
    }
}

/// A clickable "album: <name>" line, or plain info if the album has no uri.
fn push_album_link(e: &mut Vec<Entry>, album: Option<&crate::spotify::Album>) {
    if let Some(al) = album {
        match id_from_uri(&al.uri) {
            Some(aid) => e.push(link(
                ItemKind::Menu,
                clip(&format!("album: {}", al.name)),
                format!("/spot/album/{aid}"),
            )),
            None => e.push(info(clip(&format!("album: {}", al.name)))),
        }
    }
}

fn album_entries(id: &str, a: &AlbumDetail, t: &TracksPage) -> Vec<Entry> {
    let mut e = vec![info(clip(&a.name)), info("")];
    push_artist_links(&mut e, &a.artists, &artists_joined(&a.artists));
    if !a.uri.is_empty() {
        e.push(link(
            ItemKind::Menu,
            ">> Tocar album",
            format!("/spot/play?uri={}", a.uri),
        ));
    }
    e.push(info(""));
    e.push(info("Faixas:"));
    for tr in &t.items {
        if let Some(tid) = &tr.id {
            e.push(link(
                ItemKind::Menu,
                clip(&tr.name),
                format!("/spot/track/{tid}"),
            ));
        }
    }
    append_pager(
        &mut e,
        &format!("/spot/album/{id}"),
        t.offset,
        t.items.len(),
        t.total,
    );
    hub_footer(&mut e);
    e
}

fn artist_entries(id: &str, a: &Artist, top: &[Track]) -> Vec<Entry> {
    let mut e = vec![info(clip(&a.name)), info("")];
    if !a.uri.is_empty() {
        e.push(link(
            ItemKind::Menu,
            ">> Tocar artista",
            format!("/spot/play?uri={}", a.uri),
        ));
    }
    e.push(link(
        ItemKind::Menu,
        "Ver discografia",
        format!("/spot/artist/{id}/albums"),
    ));
    e.push(info(""));
    // top-tracks is best-effort (Spotify 403s it for non-extended-quota apps); when
    // it's empty, say so instead of a bare "Populares:" with nothing under it.
    if top.is_empty() {
        e.push(info("(faixas populares indisponiveis)"));
    } else {
        e.push(info("Populares:"));
        for tr in top.iter().take(10) {
            if let Some(tid) = &tr.id {
                e.push(link(
                    ItemKind::Menu,
                    clip(&tr.name),
                    format!("/spot/track/{tid}"),
                ));
            }
        }
    }
    hub_footer(&mut e);
    e
}

fn artist_albums_entries(id: &str, p: &AlbumsPage) -> Vec<Entry> {
    let mut e = vec![info("Discografia"), info("")];
    if p.items.is_empty() {
        e.push(info("Nenhum album."));
    }
    for al in &p.items {
        match id_from_uri(&al.uri) {
            Some(aid) => e.push(link(
                ItemKind::Menu,
                clip(&al.name),
                format!("/spot/album/{aid}"),
            )),
            None => e.push(info(clip(&format!("  {}", al.name)))),
        }
    }
    append_pager(
        &mut e,
        &format!("/spot/artist/{id}/albums"),
        p.offset,
        p.items.len(),
        p.total,
    );
    hub_footer(&mut e);
    e
}

/// Join artist names with ", " (Album/Artist have no `artist_line`).
fn artists_joined(artists: &[Artist]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn playlists_entries(p: &PlaylistsPage) -> Vec<Entry> {
    let mut e = vec![info("Minhas playlists"), info("")];
    if p.items.is_empty() {
        e.push(info("Nenhuma playlist."));
    } else {
        for pl in &p.items {
            if let Some(id) = &pl.id {
                e.push(link(
                    ItemKind::Menu,
                    clip(&pl.name),
                    format!("/spot/playlists/{id}"),
                ));
            }
        }
    }
    append_pager(&mut e, "/spot/playlists", p.offset, p.items.len(), p.total);
    hub_footer(&mut e);
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
    append_pager(
        &mut e,
        &format!("/spot/playlists/{id}"),
        t.offset,
        t.items.len(),
        t.total,
    );
    hub_footer(&mut e);
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
        e.push(link(
            ItemKind::Menu,
            "<< Pagina anterior",
            format!("{base}?offset={prev}"),
        ));
    }
    if has_next {
        let next = offset + PAGE_SIZE;
        e.push(link(
            ItemKind::Menu,
            ">> Proxima pagina",
            format!("{base}?offset={next}"),
        ));
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
        info(clip(&format!(
            "(mock) buscaria por: {query} -- configure o Secret OAuth"
        ))),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

/// Minimal percent-decode for query values (`%XX` and `+` -> space). Decodes into
/// BYTES and reads them back as UTF-8: a `%C3%A7` pair is one `ç`, not two Latin-1
/// chars. (Decoding each `%XX` straight into a `char` — as this once did — mangles
/// any multi-byte UTF-8, e.g. the API `search?q=construção`.) Values are ASCII in
/// most callers (`play?uri=spotify:track:…`), where byte- and char-decode agree.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(h);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::{Album, ApiError, Artist, Page, Playlist, PlaylistTracksRef};

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
        // Simulate Spotify 403ing /v1/artists/{id}/top-tracks (extended-quota
        // restriction) while the header + discography still succeed.
        artist_top_fails: bool,
    }
    impl SpotifyApi for Fake {
        fn now_playing(&self) -> Result<Playing, ApiError> {
            Ok(self.playing.clone())
        }
        fn queue(&self) -> Result<Vec<Track>, ApiError> {
            Ok(Vec::new())
        }
        fn queue_add(&self, _uri: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn album_cover(&self, _album_id: &str, _want_px: u32) -> Result<Vec<u8>, ApiError> {
            Ok(Vec::new())
        }
        fn search(&self, q: &str) -> Result<SearchResults, ApiError> {
            Ok(SearchResults {
                tracks: Some(Page {
                    items: vec![Track {
                        name: format!("Faixa {q}"),
                        artists: vec![Artist {
                            name: "Chico".into(),
                            uri: String::new(),
                        }],
                        album: Some(Album {
                            name: "Al".into(),
                            uri: String::new(),
                        }),
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
                artists: vec![Artist {
                    name: "Chico".into(),
                    uri: "spotify:artist:ar1".into(),
                }],
                album: Some(Album {
                    name: "Al".into(),
                    uri: "spotify:album:al1".into(),
                }),
                id: Some(id.into()),
                uri: format!("spotify:track:{id}"),
                duration_ms: 380000,
            })
        }
        fn play(&self, _uri: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn play_context(&self, _c: &str, _o: u32) -> Result<(), ApiError> {
            Ok(())
        }
        fn playlist_name(&self, id: &str) -> Result<String, ApiError> {
            Ok(format!("PL {id}"))
        }
        fn wake(&self, _play: bool) -> Result<(), ApiError> {
            Ok(())
        }
        fn control(&self, _cmd: Control) -> Result<(), ApiError> {
            Ok(())
        }
        fn seek(&self, _position_ms: u64) -> Result<(), ApiError> {
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
                    tracks: PlaylistTracksRef::default(),
                })
                .collect();
            Ok(PlaylistsPage {
                items,
                total,
                offset,
            })
        }
        fn playlist_tracks(&self, _id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            let items = vec![Track {
                name: "Faixa X".into(),
                artists: vec![Artist {
                    name: "Chico".into(),
                    uri: String::new(),
                }],
                album: Some(Album {
                    name: "Al".into(),
                    uri: String::new(),
                }),
                id: Some("tx".into()),
                uri: "spotify:track:tx".into(),
                duration_ms: 0,
            }];
            Ok(TracksPage {
                items,
                total: 1,
                offset,
            })
        }
        fn album(&self, id: &str) -> Result<AlbumDetail, ApiError> {
            Ok(AlbumDetail {
                name: format!("Album {id}"),
                uri: format!("spotify:album:{id}"),
                artists: vec![Artist {
                    name: "Chico".into(),
                    uri: "spotify:artist:ar1".into(),
                }],
                total: 1,
            })
        }
        fn album_tracks(&self, _id: &str, offset: u32) -> Result<TracksPage, ApiError> {
            let items = vec![Track {
                name: "Faixa A".into(),
                artists: vec![],
                album: None,
                id: Some("ta".into()),
                uri: "spotify:track:ta".into(),
                duration_ms: 0,
            }];
            Ok(TracksPage {
                items,
                total: 1,
                offset,
            })
        }
        fn artist(&self, id: &str) -> Result<Artist, ApiError> {
            Ok(Artist {
                name: format!("Artist {id}"),
                uri: format!("spotify:artist:{id}"),
            })
        }
        fn artist_albums(&self, _id: &str, offset: u32) -> Result<AlbumsPage, ApiError> {
            let items = vec![Album {
                name: "Disco".into(),
                uri: "spotify:album:d1".into(),
            }];
            Ok(AlbumsPage {
                items,
                total: 1,
                offset,
            })
        }
        fn artist_top_tracks(&self, _id: &str) -> Result<Vec<Track>, ApiError> {
            if self.artist_top_fails {
                return Err("spotify HTTP 403: Forbidden".into());
            }
            Ok(vec![Track {
                name: "Top1".into(),
                artists: vec![],
                album: None,
                id: Some("tt".into()),
                uri: "spotify:track:tt".into(),
                duration_ms: 0,
            }])
        }
    }

    fn fake() -> Fake {
        Fake {
            artist_top_fails: false,
            playing: Playing {
                is_playing: true,
                progress_ms: 1000,
                item: Some(Track {
                    name: "Construção".into(),
                    artists: vec![Artist {
                        name: "Chico Buarque".into(),
                        uri: "spotify:artist:ar1".into(),
                    }],
                    album: Some(Album {
                        name: "Construção".into(),
                        uri: "spotify:album:al1".into(),
                    }),
                    id: Some("abc".into()),
                    uri: "spotify:track:abc".into(),
                    duration_ms: 380000,
                }),
                device: None,
            },
        }
    }

    fn r(search: &str, selector: &str, api: Option<&dyn SpotifyApi>) -> String {
        route(
            &DcgiArgs::from_argv(&argv(search, selector)),
            api,
            1_700_000_000_000,
        )
    }

    #[test]
    fn now_playing_renders_track() {
        let f = fake();
        let out = r("", "/spot/now", Some(&f));
        assert!(out.contains("Construção"));
        assert!(out.contains("Chico Buarque"));
        assert!(out.contains("[tocando]"));
        // Inline controls + queue state + a Buscar shortcut all live here now.
        assert!(out.contains("/spot/control/next"));
        assert!(out.contains("fila vazia")); // Fake queue is empty
        assert!(out.contains("[7|Buscar|/spot/search|server|port]"));
        // [SND]: the type-s stream item for MacAST lives on Now Playing now.
        assert!(out.contains("[s|[SND] Ouvir agora (MacAST)|/spot/stream.pls|server|port]"));
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
    fn search_albums_and_artists_link_to_detail() {
        let r = SearchResults {
            tracks: None,
            artists: Some(Page {
                items: vec![Artist {
                    name: "The Smiths".into(),
                    uri: "spotify:artist:sm".into(),
                }],
            }),
            albums: Some(Page {
                items: vec![Album {
                    name: "The Queen Is Dead".into(),
                    uri: "spotify:album:qd".into(),
                }],
            }),
        };
        let out = render_menu_index(&search_entries("smiths", &r));
        // Both open their detail page (browsable), not a direct play.
        assert!(out.contains("[1|The Smiths|/spot/artist/sm|server|port]"));
        assert!(out.contains("[1|The Queen Is Dead|/spot/album/qd|server|port]"));
    }

    #[test]
    fn now_playing_links_album_and_artist() {
        let f = fake();
        let out = r("", "/spot/now", Some(&f));
        assert!(out.contains("[1|por Chico Buarque|/spot/artist/ar1|server|port]"));
        assert!(out.contains("[1|album: Construção|/spot/album/al1|server|port]"));
    }

    #[test]
    fn track_detail_cross_links_album_and_artist() {
        let f = fake();
        let out = r("", "/spot/track/xyz", Some(&f));
        assert!(out.contains("[1|por Chico|/spot/artist/ar1|server|port]"));
        assert!(out.contains("[1|album: Al|/spot/album/al1|server|port]"));
    }

    #[test]
    fn album_page_links_tracks_artist_and_play() {
        let f = fake();
        let out = r("", "/spot/album/qd", Some(&f));
        assert!(out.contains("[1|por Chico|/spot/artist/ar1|server|port]"));
        assert!(out.contains("[1|>> Tocar album|/spot/play?uri=spotify:album:qd|server|port]"));
        assert!(out.contains("[1|Faixa A|/spot/track/ta|server|port]"));
    }

    #[test]
    fn artist_page_lists_top_tracks_and_discography() {
        let f = fake();
        let out = r("", "/spot/artist/sm", Some(&f));
        assert!(out.contains("[1|>> Tocar artista|/spot/play?uri=spotify:artist:sm|server|port]"));
        assert!(out.contains("[1|Ver discografia|/spot/artist/sm/albums|server|port]"));
        assert!(out.contains("[1|Top1|/spot/track/tt|server|port]"));
    }

    #[test]
    fn artist_page_survives_top_tracks_403() {
        // Regression (fio S1): a 403 on top-tracks must NOT sink the artist page;
        // the header + "Ver discografia" still render, with a note in place of the
        // populares list.
        let f = Fake {
            artist_top_fails: true,
            ..fake()
        };
        let out = r("", "/spot/artist/sm", Some(&f));
        assert!(
            !out.contains("Erro"),
            "top-tracks 403 leaked to the page: {out}"
        );
        assert!(out.contains("[1|Ver discografia|/spot/artist/sm/albums|server|port]"));
        assert!(out.contains("(faixas populares indisponiveis)"));
    }

    #[test]
    fn artist_albums_page_links_albums() {
        let f = fake();
        let out = r("", "/spot/artist/sm/albums", Some(&f));
        assert!(out.contains("[1|Disco|/spot/album/d1|server|port]"));
    }

    #[test]
    fn play_parses_uri_from_query() {
        let f = fake();
        let out = r("", "/spot/play?uri=spotify:track:abc", Some(&f));
        assert!(out.contains("Mandando tocar"));
        assert!(out.contains("spotify:track:abc"));
    }

    #[test]
    fn play_context_uri_starts_context_at_offset() {
        // fio S3/5: ?context_uri=&offset= plays a context (album/playlist) so
        // next/prev follow its order. The ?uri= single-track path is untouched.
        let f = fake();
        let out = r(
            "",
            "/spot/play?context_uri=spotify:playlist:pl1&offset=3",
            Some(&f),
        );
        assert!(out.contains("Tocando contexto"));
        assert!(out.contains("spotify:playlist:pl1 (faixa 3)"));
        // A bare ?uri= still goes the single-track route.
        let out2 = r("", "/spot/play?uri=spotify:track:abc", Some(&f));
        assert!(out2.contains("Mandando tocar"));
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
            "/spot",
            "/spot/now",
            "/spot/search",
            "/spot/control",
            "/spot/control/next",
            "/spot/track/abc",
            "/spot/play?uri=spotify:track:abc",
            "/spot/playlists",
            "/spot/playlists?offset=20",
            "/spot/playlists/pl0",
            "/spot/album/qd",
            "/spot/artist/sm",
            "/spot/artist/sm/albums",
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

    #[test]
    fn query_decode_is_utf8_byte_correct() {
        // %C3%A7%C3%A3o must round-trip to "ção" (one char per multi-byte seq),
        // not four Latin-1 chars — the fio S3/4 API search?q= relies on this.
        let a = DcgiArgs::from_argv(&argv("", "/spot/api/1/search?q=constru%C3%A7%C3%A3o"));
        assert_eq!(a.query("q"), Some("construção".into()));
    }
}
