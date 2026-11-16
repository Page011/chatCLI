//! The message draft: a `TextInput` (text, cursor and the editing keys) plus the `@mentions`
//! chosen into it, and how it wraps in the message box. Pure and tested without a terminal.

use std::ops::{Deref, DerefMut};

use chatcli_core::model::PersonRef;
use unicode_segmentation::UnicodeSegmentation;

use crate::input::{class, TextInput};
use crate::text::{cluster_width, graphemes};

/// Rows the message box grows to before it scrolls.
pub const MAX_ROWS: usize = 6;

/// A draft being typed. Edits through its `TextInput`, which it derefs to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    input: TextInput,
    /// People chosen from the `@` list, one entry per token inserted. Each stands for an
    /// `@name` in the text; a token the user later edited away is dropped at send time.
    pub mentions: Vec<PersonRef>,
}

impl Deref for Composer {
    type Target = TextInput;

    fn deref(&self) -> &TextInput {
        &self.input
    }
}

impl DerefMut for Composer {
    fn deref_mut(&mut self) -> &mut TextInput {
        &mut self.input
    }
}

/// One screen row of a wrapped draft. `start..end` are grapheme indexes into the draft (the
/// newline that ended a line is not part of any row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Composer {
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
        let at = self.cursor();
        self.input.remove(at - n, at);
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

    /// The draft wrapped to `width` columns, and the cursor's row and column. One column is kept
    /// free so a cursor at the end of a full row still has a cell.
    pub fn layout(&self, width: usize) -> (Vec<Row>, usize, usize) {
        let rows = wrap_rows(self.text(), width.saturating_sub(1).max(1));
        let cursor = self.cursor();
        let row = rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0);
        let r = &rows[row];
        let upto = cursor.saturating_sub(r.start);
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
}
