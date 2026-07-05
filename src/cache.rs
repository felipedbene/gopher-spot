//! A tiny file-backed TTL cache.
//!
//! The dcgi is a short-lived per-request process (geomyidae exec's it anew each
//! time), so an in-process cache would never survive between requests — the
//! PROMPT's "cache em memória" is realized on disk. Entries are `expiry\npayload`
//! files named by a hash of the key, in a writable state dir (an emptyDir in the
//! pod). Per-replica, which is fine: the caches just warm independently.
//!
//! Used for the access token (TTL = expires_in - slack), search (5 min), devices
//! (30 s), playlists (60 s, Fio D), and — via the byte-safe variants below —
//! album cover JPEGs (fio S2), whose raw bytes are not valid UTF-8.

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
/// An expired entry is removed on the way out (GS-04): reads are the only
/// recurring visitor these files get, so this keeps re-requested keys from
/// piling up dead generations. Best-effort — racing a concurrent re-put can at
/// worst delete a just-renamed fresh entry, which is one extra cache miss.
pub fn get(dir: &Path, key: &str, now_unix: i64) -> Option<String> {
    let file = key_file(dir, key);
    let data = std::fs::read_to_string(&file).ok()?;
    let (exp, payload) = data.split_once('\n')?;
    if now_unix >= exp.trim().parse::<i64>().ok()? {
        let _ = std::fs::remove_file(&file);
        return None;
    }
    Some(payload.to_string())
}

/// Store `payload` under `key`, expiring `ttl_secs` from `now_unix`. Best-effort:
/// a write failure just means a cache miss next time.
pub fn put(dir: &Path, key: &str, now_unix: i64, ttl_secs: i64, payload: &str) {
    let _ = std::fs::create_dir_all(dir);
    write_atomic(
        &key_file(dir, key),
        format!("{}\n{}", now_unix + ttl_secs, payload).as_bytes(),
    );
}

/// Write via a same-dir temp file + rename (GS-05): concurrent per-request dcgi
/// processes write the same entries (e.g. `now_snapshot` on a TTL rollover, the
/// token at cold start), and a bare `fs::write` lets a reader see a torn,
/// half-written file. rename(2) is atomic within a filesystem, so a reader gets
/// either the old or the new entry, never a mix. The pid in the temp name keeps
/// concurrent writers off each other's temp; a crash can strand one (bounded:
/// one per pid), swept away with the emptyDir on pod restart. Still best-effort.
fn write_atomic(file: &Path, bytes: &[u8]) {
    let tmp = file.with_file_name(format!(
        "{}.tmp.{}",
        file.file_name().and_then(|n| n.to_str()).unwrap_or("k"),
        std::process::id()
    ));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// Byte-safe read: return the cached raw payload for `key` if present and unexpired.
/// Same `expiry\npayload` framing as [`get`], but the payload may be arbitrary
/// bytes (a JPEG cover), so we never go through `String`. The expiry prefix is
/// always ASCII digits, so splitting on the first `\n` byte is unambiguous.
/// Expired entries are removed on read, like [`get`] (GS-04).
pub fn get_bytes(dir: &Path, key: &str, now_unix: i64) -> Option<Vec<u8>> {
    let file = key_file(dir, key);
    let data = std::fs::read(&file).ok()?;
    let nl = data.iter().position(|&b| b == b'\n')?;
    let exp: i64 = std::str::from_utf8(&data[..nl]).ok()?.trim().parse().ok()?;
    if now_unix >= exp {
        let _ = std::fs::remove_file(&file);
        return None;
    }
    Some(data[nl + 1..].to_vec())
}

/// Byte-safe store: like [`put`], but for a raw byte payload. Best-effort.
pub fn put_bytes(dir: &Path, key: &str, now_unix: i64, ttl_secs: i64, payload: &[u8]) {
    let _ = std::fs::create_dir_all(dir);
    let mut buf = format!("{}\n", now_unix + ttl_secs).into_bytes();
    buf.extend_from_slice(payload);
    write_atomic(&key_file(dir, key), &buf);
}

/// Drop a cached entry now, ignoring whether it existed (best-effort). Used to
/// bust the `/now` micro-cache (fio S3/2) when a command changes playback state,
/// so the next `/now` re-fetches instead of serving a stale snapshot.
pub fn remove(dir: &Path, key: &str) {
    let _ = std::fs::remove_file(key_file(dir, key));
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
    fn expired_entry_is_removed_on_read() {
        // GS-04: the expired read is the reaper — after the miss, the file is
        // gone (for both the string and byte variants).
        let d = tmp("reap");
        put(&d, "search:old", 1000, 30, "x");
        put_bytes(&d, "cover:old:640", 1000, 30, &[0xFF, 0xD8]);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2);
        assert_eq!(get(&d, "search:old", 2000), None);
        assert_eq!(get_bytes(&d, "cover:old:640", 2000), None);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "reaped on read");
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

    #[test]
    fn atomic_put_leaves_no_tmp_files() {
        // GS-05: writes go through temp+rename — after a put the dir holds only
        // final entries, and a reader can never observe a half-written file.
        let d = tmp("atomic");
        put(&d, "k", 0, 100, "v");
        put_bytes(&d, "kb", 0, 100, &[0xFF, 0xD8]);
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "exactly the two entries: {names:?}");
        assert!(names.iter().all(|n| !n.contains(".tmp.")), "{names:?}");
    }

    #[test]
    fn bytes_round_trip_including_non_utf8() {
        let d = tmp("bytes");
        // JPEG SOI + a byte that is invalid UTF-8 (0xFF) + an embedded newline,
        // to prove the cover cache is byte-exact and framing-safe.
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, b'\n', 0x00, 0xFF, 0xD9];
        put_bytes(&d, "cover:al1:640", 1000, 86_400, &jpeg);
        assert_eq!(get_bytes(&d, "cover:al1:640", 1000), Some(jpeg.to_vec()));
        assert_eq!(get_bytes(&d, "cover:al1:640", 87_401), None); // expired
        assert_eq!(get_bytes(&d, "never", 0), None);
    }
}
