//! UTF-8 -> MacRoman transcoding for the wire.
//!
//! TurboGopher on Mac OS 9 decodes bytes as MacRoman, so once we echo Spotify
//! track/artist names (accents, smart quotes, non-Latin scripts) the gophermap
//! bytes must be MacRoman, not UTF-8. The whole rendered `.gph` is passed through
//! [`encode`] at the IO edge: ASCII (< 0x80) — including every structural byte
//! `[ ] | \t \n` geomyidae parses — is identity, and only the accented display
//! text is remapped. Codepoints MacRoman can't represent become `?`.

/// MacRoman's upper half: the Unicode codepoint for each byte 0x80..=0xFF.
/// (Apple's canonical Mac OS Roman table, with 0xDB as the euro sign.)
const HIGH: [u32; 128] = [
    0x00C4, 0x00C5, 0x00C7, 0x00C9, 0x00D1, 0x00D6, 0x00DC, 0x00E1, 0x00E0, 0x00E2, 0x00E4, 0x00E3, 0x00E5, 0x00E7,
    0x00E9, 0x00E8, 0x00EA, 0x00EB, 0x00ED, 0x00EC, 0x00EE, 0x00EF, 0x00F1, 0x00F3, 0x00F2, 0x00F4, 0x00F6, 0x00F5,
    0x00FA, 0x00F9, 0x00FB, 0x00FC, 0x2020, 0x00B0, 0x00A2, 0x00A3, 0x00A7, 0x2022, 0x00B6, 0x00DF, 0x00AE, 0x00A9,
    0x2122, 0x00B4, 0x00A8, 0x2260, 0x00C6, 0x00D8, 0x221E, 0x00B1, 0x2264, 0x2265, 0x00A5, 0x00B5, 0x2202, 0x2211,
    0x220F, 0x03C0, 0x222B, 0x00AA, 0x00BA, 0x03A9, 0x00E6, 0x00F8, 0x00BF, 0x00A1, 0x00AC, 0x221A, 0x0192, 0x2248,
    0x2206, 0x00AB, 0x00BB, 0x2026, 0x00A0, 0x00C0, 0x00C3, 0x00D5, 0x0152, 0x0153, 0x2013, 0x2014, 0x201C, 0x201D,
    0x2018, 0x2019, 0x00F7, 0x25CA, 0x00FF, 0x0178, 0x2044, 0x20AC, 0x2039, 0x203A, 0xFB01, 0xFB02, 0x2021, 0x00B7,
    0x201A, 0x201E, 0x2030, 0x00C2, 0x00CA, 0x00C1, 0x00CB, 0x00C8, 0x00CD, 0x00CE, 0x00CF, 0x00CC, 0x00D3, 0x00D4,
    0xF8FF, 0x00D2, 0x00DA, 0x00DB, 0x00D9, 0x0131, 0x02C6, 0x02DC, 0x00AF, 0x02D8, 0x02D9, 0x02DA, 0x00B8, 0x02DD,
    0x02DB, 0x02C7,
];

/// Transcode a UTF-8 string to MacRoman bytes. ASCII passes through; mapped
/// codepoints become their MacRoman byte; anything else becomes `?`.
pub fn encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp < 0x80 {
            out.push(cp as u8);
        } else if let Some(i) = HIGH.iter().position(|&h| h == cp) {
            out.push(0x80 + i as u8);
        } else if cp == 0x00A4 {
            // Currency sign shares the euro slot on classic MacRoman.
            out.push(0xDB);
        } else {
            out.push(b'?');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        assert_eq!(encode("Now Playing /spot/now [1|x|/y|server|port]"), b"Now Playing /spot/now [1|x|/y|server|port]");
    }

    #[test]
    fn portuguese_accents_map_to_macroman_bytes() {
        // é 0x8E, ç 0x8D, ã 0x8B, í 0x92, ó 0x97, ú 0x9C, õ 0x9B
        assert_eq!(encode("café"), vec![b'c', b'a', b'f', 0x8E]);
        assert_eq!(
            encode("Construção"),
            vec![b'C', b'o', b'n', b's', b't', b'r', b'u', 0x8D, 0x8B, b'o'],
        );
        assert_eq!(encode(" é í ó ú õ"), vec![b' ', 0x8E, b' ', 0x92, b' ', 0x97, b' ', 0x9C, b' ', 0x9B]);
    }

    #[test]
    fn structural_bytes_survive_around_accents() {
        let gph = "[1|Não|/spot/x|server|port]\n";
        let out = encode(gph);
        assert_eq!(out[0], b'[');
        assert_eq!(out[1], b'1');
        assert_eq!(out[2], b'|');
        assert_eq!(*out.last().unwrap(), b'\n');
        // N ã o -> the ã became 0x8B, still bracketed by ASCII structure
        assert!(out.windows(3).any(|w| w == [b'N', 0x8B, b'o']));
    }

    #[test]
    fn unmappable_becomes_question_mark() {
        assert_eq!(encode("日本"), vec![b'?', b'?']);
        assert_eq!(encode("😀"), vec![b'?']);
    }
}
