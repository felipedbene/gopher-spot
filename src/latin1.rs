//! UTF-8 -> ISO-8859-1 (Latin-1) transcoding for the wire.
//!
//! The real OS 9 client is **Netscape Communicator**, whose Gopher renderer treats
//! charset-less text as Latin-1, NOT Mac OS Roman. Proof: sending `Você` as MacRoman
//! (`ê` = 0x90) blanks the line in Netscape, because 0x90 is a C1 control byte in
//! Latin-1. Latin-1 puts every Portuguese accent in 0xC0..=0xFF (`ê`=0xEA, `ç`=0xE7,
//! `ã`=0xE3, `õ`=0xF5, `á`=0xE1 …), which Netscape renders correctly.
//!
//! ASCII (< 0x80) — every structural byte `[ ] | \t \n` geomyidae parses — is
//! identity. Code points 0x80..=0xFF map to their own byte (Latin-1 == the first 256
//! Unicode scalars). Common typographic punctuation from Spotify titles (smart
//! quotes, en/em dashes, ellipsis) is folded to an ASCII lookalike so it doesn't turn
//! into `?`. Anything else becomes `?`.

/// Transcode a UTF-8 string to Latin-1 bytes (see module docs).
pub fn encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp <= 0xFF {
            // ASCII and Latin-1 supplement both map one code point -> one byte.
            out.push(cp as u8);
        } else {
            // Fold the punctuation Spotify uses a lot to ASCII rather than '?'.
            match cp {
                0x2018 | 0x2019 => out.push(b'\''),      // ' '
                0x201C | 0x201D => out.push(b'"'),       // " "
                0x2013 | 0x2014 => out.push(b'-'),       // en / em dash
                0x2026 => out.extend_from_slice(b"..."), // ellipsis
                _ => out.push(b'?'),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        assert_eq!(
            encode("Now Playing /spot/now [1|x|/y|server|port]"),
            b"Now Playing /spot/now [1|x|/y|server|port]"
        );
    }

    #[test]
    fn portuguese_accents_map_to_latin1_bytes() {
        // ê=0xEA ç=0xE7 ã=0xE3 é=0xE9 í=0xED ó=0xF3 ú=0xFA õ=0xF5
        assert_eq!(encode("Você"), vec![b'V', b'o', b'c', 0xEA]);
        assert_eq!(encode("café"), vec![b'c', b'a', b'f', 0xE9]);
        assert_eq!(
            encode("Construção"),
            vec![b'C', b'o', b'n', b's', b't', b'r', b'u', 0xE7, 0xE3, b'o'],
        );
    }

    #[test]
    fn structural_bytes_survive_around_accents() {
        let gph = "[1|Não|/spot/x|server|port]\n";
        let out = encode(gph);
        assert_eq!(out[0], b'[');
        assert_eq!(*out.last().unwrap(), b'\n');
        assert!(out.windows(3).any(|w| w == [b'N', 0xE3, b'o'])); // N ã o
    }

    #[test]
    fn smart_punctuation_folds_to_ascii() {
        assert_eq!(encode("Don\u{2019}t"), b"Don't");
        assert_eq!(encode("A \u{2013} B"), b"A - B");
        assert_eq!(encode("wait\u{2026}"), b"wait...");
    }

    #[test]
    fn unmappable_becomes_question_mark() {
        assert_eq!(encode("日本"), vec![b'?', b'?']);
        assert_eq!(encode("😀"), vec![b'?']);
    }
}
