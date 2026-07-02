//! gopher-spot CLI. Shapes sharing one binary:
//!   gopher-spot root
//!       Print the static root menu (.gph). Baked to /srv/index.gph at build.
//!   gopher-spot dcgi $search $arguments $host $port $traversal $selector
//!       The dcgi entry geomyidae calls for /spot/* selectors; prints a gophermap.
//!   gopher-spot oauth-init                        (net feature only)
//!       One-shot Spotify Authorization Code flow; prints REFRESH_TOKEN=... .
//!
//! All gophermap output is transcoded on the way to stdout so the OS 9 gopher
//! client renders accented track names cleanly (ASCII is always identity). The
//! active client is Netscape Communicator, which reads charset-less Gopher as
//! Latin-1 — that's the default. Override with `GOPHER_ENCODING=macroman|utf8`
//! (macroman for TurboGopher; utf8 for a raw passthrough).

use std::io::Write;
use std::process::ExitCode;

use gopher_spot::{dcgi, latin1, macroman, menu};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("root") => {
            emit(&menu::root_gph());
            ExitCode::SUCCESS
        }
        Some("dcgi") => {
            emit(&run_dcgi(&args[2..]));
            ExitCode::SUCCESS
        }
        #[cfg(feature = "net")]
        Some("oauth-init") => oauth::run(),
        _ => {
            eprintln!("usage: gopher-spot <root|dcgi|oauth-init> [args...]");
            ExitCode::from(2)
        }
    }
}

/// Write a rendered gophermap to stdout, transcoded to the client's charset.
/// Default Latin-1 (Netscape Communicator); `GOPHER_ENCODING` overrides. ASCII —
/// including every byte geomyidae parses — is identity under all three.
fn emit(gph: &str) {
    let bytes = match std::env::var("GOPHER_ENCODING").as_deref() {
        Ok("macroman") => macroman::encode(gph),
        Ok("utf8") => gph.as_bytes().to_vec(),
        _ => latin1::encode(gph),
    };
    let _ = std::io::stdout().write_all(&bytes);
}

/// Route a dcgi request. On the `net` build we try to construct a live Spotify
/// client from the OAuth env (the Secret); if it's absent, `route` gets `None`
/// and serves the offline mock menus.
fn run_dcgi(rest: &[String]) -> String {
    let a = dcgi::DcgiArgs::from_argv(rest);

    #[cfg(feature = "net")]
    {
        use gopher_spot::spotify::{Client, SpotifyApi};
        let state = std::env::var("SPOT_STATE_DIR").unwrap_or_else(|_| "/var/cache/spot".to_string());
        let client = Client::from_env(now_unix(), std::path::PathBuf::from(state));
        dcgi::route(&a, client.as_ref().map(|c| c as &dyn SpotifyApi))
    }

    #[cfg(not(feature = "net"))]
    {
        dcgi::route(&a, None)
    }
}

#[cfg(feature = "net")]
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The one-shot OAuth Authorization Code flow. Run locally once to mint a refresh
/// token for the Secret; see scripts/spotify-oauth-init.sh.
#[cfg(feature = "net")]
mod oauth {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::ExitCode;

    // Spotify deprecated `localhost` in redirect URIs — the loopback must be the
    // explicit IP. Register exactly this in the app's Redirect URIs.
    const REDIRECT: &str = "http://127.0.0.1:8888/callback";
    const SCOPES: &str = "user-read-private user-read-playback-state \
        user-modify-playback-state user-read-currently-playing \
        playlist-read-private playlist-read-collaborative user-library-read";

    pub fn run() -> ExitCode {
        let (id, secret) = match (env("SPOTIFY_CLIENT_ID"), env("SPOTIFY_CLIENT_SECRET")) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                eprintln!("set SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET first");
                return ExitCode::from(2);
            }
        };
        let auth_url = format!(
            "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}",
            id,
            urlencode(REDIRECT),
            urlencode(SCOPES),
        );
        eprintln!(
            "\n1) Abra no browser (mesma maquina):\n\n{auth_url}\n\n\
             2) Autorize. O callback volta pra {REDIRECT} e este processo pega o code.\n"
        );
        let code = match wait_for_code() {
            Some(c) => c,
            None => {
                eprintln!("no authorization code received");
                return ExitCode::FAILURE;
            }
        };
        match exchange(&id, &secret, &code) {
            Ok(refresh) => {
                eprintln!("\nOK. refresh token (stdout):");
                println!("REFRESH_TOKEN={refresh}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("token exchange failed: {e}");
                ExitCode::FAILURE
            }
        }
    }

    /// Listen on :8888 for the redirect and pull `code` from the request line.
    /// Non-callback hits (e.g. favicon) get a 200 and the loop keeps waiting.
    fn wait_for_code() -> Option<String> {
        let listener = match TcpListener::bind("127.0.0.1:8888") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("cannot bind 127.0.0.1:8888: {e}");
                return None;
            }
        };
        for stream in listener.incoming() {
            let mut stream = stream.ok()?;
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).ok()?;
            let code = line
                .split_whitespace()
                .nth(1)
                .and_then(|p| p.split_once('?').map(|(_, q)| q.to_string()))
                .and_then(|q| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix("code=").map(String::from))
                });
            let body = "gopher-spot: code recebido. Pode fechar esta aba e voltar ao terminal.";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            if let Some(c) = code {
                return Some(c);
            }
        }
        None
    }

    fn exchange(id: &str, secret: &str, code: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Tok {
            refresh_token: String,
        }
        let resp = ureq::post("https://accounts.spotify.com/api/token")
            .send_form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT),
                ("client_id", id),
                ("client_secret", secret),
            ])
            .map_err(|e| e.to_string())?;
        let tok: Tok = resp.into_json().map_err(|e| e.to_string())?;
        Ok(tok.refresh_token)
    }

    fn env(k: &str) -> Option<String> {
        std::env::var(k).ok().filter(|v| !v.is_empty())
    }

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
}
