//! `gopher-spot` — a Spotify control surface served over Gopher.
//!
//! Two shapes share one binary (see `main.rs`):
//!   gopher-spot root
//!       Print the static root menu as a geomyidae `.gph`. Baked to
//!       `/srv/index.gph` at image build time (the root is a fixed list of
//!       links; nothing dynamic lives there).
//!   gopher-spot dcgi $search $arguments $host $port $traversal $selector
//!       The dynamic entry geomyidae calls for any `/spot/*` selector (via the
//!       `index.dcgi` fallback). Routes on the selector and prints a gophermap.
//!
//! Fio B is the offline skeleton: the `/spot/*` endpoints render mock menus. The
//! Web API wiring (search/control/now-playing) arrives in Fio C.

pub mod dcgi;
pub mod menu;
