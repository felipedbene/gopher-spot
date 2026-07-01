//! A tiny file-backed TTL cache.
//!
//! The dcgi is a short-lived per-request process (geomyidae exec's it anew each
//! time), so an in-process cache would never survive between requests — the
//! PROMPT's "cache em memória" is realized on disk. Entries are `expiry\npayload`
//! files named by a hash of the key, in a writable state dir (an emptyDir in the
//! pod). Per-replica, which is fine: the caches just warm independently.
//!
//! Used for the access token (TTL = expires_in - slack), search (5 min), devices
//! (30 s), and playlists (60 s, Fio D).

use std::path::{Path, PathBuf};

/// FNV-1a/64 of the key -> the cache file name (keeps arbitrary keys, e.g. search
/// queries, filesystem-safe).
fn key_file(dir: &Path, key: &str) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    dir.join(format!("{h:016x}"))
}

/// Return the cached payload for `key` if present and not past its expiry.
pub fn get(dir: &Path, key: &str, now_unix: i64) -> Option<String> {
    let data = std::fs::read_to_string(key_file(dir, key)).ok()?;
    let (exp, payload) = data.split_once('\n')?;
    if now_unix >= exp.trim().parse::<i64>().ok()? {
        return None;
    }
    Some(payload.to_string())
}

/// Store `payload` under `key`, expiring `ttl_secs` from `now_unix`. Best-effort:
/// a write failure just means a cache miss next time.
pub fn put(dir: &Path, key: &str, now_unix: i64, ttl_secs: i64, payload: &str) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(key_file(dir, key), format!("{}\n{}", now_unix + ttl_secs, payload));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gopher-spot-cache-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip_within_ttl() {
        let d = tmp("rt");
        put(&d, "search:chico", 1000, 300, "payload-body");
        assert_eq!(get(&d, "search:chico", 1000), Some("payload-body".into()));
        assert_eq!(get(&d, "search:chico", 1299), Some("payload-body".into()));
    }

    #[test]
    fn miss_after_expiry() {
        let d = tmp("exp");
        put(&d, "devices", 1000, 30, "x");
        assert_eq!(get(&d, "devices", 1030), None);
        assert_eq!(get(&d, "devices", 5000), None);
    }

    #[test]
    fn miss_on_unknown_key() {
        let d = tmp("unk");
        assert_eq!(get(&d, "never-written", 0), None);
    }

    #[test]
    fn payload_with_newlines_survives() {
        let d = tmp("multiline");
        let body = "line1\nline2\nline3";
        put(&d, "k", 0, 100, body);
        assert_eq!(get(&d, "k", 1), Some(body.into()));
    }
}
