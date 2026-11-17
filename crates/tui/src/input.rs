//! Editable text with a cursor: the message box, the chat filter, the command bar, search, the
//! emoji picker's query, the file prompt and settings values all edit through this. Pure and
//! tested without a terminal.
//!
//! The cursor is an index in grapheme clusters, so it can never sit inside an emoji or between a
//! letter and its accent, and one Backspace removes one whole flag, skin-tone emoji or ZWJ
//! family. Byte offsets exist only inside this file, converted on the way in and out.

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{clean_line, cluster_width, graphemes, sw};

/// Text being typed, and where the cursor is in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    /// In grapheme clusters, 0..=count.
    cursor: usize,
}

/// How a character counts for word movement: spaces, word characters, everything else.
pub(crate) fn class(g: &str) -> u8 {
    match g.chars().next() {
        _ if g.trim().is_empty() => 0,
        Some(c) if c.is_alphanumeric() || c == '_' => 1,
        _ => 2,
    }
}

impl From<&str> for TextInput {
    /// `text` with the cursor at its end.
    fn from(text: &str) -> TextInput {
        let mut t = TextInput::default();
        t.set(text);
        t
    }
}

impl From<String> for TextInput {
    fn from(text: String) -> TextInput {
        TextInput::from(text.as_str())
    }
}

impl PartialEq<str> for TextInput {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for TextInput {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for TextInput {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl std::fmt::Display for TextInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl TextInput {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Cursor position in grapheme clusters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn count(&self) -> usize {
        self.text.graphemes(true).count()
    }

    /// Byte offset of grapheme index `g` (the end of the text when past it).
    fn byte_at(&self, g: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .nth(g)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Text before the cursor, for what completion looks at.
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.byte_at(self.cursor)]
    }

    /// Replaces the text; the cursor goes to the end.
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Inserts already-cleaned text at the cursor (the caller strips control characters). The
    /// cursor ends after what was inserted, even when it joined the cluster before it (a ZWJ, a
    /// skin tone, an accent).
    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.cursor = self.text[..at + s.len()].graphemes(true).count();
    }

    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.count());
    }

    /// Start of the word before the cursor: skips spaces, then one run of one kind of character.
    fn word_start(&self) -> usize {
        let gs: Vec<&str> = graphemes(&self.text).collect();
        let mut i = self.cursor.min(gs.len());
        while i > 0 && class(gs[i - 1]) == 0 {
            i -= 1;
        }
        if i > 0 {
            let c = class(gs[i - 1]);
            while i > 0 && class(gs[i - 1]) == c {
                i -= 1;
            }
        }
        i
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// To the start of the next word: past the run under the cursor, then past spaces.
    pub fn word_right(&mut self) {
        let gs: Vec<&str> = graphemes(&self.text).collect();
        let mut i = self.cursor.min(gs.len());
        if i < gs.len() && class(gs[i]) != 0 {
            let c = class(gs[i]);
            while i < gs.len() && class(gs[i]) == c {
                i += 1;
            }
        }
        while i < gs.len() && class(gs[i]) == 0 {
            i += 1;
        }
        self.cursor = i;
    }

    /// Grapheme range of the line (up to `\n`) the cursor is on.
    fn line_range(&self) -> (usize, usize) {
        let gs: Vec<&str> = graphemes(&self.text).collect();
        let at = self.cursor.min(gs.len());
        let start = gs[..at]
            .iter()
            .rposition(|g| *g == "\n")
            .map_or(0, |i| i + 1);
        let end = gs[at..]
            .iter()
            .position(|g| *g == "\n")
            .map_or(gs.len(), |i| at + i);
        (start, end)
    }

    pub fn line_start(&mut self) {
        self.cursor = self.line_range().0;
    }

    pub fn line_end(&mut self) {
        self.cursor = self.line_range().1;
    }

    /// Removes graphemes `from..to`; the cursor goes to `from`.
    pub(crate) fn remove(&mut self, from: usize, to: usize) {
        if from < to {
            let (a, b) = (self.byte_at(from), self.byte_at(to));
            self.text.replace_range(a..b, "");
        }
        self.cursor = from;
    }

    /// Backspace: the whole cluster before the cursor.
    pub fn delete_back(&mut self) {
        if self.cursor > 0 {
            self.remove(self.cursor - 1, self.cursor);
        }
    }

    pub fn delete_forward(&mut self) {
        if self.cursor < self.count() {
            self.remove(self.cursor, self.cursor + 1);
        }
    }

    /// Ctrl+W: from the start of the word before the cursor to the cursor.
    pub fn delete_word_back(&mut self) {
        self.remove(self.word_start(), self.cursor);
    }

    /// Ctrl+U: empties the line the cursor is on (the whole text when it is one line).
    pub fn clear_line(&mut self) {
        let (a, b) = self.line_range();
        self.remove(a, b);
    }

    /// Replaces the `n` clusters before the cursor with `with` (completion).
    pub fn replace_before_cursor(&mut self, n: usize, with: &str) {
        let n = n.min(self.cursor);
        self.remove(self.cursor - n, self.cursor);
        self.insert_str(with);
    }

    /// One line of a single-line box `width` columns wide: the text shown and the cursor's
    /// column in it. The view scrolls so the cursor stays on screen, with a `…` where text is cut
    /// off on the left. One column is kept free so a cursor at the end still has a cell. Each
    /// cluster is shown through `clean_line`, so a control or bidi character can never reach the
    /// terminal. With the cursor at the end this is `text::tail` of the cleaned text.
    pub fn view(&self, width: usize) -> (String, usize) {
        let room = width.saturating_sub(1);
        if room == 0 {
            return (String::new(), 0);
        }
        let cleaned: Vec<String> = graphemes(&self.text).map(clean_line).collect();
        let gs: Vec<&str> = cleaned.iter().map(String::as_str).collect();
        let at = self.cursor.min(gs.len());
        let before: usize = gs[..at].iter().map(|g| cluster_width(g)).sum();
        if before <= room {
            let mut shown = String::new();
            let mut used = 0;
            for g in &gs {
                let w = cluster_width(g);
                if used + w > room {
                    break;
                }
                shown.push_str(g);
                used += w;
            }
            return (shown, before);
        }
        // Cut on the left: an ellipsis, then as much before the cursor as fits.
        let mut start = at;
        let mut used = 1;
        while start > 0 && used + cluster_width(gs[start - 1]) <= room {
            start -= 1;
            used += cluster_width(gs[start]);
        }
        let mut shown = String::from('\u{2026}');
        shown.extend(gs[start..at].iter().copied());
        let col = sw(&shown);
        for g in &gs[at..] {
            if sw(&shown) + cluster_width(g) > room {
                break;
            }
            shown.push_str(g);
        }
        (shown, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> TextInput {
        TextInput::from(text)
    }

    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    const FLAG: &str = "\u{1F1FA}\u{1F1F3}";
    const THUMB: &str = "\u{1F44D}\u{1F3FD}";

    #[test]
    fn inserts_at_the_cursor_for_ascii_accents_and_emoji() {
        let mut d = t("helo");
        d.left();
        d.insert_char('l');
        assert_eq!((d.text(), d.cursor()), ("hello", 4));

        let mut d = t("cafe");
        d.insert_char('\u{301}');
        // The accent joined the e: still 4 clusters, cursor after them.
        assert_eq!((d.text(), d.cursor()), ("cafe\u{301}", 4));

        let mut d = t("ab");
        d.left();
        d.insert_str(FAMILY);
        assert_eq!(d.text(), format!("a{FAMILY}b"));
        assert_eq!(d.cursor(), 2);
    }

    #[test]
    fn backspace_removes_one_whole_emoji() {
        for emoji in [
            FAMILY,
            FLAG,
            THUMB,
            "\u{2764}\u{FE0F}",
            "1\u{FE0F}\u{20E3}",
            "e\u{301}",
        ] {
            let mut d = t(&format!("hi{emoji}"));
            d.delete_back();
            assert_eq!(d.text(), "hi", "{emoji:?}");
            assert_eq!(d.cursor(), 2);
        }
        let mut d = t(&format!("{FLAG}x"));
        d.left();
        d.delete_forward();
        assert_eq!(d.text(), FLAG);
        d.delete_forward();
        assert_eq!(d.text(), FLAG, "nothing after the cursor");
        d.left();
        d.delete_forward();
        assert_eq!(d.text(), "");
    }

    #[test]
    fn delete_at_the_ends_is_harmless() {
        let mut d = TextInput::default();
        d.delete_back();
        d.delete_forward();
        d.left();
        d.word_left();
        d.word_right();
        d.line_start();
        d.line_end();
        d.delete_word_back();
        d.clear_line();
        assert_eq!((d.text(), d.cursor()), ("", 0));
    }

    #[test]
    fn cursor_moves_by_cluster() {
        let mut d = t(&format!("a{FAMILY}b"));
        assert_eq!(d.cursor(), 3);
        d.left();
        d.left();
        assert_eq!(d.cursor(), 1);
        d.right();
        assert_eq!(d.cursor(), 2);
        d.right();
        d.right();
        assert_eq!(d.cursor(), 3, "stops at the end");
    }

    #[test]
    fn word_movement_and_word_delete() {
        let mut d = t("one two,  three");
        d.word_left();
        assert_eq!(d.cursor(), 10);
        d.word_left();
        assert_eq!(d.cursor(), 7, "punctuation is its own run");
        d.word_left();
        assert_eq!(d.cursor(), 4);
        d.word_right();
        assert_eq!(d.cursor(), 7);
        d.word_right();
        assert_eq!(d.cursor(), 10);
        d.word_right();
        d.word_right();
        assert_eq!(d.cursor(), 15);

        let mut d = t("say hello world");
        d.delete_word_back();
        assert_eq!((d.text(), d.cursor()), ("say hello ", 10));
        d.delete_word_back();
        assert_eq!(d.text(), "say ");
    }

    #[test]
    fn home_end_and_clear_work_on_the_current_line() {
        let mut d = t("first\nsecond line\nthird");
        for _ in 0..6 {
            d.left();
        }
        // Cursor now on the newline that ends "second line" (grapheme 17).
        d.line_start();
        assert_eq!(d.cursor(), 6);
        d.right();
        d.line_end();
        assert_eq!(d.cursor(), 17);
        d.clear_line();
        assert_eq!(d.text(), "first\n\nthird");
        assert_eq!(d.cursor(), 6);
    }

    #[test]
    fn before_cursor_and_replace() {
        let mut d = t("hi :thu there");
        for _ in 0..6 {
            d.left();
        }
        assert_eq!(d.before_cursor(), "hi :thu");
        d.replace_before_cursor(4, "\u{1F44D}");
        assert_eq!(d.text(), "hi \u{1F44D} there");
        assert_eq!(d.cursor(), 4);
    }

    #[test]
    fn set_clear_and_comparisons() {
        let mut d = t("abc");
        assert_eq!(d, "abc");
        assert_eq!(d.cursor(), 3);
        assert_eq!(d.to_string(), "abc");
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.cursor(), 0);
    }

    #[test]
    fn with_the_cursor_at_the_end_the_view_keeps_the_end_of_the_text() {
        let fam3 = FAMILY.repeat(3);
        for (text, width, want) in [
            ("short", 20, "short".to_string()),
            ("abc", 5, "abc".to_string()),
            ("abcdef", 5, "\u{2026}def".to_string()),
            (
                "\u{4f60}\u{597d}\u{4e16}\u{754c}",
                6,
                "\u{2026}\u{4e16}\u{754c}".to_string(),
            ),
            // A family emoji is 2 columns: 5 free columns keep two of them after the ellipsis.
            (fam3.as_str(), 6, format!("\u{2026}{FAMILY}{FAMILY}")),
            ("", 5, String::new()),
            ("abc", 1, String::new()),
        ] {
            let (shown, col) = t(text).view(width);
            assert_eq!(shown, want, "{text:?} {width}");
            assert_eq!(col, sw(&shown), "{text:?} {width}");
        }
        // Never a split cluster, never wider than the box, for every width.
        for text in [
            fam3.as_str(),
            "a\u{301}b\u{301}c\u{301}d\u{301}",
            "\u{4f60}x\u{597d}y",
        ] {
            let all: Vec<&str> = graphemes(text).collect();
            for width in 1..=12 {
                let (shown, _) = t(text).view(width);
                assert!(sw(&shown) < width.max(1), "{text:?} {width}: {shown:?}");
                let kept: Vec<&str> = graphemes(shown.trim_start_matches('\u{2026}')).collect();
                assert_eq!(kept, all[all.len() - kept.len()..], "{text:?} {width}");
            }
        }
    }

    #[test]
    fn the_view_never_shows_control_or_bidi_characters() {
        // A right-to-left override would flip what follows; a tab or escape would move the
        // cursor. The text keeps them, the screen never sees them.
        let d = t("a\u{202E}b\tc\u{1b}d");
        let (shown, col) = d.view(20);
        assert_eq!(shown, "ab c d");
        assert_eq!(col, 6);
        assert_eq!(d.text(), "a\u{202E}b\tc\u{1b}d");
    }

    #[test]
    fn the_view_keeps_the_cursor_on_screen() {
        let mut d = t("abcdefghijklmnop");
        d.line_start();
        // The start of a long text: shown from the left, cursor at 0.
        assert_eq!(d.view(6), ("abcde".to_string(), 0));
        for _ in 0..3 {
            d.right();
        }
        assert_eq!(d.view(6), ("abcde".to_string(), 3));
        // Past the right edge: cut on the left, cursor still inside the box.
        for _ in 0..7 {
            d.right();
        }
        let (shown, col) = d.view(6);
        assert!(shown.starts_with('\u{2026}'), "{shown}");
        assert!(col < 6, "{shown} {col}");
        assert!(sw(&shown) <= 5, "{shown}");
        assert_eq!(&shown[shown.len() - 1..], "j", "{shown}");
    }
}
