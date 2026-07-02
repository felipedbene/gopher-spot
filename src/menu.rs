//! The menu model for gopher-spot, built on `gopher-core`'s `Entry`/`.gph`
//! serializer. The root menu is a fixed list; the `/spot/*` menus (Fio B: mock)
//! are assembled in `dcgi.rs`.
//!
//! Width + charset: display strings stay <= 66 columns (RFC 1436) and ASCII, so
//! the bytes are identical in UTF-8 and MacRoman and the OS 9 gopher client renders
//! them clean. When Fio C starts echoing Spotify track/artist names (which carry
//! accents and non-Latin scripts), those dynamic strings will need a UTF-8 ->
//! MacRoman transcode at the IO edge — flagged there, not needed yet.

use gopher_core::{info, link, render_menu_index, Entry, ItemKind};

/// RFC 1436 display-width budget (PROMPT constraint).
pub const MAX_WIDTH: usize = 66;

/// Clip a (possibly multi-byte) display string to [`MAX_WIDTH`] columns, adding
/// an ellipsis when it overflows. Counts chars, not bytes, so accents don't eat
/// budget; the MacRoman transcode at the IO edge maps each char to one byte.
pub fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= MAX_WIDTH {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_WIDTH - 3).collect();
    out.push_str("...");
    out
}

/// The static root menu (PROMPT's selector list). A fixed set of links; the
/// endpoints behind them are wired in later fios.
pub fn root_entries() -> Vec<Entry> {
    vec![
        info("Spotify pelo Gopher, safadinho."),
        info(""),
        link(ItemKind::Menu, "Now Playing", "/spot/now"),
        link(ItemKind::Search, "Buscar", "/spot/search"),
        link(ItemKind::Menu, "Minhas playlists", "/spot/playlists"),
        link(ItemKind::Menu, "Controles", "/spot/control"),
        // The type-s (sound) reopen item is appended by `root_gph` — gopher-core's
        // ItemKind has no Sound kind (see `sound_line`).
    ]
}

/// The full root menu as a geomyidae `.gph`, including the type-s PLS reopen
/// item. Used both by the `root` subcommand (baked to /srv/index.gph) and by the
/// dcgi `/spot` route, so there is one source of truth.
pub fn root_gph() -> String {
    let mut out = render_menu_index(&root_entries());
    out.push_str(&sound_line("Reabrir stream (MacAST)", "/spot/stream.pls"));
    out
}

/// A type-s (sound) `.gph` line. gopher-core's `ItemKind` covers 0/1/7/h/9 but
/// not `s`, so we format this one line in the same `[type|name|sel|server|port]`
/// shape geomyidae fills in. (Candidate to upstream into gopher-core as
/// `ItemKind::Sound`.) `server`/`port` are geomyidae's placeholder tokens.
pub fn sound_line(display: &str, selector: &str) -> String {
    format!(
        "[s|{}|{}|server|port]\n",
        gph_escape(display),
        gph_escape(selector)
    )
}

fn gph_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every display column stays within the RFC 1436 budget and is ASCII (so
    /// UTF-8 == MacRoman on the wire). Guards against a future edit sneaking in a
    /// wide or accented literal.
    #[test]
    fn root_displays_are_ascii_and_within_width() {
        for e in root_entries() {
            let s = match &e {
                Entry::Info(s) => s.clone(),
                Entry::Link { display, .. } => display.clone(),
            };
            assert!(s.is_ascii(), "non-ASCII display would break MacRoman: {s:?}");
            assert!(s.len() <= MAX_WIDTH, "display over {MAX_WIDTH} cols: {s:?}");
        }
    }

    #[test]
    fn root_gph_has_the_five_items_and_no_tabs() {
        let gph = root_gph();
        assert!(!gph.contains('\t'), "gophermap uses [] lines, never raw tabs");
        assert!(gph.contains("[1|Now Playing|/spot/now|server|port]"));
        assert!(gph.contains("[7|Buscar|/spot/search|server|port]"));
        assert!(gph.contains("[1|Minhas playlists|/spot/playlists|server|port]"));
        assert!(gph.contains("[1|Controles|/spot/control|server|port]"));
        assert!(gph.contains("[s|Reabrir stream (MacAST)|/spot/stream.pls|server|port]"));
    }
}
