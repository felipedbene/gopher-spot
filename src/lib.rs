//! `gopher-spot` — a Spotify control surface served over Gopher.
//!
//! Two shapes share one binary (see `main.rs`):
//!   gopher-spot root
//!       Print the static root menu as a geomyidae `.gph`. Baked to
//!       `/srv/index.gph` at image build time.
//!   gopher-spot dcgi $search $arguments $host $port $traversal $selector
//!       The dynamic entry geomyidae calls for any `/spot/*` selector (via the
//!       `index.dcgi` fallback). Routes on the selector and prints a gophermap.
//!   gopher-spot oauth-init                       (net feature)
//!       One-shot Spotify Authorization Code flow that prints a refresh token.
//!
//! The dcgi drives the Spotify Web API (blocking `ureq`) against the
//! `gopher-spot` Connect device. Output is transcoded at the IO edge (`main.rs`)
//! so the OS 9 gopher client renders accented names cleanly. The active client is
//! Netscape Communicator, which decodes charset-less Gopher as **Latin-1**
//! (`latin1`), so that's the default; `macroman` is kept for TurboGopher. The
//! encoder is chosen at runtime by `GOPHER_ENCODING` (see `main.rs`).

pub mod cache;
pub mod dcgi;
pub mod latin1;
pub mod macroman;
pub mod menu;
pub mod spotify;
