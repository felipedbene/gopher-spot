//! The dynamic `/spot/*` entry point. geomyidae runs `spot/index.dcgi` for any
//! non-existent `/spot/...` selector, calling it
//!   index.dcgi $search $arguments $host $port $traversal $selector
//! and interpreting stdout as a gophermap (`.gph`). We route on the selector and
//! emit menus via `gopher-core`.
//!
//! Fio B: the endpoints render mock menus (no network). Fio C wires the Spotify
//! Web API behind the `net` feature.

use gopher_core::{info, link, render_menu_index, ItemKind};

use crate::menu;

/// The six arguments geomyidae hands a dcgi, in its documented order
/// (`$search $arguments $host $port $traversal $selector`).
#[derive(Debug, Clone, Default)]
pub struct DcgiArgs {
    /// argv[1] — the type-7 search term (after a TAB). Empty for plain type-1
    /// links; carries the query for `/spot/search`.
    pub search: String,
    /// argv[2] — the query string after `?` in the selector.
    pub arguments: String,
    /// argv[3] / argv[4] — the SERVER's host/port (what geomyidae advertises).
    pub host: String,
    pub port: String,
    /// argv[5] — the unreachable path portion (`/now` for `/spot/now` when
    /// `/spot` exists). Kept for completeness; we route on `selector`.
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
    /// and a trailing slash normalized off (except the bare root). Falls back to
    /// the traversal if the selector is somehow empty.
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
}

/// Route a request to its gophermap. Fio B endpoints are mock.
pub fn route(args: &DcgiArgs) -> String {
    match args.path().as_str() {
        // The dcgi is also reachable at the section root (/spot, /spot/) — serve
        // the same menu as the baked /srv/index.gph so both entry points agree.
        "" | "/" | "/spot" => menu::root_gph(),
        "/spot/now" => mock("Now Playing", "(mock) nada tocando ainda -- Fio C"),
        "/spot/search" => search_mock(&args.search),
        "/spot/playlists" => mock("Minhas playlists", "(mock) sem playlists ainda -- Fio D"),
        "/spot/control" => control_menu(),
        p if p.starts_with("/spot/") => {
            mock("Em construcao", &format!("rota {p} ainda nao implementada"))
        }
        p => not_found(p),
    }
}

/// A simple one-body mock menu with a back link. Fio B placeholder.
fn mock(title: &str, body: &str) -> String {
    render_menu_index(&[
        info(title),
        info(""),
        info(body),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

/// The type-7 search mock: echoes the query (or prompts) and links back.
fn search_mock(query: &str) -> String {
    let body = if query.trim().is_empty() {
        "(mock) digite um termo de busca -- Fio C liga a Web API".to_string()
    } else {
        format!("(mock) buscando por: {} -- Fio C", query.trim())
    };
    render_menu_index(&[
        info("Buscar"),
        info(""),
        info(&body),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

/// The controls menu (mock links; Fio C wires them to the Web API).
fn control_menu() -> String {
    render_menu_index(&[
        info("Controles"),
        info(""),
        link(ItemKind::Menu, "Pause", "/spot/control/pause"),
        link(ItemKind::Menu, "Proxima", "/spot/control/next"),
        link(ItemKind::Menu, "Anterior", "/spot/control/prev"),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

/// Unknown selector -> a small 404 menu (still a valid gophermap).
fn not_found(path: &str) -> String {
    render_menu_index(&[
        info("404 -- selector desconhecido"),
        info(&format!("  {path}")),
        info(""),
        link(ItemKind::Menu, "Voltar ao menu", "/"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(search: &str, selector: &str) -> Vec<String> {
        // $search $arguments $host $port $traversal $selector
        vec![
            search.into(),
            "".into(),
            "10.0.10.9".into(),
            "70".into(),
            "".into(),
            selector.into(),
        ]
    }

    #[test]
    fn argv_maps_in_geomyidae_order() {
        let a = DcgiArgs::from_argv(&argv("chico", "/spot/search"));
        assert_eq!(a.search, "chico");
        assert_eq!(a.host, "10.0.10.9");
        assert_eq!(a.port, "70");
        assert_eq!(a.selector, "/spot/search");
    }

    #[test]
    fn path_strips_query_search_and_trailing_slash() {
        let a = DcgiArgs {
            selector: "/spot/control/".into(),
            ..Default::default()
        };
        assert_eq!(a.path(), "/spot/control");
        let b = DcgiArgs {
            selector: "/spot/search?q\tchico".into(),
            ..Default::default()
        };
        assert_eq!(b.path(), "/spot/search");
    }

    #[test]
    fn routes_known_selectors() {
        assert!(route(&DcgiArgs::from_argv(&argv("", "/spot/now"))).contains("Now Playing"));
        assert!(route(&DcgiArgs::from_argv(&argv("", "/spot/control"))).contains("Pause"));
        assert!(route(&DcgiArgs::from_argv(&argv("", "/spot")))
            .contains("[7|Buscar|/spot/search|server|port]"));
    }

    #[test]
    fn search_echoes_the_query() {
        let out = route(&DcgiArgs::from_argv(&argv("construcao", "/spot/search")));
        assert!(out.contains("construcao"));
    }

    #[test]
    fn unknown_spot_route_is_under_construction() {
        let out = route(&DcgiArgs::from_argv(&argv("", "/spot/bogus/x")));
        assert!(out.contains("Em construcao"));
        assert!(out.contains("Voltar ao menu"));
    }

    #[test]
    fn non_spot_selector_is_a_404_menu() {
        let out = route(&DcgiArgs::from_argv(&argv("", "/bogus")));
        assert!(out.contains("404"));
        assert!(out.contains("Voltar ao menu"));
    }

    #[test]
    fn every_route_is_a_tabless_gophermap() {
        for sel in ["/spot", "/spot/now", "/spot/search", "/spot/control", "/spot/x"] {
            let out = route(&DcgiArgs::from_argv(&argv("", sel)));
            assert!(!out.contains('\t'), "gophermap must not contain tabs: {sel}");
            for line in out.lines() {
                if line.starts_with('[') {
                    assert!(line.ends_with(']'), "malformed link line: {line}");
                }
            }
        }
    }
}
