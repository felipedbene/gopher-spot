//! `/spot/api/1/stream` — the media plane (Icecast) reporting its own state.
//!
//! The bridge proxies three independent state owners: the Spotify Web API
//! (`/now`), the librespot→ffmpeg→Icecast audio chain (what the ear hears), and
//! Spotify's queue. This module makes the second one observable, replacing the
//! clients' "rx went dry" guessing (Casquinha b49's "waiting for Spotify"
//! heuristic becomes a server fact).
//!
//! Source of truth: Icecast's **public** `status-json.xsl` on the audio-stream
//! service — verified live (Icecast 2.4.4): it lists the mounts even with
//! `<public>0</public>`, so no admin auth is needed. Facts derived from it:
//!
//! - `live` — a source is feeding the `/spotify.mp3` mount, i.e. the stream
//!   carries real audio. When the live chain drops (idle/pause past
//!   source-timeout, or a crash) the mount disappears from the stats and
//!   Icecast fails listeners over to `/silence.mp3`.
//! - `listeners` — external listeners: Icecast's count minus the entrypoint's
//!   permanent internal drainer (fio S3/1), clamped at 0. Counted across BOTH
//!   mounts, because the failover moves everyone (drainer included) to the
//!   silence mount — only the total is stable across that transition.
//!
//! Kept out of `spotify.rs` on purpose: this is the OTHER state owner,
//! independent of the Web API. `/stream` must answer even when the OAuth Secret
//! is absent, and `/now` must never pay Icecast latency (the two endpoints
//! never call each other's upstream).

use serde::Deserialize;

/// The live mount clients (and the internal drainer) dial.
const LIVE_MOUNT: &str = "/spotify.mp3";

/// The entrypoint's permanent internal listener (the fio S3/1 drainer): always
/// connected — to /spotify.mp3, or failed over to /silence.mp3 when the live
/// source is down — so it inflates the listener total by exactly one. Without
/// this subtraction `/stream` would be born lying.
const DRAINER_LISTENERS: u64 = 1;

/// What `/stream` reports about the media plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFacts {
    /// A live source feeds /spotify.mp3 (real audio, not the silence fallback).
    pub live: bool,
    /// External listeners (internal drainer subtracted, clamped ≥ 0).
    pub listeners: u64,
}

// status-json.xsl quirk: `source` is an OBJECT when one mount is up, an ARRAY
// when several, and absent when none. The untagged enum absorbs all three.
#[derive(Deserialize)]
struct Root {
    icestats: IceStats,
}
#[derive(Deserialize)]
struct IceStats {
    #[serde(default)]
    source: Option<Sources>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Sources {
    Many(Vec<Source>),
    One(Box<Source>),
}
#[derive(Deserialize)]
struct Source {
    #[serde(default)]
    listenurl: String,
    #[serde(default)]
    listeners: u64,
}

/// Distill Icecast's `status-json.xsl` body into [`StreamFacts`]. Pure — the
/// fixture tests below run it against captured live bodies.
pub fn parse_status(json: &str) -> Result<StreamFacts, String> {
    let root: Root =
        serde_json::from_str(json).map_err(|e| format!("icecast status parse failed: {e}"))?;
    let sources: Vec<Source> = match root.icestats.source {
        None => Vec::new(),
        Some(Sources::One(s)) => vec![*s],
        Some(Sources::Many(v)) => v,
    };
    let live = sources.iter().any(|s| s.listenurl.ends_with(LIVE_MOUNT));
    let total: u64 = sources.iter().map(|s| s.listeners).sum();
    Ok(StreamFacts {
        live,
        listeners: total.saturating_sub(DRAINER_LISTENERS),
    })
}

/// The media-plane status source the API layer routes against — implemented by
/// the real [`IcecastStatus`] fetcher (net feature) and by test fakes. Mirrors
/// the `SpotifyApi` micro-cache hooks: the rendered document is cached ~2 s so
/// a poll burst never adds load to Icecast (and `/stream` stays independent of
/// `/now` — neither pays the other's upstream).
pub trait StreamSource {
    /// Fresh mount facts from Icecast. `Err` = unreachable / bad JSON.
    fn stream_facts(&self) -> Result<StreamFacts, String>;
    /// The cached rendered `/stream` document, if stored < ~2 s before `now_ms`.
    fn cached_stream(&self, _now_ms: i64) -> Option<String> {
        None
    }
    /// Store the just-rendered document, stamped at `now_ms`, for the TTL.
    fn store_stream(&self, _now_ms: i64, _doc: &str) {}
}

#[cfg(feature = "net")]
pub use net::IcecastStatus;

#[cfg(feature = "net")]
mod net {
    use super::*;
    use crate::cache;
    use std::path::PathBuf;

    // The audio-stream Service, as the gopher-server pod dials it. Full FQDN on
    // purpose: both Deployments run with ndots:1 (the debene.dev search-domain
    // trap), so a dotted name resolves absolute-first and never consults the
    // search path. AUDIO_STATUS_URL overrides (tests, local runs).
    const DEFAULT_STATUS_URL: &str =
        "http://audio-stream.gopher-spot.svc.cluster.local:8000/status-json.xsl";
    // A short, dedicated budget — never the Web API agent's 10 s: geomyidae
    // gives the whole request ~10 s, and /stream must stay a cheap side-fact.
    const STATUS_TIMEOUT_MS: u64 = 2_000;
    // Rendered-document micro-cache window. Clocked in MS under a fixed key,
    // exactly like `now_snapshot` (same atomic temp+rename writes underneath).
    const STREAM_CACHE_TTL_MS: i64 = 2_000;

    /// The real fetcher: one GET against Icecast's public status JSON with its
    /// own short-timeout agent, plus the on-disk document micro-cache.
    pub struct IcecastStatus {
        url: String,
        state_dir: PathBuf,
        agent: ureq::Agent,
    }

    impl IcecastStatus {
        pub fn new(url: String, state_dir: PathBuf, timeout_ms: u64) -> IcecastStatus {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_millis(timeout_ms))
                .build();
            IcecastStatus {
                url,
                state_dir,
                agent,
            }
        }

        /// Build from the env: `AUDIO_STATUS_URL` overrides the in-cluster
        /// default. Cheap per request, like `Client::from_env`.
        pub fn from_env(state_dir: PathBuf) -> IcecastStatus {
            let url = std::env::var("AUDIO_STATUS_URL")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_STATUS_URL.to_string());
            IcecastStatus::new(url, state_dir, STATUS_TIMEOUT_MS)
        }
    }

    impl StreamSource for IcecastStatus {
        fn stream_facts(&self) -> Result<StreamFacts, String> {
            let body = self
                .agent
                .get(&self.url)
                .call()
                .map_err(|e| format!("icecast status fetch failed: {e}"))?
                .into_string()
                .map_err(|e| format!("icecast status read failed: {e}"))?;
            parse_status(&body)
        }
        fn cached_stream(&self, now_ms: i64) -> Option<String> {
            cache::get(&self.state_dir, "stream_snapshot", now_ms)
        }
        fn store_stream(&self, now_ms: i64, doc: &str) {
            cache::put(
                &self.state_dir,
                "stream_snapshot",
                now_ms,
                STREAM_CACHE_TTL_MS,
                doc,
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tmp(name: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("gopher-spot-stream-{name}"));
            let _ = std::fs::remove_dir_all(&d);
            d
        }

        #[test]
        fn unreachable_icecast_is_an_error() {
            // Bind a port to learn a free one, then drop it: the connection is
            // refused, which must surface as Err (the API layer maps it to
            // `error upstream`) — never a panic or a hang.
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            drop(l);
            let s = IcecastStatus::new(
                format!("http://127.0.0.1:{port}/status-json.xsl"),
                tmp("refused"),
                500,
            );
            assert!(s.stream_facts().is_err());
        }

        #[test]
        fn silent_icecast_times_out_as_error() {
            // A listener that accepts but never answers: the dedicated short
            // timeout (not the Web API's 10 s) must turn it into Err.
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let s = IcecastStatus::new(
                format!("http://127.0.0.1:{port}/status-json.xsl"),
                tmp("timeout"),
                200,
            );
            let started = std::time::Instant::now();
            assert!(s.stream_facts().is_err());
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "the short per-source timeout must apply"
            );
            drop(l);
        }

        #[test]
        fn stream_cache_round_trip_in_ms_window() {
            let dir = tmp("cache");
            let s = IcecastStatus::new("http://unused.invalid/".into(), dir, 100);
            let t0: i64 = 1_700_000_000_000;
            assert!(s.cached_stream(t0).is_none());
            s.store_stream(t0, "api\t1\r\nlive\t1\r\n");
            assert_eq!(
                s.cached_stream(t0 + 1_999).as_deref(),
                Some("api\t1\r\nlive\t1\r\n")
            );
            assert!(s.cached_stream(t0 + 2_000).is_none(), "expired at the TTL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact body the live cluster served on 2026-07-07 (two mounts up,
    /// `source` as an ARRAY; /spotify.mp3 carried the drainer + one external).
    const LIVE_CAPTURE: &str = include_str!("../tests/fixtures/icecast-status-live.json");

    #[test]
    fn live_capture_parses_live_with_one_external_listener() {
        let f = parse_status(LIVE_CAPTURE).unwrap();
        assert!(f.live);
        // 2 on /spotify.mp3 + 0 on /silence.mp3, minus the drainer.
        assert_eq!(f.listeners, 1);
    }

    #[test]
    fn no_source_at_all_is_dead_air() {
        // Both encoders down (Icecast just booted): no `source` key.
        let f = parse_status(r#"{"icestats":{"host":"localhost"}}"#).unwrap();
        assert_eq!(
            f,
            StreamFacts {
                live: false,
                listeners: 0
            }
        );
    }

    #[test]
    fn single_source_object_form_is_tolerated() {
        // Icecast's quirk: ONE mount serializes `source` as an object, not a
        // one-element array. Silence-only = the live chain is down and the
        // drainer failed over to the fallback mount.
        let j = r#"{"icestats":{"source":
            {"listenurl":"http://localhost:8000/silence.mp3","listeners":1}}}"#;
        let f = parse_status(j).unwrap();
        assert!(!f.live, "silence fallback is not live audio");
        assert_eq!(f.listeners, 0, "the failed-over drainer is subtracted");
    }

    #[test]
    fn single_live_source_object_form() {
        let j = r#"{"icestats":{"source":
            {"listenurl":"http://localhost:8000/spotify.mp3","listeners":3}}}"#;
        let f = parse_status(j).unwrap();
        assert!(f.live);
        assert_eq!(f.listeners, 2);
    }

    #[test]
    fn drainer_only_listeners_report_zero_external() {
        // Steady idle state: both mounts up, only the drainer attached.
        let j = r#"{"icestats":{"source":[
            {"listenurl":"http://localhost:8000/silence.mp3","listeners":0},
            {"listenurl":"http://localhost:8000/spotify.mp3","listeners":1}]}}"#;
        let f = parse_status(j).unwrap();
        assert!(f.live);
        assert_eq!(f.listeners, 0);
    }

    #[test]
    fn listeners_clamp_never_goes_negative() {
        // A race where even the drainer is momentarily disconnected must clamp
        // to 0, not underflow.
        let j = r#"{"icestats":{"source":[
            {"listenurl":"http://localhost:8000/spotify.mp3","listeners":0}]}}"#;
        assert_eq!(parse_status(j).unwrap().listeners, 0);
    }

    #[test]
    fn listeners_sum_across_mounts() {
        // Mid-failover: an external listener parked on silence still counts.
        let j = r#"{"icestats":{"source":[
            {"listenurl":"http://localhost:8000/silence.mp3","listeners":2}]}}"#;
        let f = parse_status(j).unwrap();
        assert!(!f.live);
        assert_eq!(f.listeners, 1);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_status("not json").is_err());
        assert!(parse_status(r#"{"unexpected":true}"#).is_err());
    }
}
