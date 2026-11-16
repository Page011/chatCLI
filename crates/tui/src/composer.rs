//! The message draft: text plus a cursor, and the editing keys that move it. Pure and tested
//! without a terminal.
//!
//! The cursor is an index in grapheme clusters, so it can never sit inside an emoji or between a
//! letter and its accent, and one Backspace removes one whole flag, skin-tone emoji or ZWJ
//! family. Byte offsets exist only inside this file, converted on the way in and out.

use chatcli_core::model::PersonRef;
use unicode_segmentation::UnicodeSegmentation;

use crate::text::{cluster_width, graphemes};

/// Rows the message box grows to before it scrolls.
pub const MAX_ROWS: usize = 6;

/// A draft being typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    /// In grapheme clusters, 0..=count.
    cursor: usize,
    /// People chosen from the `@` list, one entry per token inserted. Each stands for an
    /// `@name` in the text; a token the user later edited away is dropped at send time.
    pub mentions: Vec<PersonRef>,
}

/// One screen row of a wrapped draft. `start..end` are grapheme indexes into the draft (the
/// newline that ended a line is not part of any row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// How a character counts for word movement: spaces, word characters, everything else.
fn class(g: &str) -> u8 {
    match g.chars().next() {
        _ if g.trim().is_empty() => 0,
        Some(c) if c.is_alphanumeric() || c == '_' => 1,
        _ => 2,
    }
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Cursor position in grapheme clusters.
    #[cfg(test)]
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

    /// Replaces the draft; the cursor goes to the end.
    #[cfg(test)]
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.count();
    }

    /// Inserts already-cleaned text at the cursor (the caller strips control characters; only
    /// `\n` is kept). The cursor ends after what was inserted, even when it joined the cluster
    /// before it (a ZWJ, a skin tone, an accent).
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
    fn remove(&mut self, from: usize, to: usize) {
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

    /// Ctrl+U: empties the line the cursor is on (the whole draft when it is one line).
    pub fn clear_line(&mut self) {
        let (a, b) = self.line_range();
        self.remove(a, b);
    }

    /// Backspace on a mention token the `@` list inserted (`@Name` with the space completion adds,
    /// or without it): removes the whole token and its entry. False when the cursor is not right
    /// after such a token, so the caller falls back to one character.
    pub fn delete_mention_back(&mut self) -> bool {
        let before = self.before_cursor().to_string();
        let mut best: Option<(usize, usize)> = None; // (index in mentions, graphemes to remove)
        for (i, p) in self.mentions.iter().enumerate() {
            let token = format!("@{}", p.name);
            for tail in [format!("{token} "), token.clone()] {
                let Some(head) = before.strip_suffix(&tail) else {
                    continue;
                };
                // Only a token that starts a word: "mail@name" is not a mention.
                if !head.is_empty() && !head.ends_with(char::is_whitespace) {
                    continue;
                }
                let n = tail.graphemes(true).count();
                if best.is_none_or(|(_, b)| n > b) {
                    best = Some((i, n));
                }
            }
        }
        let Some((i, n)) = best else {
            return false;
        };
        self.mentions.remove(i);
        self.remove(self.cursor - n, self.cursor);
        true
    }

    /// The chosen mentions that still stand in `text`: a person is kept once per `@name` token
    /// left in it, in the order they were chosen. The backend replaces each token in list order.
    pub fn mentions_in(&self, text: &str) -> Vec<PersonRef> {
        let mut used: Vec<(String, usize)> = Vec::new();
        let mut out = Vec::new();
        for p in &self.mentions {
            let token = format!("@{}", p.name);
            let seen = match used.iter_mut().find(|(t, _)| *t == token) {
                Some(e) => {
                    e.1 += 1;
                    e.1
                }
                None => {
                    used.push((token.clone(), 1));
                    1
                }
            };
            if text.matches(&token).count() >= seen {
                out.push(p.clone());
            }
        }
        out
    }

    /// Replaces the `n` clusters before the cursor with `with` (completion).
    pub fn replace_before_cursor(&mut self, n: usize, with: &str) {
        let n = n.min(self.cursor);
        self.remove(self.cursor - n, self.cursor);
        self.insert_str(with);
    }

    /// Puts the cursor on the character under a click: `row` and `col` are in the wrapped draft
    /// (`layout(width)`), the column counted in display cells. A click past the end of a row
    /// lands at its end; a wide character takes the click on either of its two cells.
    pub fn click(&mut self, width: usize, row: usize, col: usize) {
        let (rows, _, _) = self.layout(width);
        let Some(r) = rows.get(row.min(rows.len() - 1)) else {
            return;
        };
        let wrapped = rows.get(row + 1).is_some_and(|n| n.start == r.end);
        let (mut used, mut at) = (0, r.start);
        for g in graphemes(&r.text) {
            let w = cluster_width(g);
            if col < used + w {
                break;
            }
            used += w;
            at += 1;
        }
        // The cursor at the end of a wrapped row would draw on the next row.
        self.cursor = if wrapped && at == r.end {
            at.saturating_sub(1).max(r.start)
        } else {
            at
        };
    }

    /// The draft wrapped to `width` columns, and the cursor's row and column. One column is kept
    /// free so a cursor at the end of a full row still has a cell.
    pub fn layout(&self, width: usize) -> (Vec<Row>, usize, usize) {
        let rows = wrap_rows(&self.text, width.saturating_sub(1).max(1));
        let row = rows
            .iter()
            .rposition(|r| r.start <= self.cursor)
            .unwrap_or(0);
        let r = &rows[row];
        let upto = self.cursor.saturating_sub(r.start);
        let col: usize = graphemes(&r.text).take(upto).map(cluster_width).sum();
        (rows, row, col)
    }
}

/// Wraps `text` into rows of at most `width` columns, breaking after the last space that fits
/// and inside a word only when it is wider than the row. Never cuts a cluster. Every
/// line (text between newlines) gets at least one row, so a blank line is visible.
pub fn wrap_rows(text: &str, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut base = 0; // grapheme index where the current line starts in `text`
    for line in text.split('\n') {
        let gs: Vec<&str> = graphemes(line).collect();
        let ws: Vec<usize> = gs.iter().map(|g| cluster_width(g)).collect();
        let mut start = 0;
        loop {
            let (mut used, mut i, mut brk) = (0, start, None);
            while i < gs.len() {
                if used + ws[i] > width && i > start {
                    break;
                }
                used += ws[i];
                i += 1;
                if class(gs[i - 1]) == 0 {
                    brk = Some(i);
                }
            }
            let mut end = i;
            if i < gs.len() {
                if class(gs[i]) == 0 {
                    // The space that ends the row hangs in the column the box keeps spare.
                    end = i + 1;
                } else if let Some(b) = brk.filter(|b| *b > start) {
                    end = b;
                }
            }
            rows.push(Row {
                start: base + start,
                end: base + end,
                text: gs[start..end].concat(),
            });
            start = end;
            if start >= gs.len() {
                break;
            }
        }
        base += gs.len() + 1;
    }
    rows
}

/// Rows the box needs for `text` at `width` columns, 1 to `MAX_ROWS`. Used by the layout.
pub fn rows_needed(text: &str, width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    wrap_rows(text, width.saturating_sub(1).max(1))
        .len()
        .clamp(1, MAX_ROWS)
}

/// First row to draw so the cursor row is visible in a box `visible` rows tall.
pub fn top_row(cursor_row: usize, visible: usize) -> usize {
    (cursor_row + 1).saturating_sub(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::sw;

    fn c(text: &str) -> Composer {
        let mut c = Composer::default();
        c.set(text);
        c
    }

    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    const FLAG: &str = "\u{1F1FA}\u{1F1F3}";
    const THUMB: &str = "\u{1F44D}\u{1F3FD}";

    #[test]
    fn inserts_at_the_cursor_for_ascii_accents_and_emoji() {
        let mut d = c("helo");
        d.left();
        d.insert_char('l');
        assert_eq!((d.text(), d.cursor()), ("hello", 4));

        let mut d = c("cafe");
        d.insert_char('\u{301}');
        // The accent joined the e: still 4 clusters, cursor after them.
        assert_eq!((d.text(), d.cursor()), ("cafe\u{301}", 4));

        let mut d = c("ab");
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
            let mut d = c(&format!("hi{emoji}"));
            d.delete_back();
            assert_eq!(d.text(), "hi", "{emoji:?}");
            assert_eq!(d.cursor(), 2);
        }
        let mut d = c(&format!("{FLAG}x"));
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
        let mut d = Composer::default();
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
        let mut d = c(&format!("a{FAMILY}b"));
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
        let mut d = c("one two,  three");
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

        let mut d = c("say hello world");
        d.delete_word_back();
        assert_eq!((d.text(), d.cursor()), ("say hello ", 10));
        d.delete_word_back();
        assert_eq!(d.text(), "say ");
    }

    #[test]
    fn home_end_and_clear_work_on_the_current_line() {
        let mut d = c("first\nsecond line\nthird");
        d.left();
        d.left();
        d.left();
        d.left();
        d.left();
        d.left();
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
        let mut d = c("hi :thu there");
        for _ in 0..6 {
            d.left();
        }
        assert_eq!(d.before_cursor(), "hi :thu");
        d.replace_before_cursor(4, "\u{1F44D}");
        assert_eq!(d.text(), "hi \u{1F44D} there");
        assert_eq!(d.cursor(), 4);
    }

    #[test]
    fn wrap_rows_break_after_spaces_and_never_cut_clusters() {
        let rows = wrap_rows("hello brave new world", 11);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["hello brave ", "new world"]);
        assert_eq!((rows[1].start, rows[1].end), (12, 21));

        // A word wider than the row is hard-split between clusters.
        let rows = wrap_rows(&FAMILY.repeat(4), 5);
        assert!(rows.iter().all(|r| sw(&r.text) <= 5), "{rows:?}");
        let joined: String = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, FAMILY.repeat(4));

        // Blank lines keep a row each; graphemes index across the newline.
        let rows = wrap_rows("a\n\nb", 10);
        assert_eq!(
            rows.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(),
            [(0, 1), (2, 2), (3, 4)]
        );
        assert_eq!(wrap_rows("", 10).len(), 1);
    }

    #[test]
    fn cursor_column_counts_wide_characters() {
        let d = c("\u{4f60}\u{597d}a");
        let (rows, row, col) = d.layout(20);
        assert_eq!((rows.len(), row, col), (1, 0, 5));
        let d = c(&format!("{FAMILY}{FLAG}"));
        assert_eq!(d.layout(20).2, 4);
        // Cursor in the middle of CJK text: 1 char = 2 columns.
        let mut d = c("\u{4f60}\u{597d}");
        d.left();
        assert_eq!(d.layout(20).2, 2);
    }

    #[test]
    fn cursor_row_follows_wrapping_and_newlines() {
        let d = c("abc def ghi jkl");
        // Width 9 leaves 8 usable columns: "abc def " / "ghi jkl".
        let (rows, row, col) = d.layout(9);
        assert_eq!(rows.len(), 2);
        assert_eq!((row, col), (1, 7));
        let d = c("one\n");
        let (rows, row, col) = d.layout(20);
        assert_eq!(
            (rows.len(), row, col),
            (2, 1, 0),
            "cursor on the new empty line"
        );
        // A cursor at the end of a full row has a cell of its own.
        let d = c("12345678");
        let (_, row, col) = d.layout(9);
        assert!(col < 9, "row {row} col {col}");
    }

    #[test]
    fn rows_needed_is_capped() {
        assert_eq!(rows_needed("", 40), 1);
        assert_eq!(rows_needed("one\ntwo\nthree", 40), 3);
        assert_eq!(rows_needed(&"x\n".repeat(30), 40), MAX_ROWS);
        assert_eq!(top_row(0, MAX_ROWS), 0);
        assert_eq!(top_row(5, MAX_ROWS), 0);
        assert_eq!(top_row(6, MAX_ROWS), 1);
        assert_eq!(top_row(20, MAX_ROWS), 15);
        assert_eq!(
            top_row(3, 2),
            2,
            "a box squeezed to 2 rows still shows the cursor row"
        );
    }

    #[test]
    fn click_counts_display_width_and_stays_on_the_clicked_row() {
        // `a`, a wide emoji (2 cells), `b`: cells 0 | 1 2 | 3.
        let mut x = c("a\u{1F600}b");
        for (col, want) in [(0, 0), (1, 1), (2, 1), (3, 2), (9, 3)] {
            x.click(20, 0, col);
            assert_eq!(x.cursor(), want, "col {col}");
        }
        // Wrapped rows: a click past the end of a wrapped row stays on that row.
        let mut x = c("aaaa bbbb cccc");
        let (rows, _, _) = x.layout(6);
        assert!(rows.len() >= 3);
        x.click(6, 0, 30);
        assert_eq!(x.layout(6).1, 0);
        x.click(6, 1, 0);
        assert_eq!(x.cursor(), rows[1].start);
        // A row below the last one is the last.
        x.click(6, 99, 0);
        assert_eq!(x.cursor(), rows[rows.len() - 1].start);
    }
}
