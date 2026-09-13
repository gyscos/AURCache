//! Byte-window helpers for walking a build log in bounded pages.
//!
//! The `/output` endpoint serves raw file bytes sliced by byte offset. A page
//! can start or end inside a multi-byte UTF-8 character, and each consumer —
//! the CLI's pager and the frontend's log window — has to resolve that the
//! same way. The one function that needs to be identical everywhere, so the
//! offsets both ends compute always agree, lives here.

/// Resolve a raw page of build log to a UTF-8-aligned slice, and say how far
/// the page actually advanced.
///
/// Returns `(front_skip, back_drop)`:
///
/// - `front_skip` is how many leading bytes to drop before the slice is
///   displayable — the *tail* of a character that started on the previous
///   page. A page normally starts at a boundary (the start of the log, a
///   re-aligned offset, or `log_size - cap`), so this is usually zero.
/// - `back_drop` is how many trailing bytes to drop — the *start* of a
///   character that is still being written, or that got cut by the page's
///   `limit`. The caller advances its next offset by
///   `offset + raw.len() - back_drop`, and that re-reads the character whole
///   instead of printing two mangled halves.
///
/// The slice `&raw[front_skip..raw.len() - back_drop]` is valid UTF-8 at both
/// edges (interior bytes are the log's problem, and are lossy-decoded by the
/// caller). The two values sum to at most `raw.len()`, and to all of it when
/// the page is nothing but a split character — which is why callers also stop
/// when `raw.len() - back_drop == 0`: no forward progress, so EOF mid-character
/// terminates rather than re-reading the same tail forever.
pub fn align(raw: &[u8]) -> (usize, usize) {
    if raw.is_empty() {
        return (0, 0);
    }

    // Leading continuation bytes continue a character started before the
    // page. Full stop — that character cannot be recovered from what we have
    // been given, so it is dropped from the display.
    let front_skip = raw.iter().take_while(|b| (**b & 0xC0) == 0x80).count();

    // Everything from the page's last valid UTF-8 prefix is either a cut-off
    // character or garbage; neither is printable. `valid_up_to` is where
    // decoding failed, which for a page that ends mid-character is the start
    // of that character's remaining bytes.
    let valid = match std::str::from_utf8(&raw[front_skip..]) {
        Ok(_) => raw.len(),
        Err(e) => front_skip + e.valid_up_to(),
    };

    (front_skip, raw.len() - valid)
}

#[cfg(test)]
mod tests {
    use super::align;

    /// The trivial case: an already-aligned page is dropped as-is.
    #[test]
    fn a_complete_page_needs_no_adjustment() {
        assert_eq!(align(b""), (0, 0));
        assert_eq!(align(b"hello\n"), (0, 0));
        assert_eq!(
            align("unrecognized option \u{2018}a\u{2019}\n".as_bytes()),
            (0, 0)
        );
    }

    /// The whole point of the rewind: cut a 3-byte character (U+2018, which
    /// gcc quotes its diagnostics with) and the page drops the split byte — but
    /// the rewind offset re-reads the character whole.
    #[test]
    fn a_split_character_is_dropped_and_re_read_whole() {
        let raw = "unrecognized option \u{2018}".as_bytes();
        // Cut after 'unrecognized option ' (index 20) plus the leading byte of
        // U+2018: the page is 21 bytes, of which the last is half a character.
        assert_eq!(align(&raw[..21]), (0, 1));
        // `next_offset = 0 + 21 - 1 = 20` re-reads the character whole.
        assert_eq!(align(&raw[20..]), (0, 0));
        assert_eq!(&raw[20..23], "\u{2018}".as_bytes());
    }

    /// A page whose first bytes continue a character from the previous page
    /// cannot recover that character; those bytes are dropped from display.
    #[test]
    fn leading_continuation_bytes_are_dropped() {
        let raw = b"\x80\x98-fno_char8_t\n";
        assert_eq!(align(raw), (2, 0));
    }

    /// A page of nothing but one character's remaining bytes is all slack:
    /// nothing to display, and (for the display slice) no character at all.
    #[test]
    fn a_page_of_only_continuation_bytes_has_nothing_to_show() {
        let raw = b"\x80\x98";
        let (front, back) = align(raw);
        assert_eq!((front, back), (2, 0));
        assert_eq!(&raw[front..raw.len() - back], b"");
    }

    /// The zero-progress case that makes EOF mid-character terminate: a page
    /// containing only the leading byte of a character neither displays nor
    /// advances the offset, so the walker must stop rather than re-read the
    /// same byte forever.
    #[test]
    fn an_entire_page_of_a_split_character_is_zero_progress() {
        let raw = b"\xE2";
        assert_eq!(align(raw), (0, 1));
        assert_eq!(raw.len() - 1, 0);
    }

    /// A totally empty page advances nothing by definition; it is how a walker
    /// knows it has caught up with a log that ends on a character boundary.
    #[test]
    fn an_empty_page_is_zero_progress() {
        assert_eq!(align(b""), (0, 0));
    }

    /// Interior garbage is left for the caller's lossy decode: the edges stay
    /// clean, but only a page ending at the garbage shows a clean boundary.
    #[test]
    fn interior_garbage_extends_to_the_ends_only_when_there() {
        let raw = b"before\xFFafter";
        assert_eq!(align(&raw[..6]), (0, 0));
        // A page that *includes* the garbage ends inside the invalid region.
        assert_eq!(align(&raw[..9]), (0, 3));
    }
}
