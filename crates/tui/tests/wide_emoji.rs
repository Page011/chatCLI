//! Emoji ending in VS16 (U+FE0F, e.g. ♻️ ❤️) must not shift the rest of their row.
//!
//! ratatui-core 0.1.2 repaints the column after such an emoji right after printing it, and its
//! crossterm backend skipped the cursor move for that write, so on a terminal that draws the emoji
//! two columns wide (Ghostty, kitty, WezTerm...) every later cell on the row landed one column to
//! the right and the columns it stepped over kept the previous chat's characters (ratatui#2651).
//! The workspace pins ratatui to a git rev with the fix (`[patch.crates-io]` in Cargo.toml); this
//! test fails if that pin is dropped before a release carries the fix.
//!
//! The real `CrosstermBackend` writes into a byte buffer, a tiny terminal model replays the bytes
//! (cursor moves, printing; a VS16 emoji takes two columns) and the screen it ends with must
//! match the frame ratatui thinks it drew.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const W: u16 = 12;
const H: u16 = 2;

/// What the backend writes, kept where the test can take it after each frame (the backend's own
/// `writer_mut` needs an unstable ratatui feature).
#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A screen that understands just what the backend sends while drawing: `CSI row;col H` moves
/// the cursor, other escape sequences are ignored, and printed text fills cells by its width.
struct Screen {
    cells: Vec<Vec<String>>,
    x: usize,
    y: usize,
}

impl Screen {
    fn new() -> Self {
        Screen {
            cells: vec![vec![" ".to_string(); W as usize]; H as usize],
            x: 0,
            y: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let s = std::str::from_utf8(bytes).expect("backend output is UTF-8");
        let mut rest = s;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix("\x1b[") {
                let end = after
                    .find(|c: char| ('\x40'..='\x7e').contains(&c))
                    .expect("complete CSI sequence");
                let (params, fin) = (&after[..end], &after[end..=end]);
                if fin == "H" {
                    let mut it = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1));
                    self.y = it.next().unwrap_or(1) - 1;
                    self.x = it.next().unwrap_or(1) - 1;
                }
                rest = &after[end + 1..];
            } else if let Some(after) = rest.strip_prefix('\x1b') {
                // Two-byte escapes (save/restore cursor and the like) don't print anything.
                rest = &after[after.chars().next().map_or(0, char::len_utf8)..];
            } else {
                let end = rest.find('\x1b').unwrap_or(rest.len());
                for g in rest[..end].graphemes(true) {
                    self.print(g);
                }
                rest = &rest[end..];
            }
        }
    }

    fn print(&mut self, g: &str) {
        let w = g.width().max(1);
        if self.y >= H as usize || self.x + w > W as usize {
            return;
        }
        let row = &mut self.cells[self.y];
        // Like a real terminal, writing over either half of a wide glyph blanks its other half.
        // (A hidden half is the empty string.)
        if row[self.x].is_empty() && self.x > 0 {
            row[self.x - 1] = " ".to_string();
        }
        let last = self.x + w - 1;
        if last + 1 < W as usize && row[last + 1].is_empty() {
            row[last + 1] = " ".to_string();
        }
        row[self.x] = g.to_string();
        if w == 2 {
            row[self.x + 1] = String::new();
        }
        self.x += w;
    }

    fn row(&self, y: usize) -> String {
        self.cells[y].concat()
    }
}

/// Draws each frame (a line per row, optionally reversed like a selected message) and checks the
/// replayed screen against what ratatui drew after every one.
fn check(frames: &[(&[&str], bool)]) {
    let area = Rect::new(0, 0, W, H);
    let out = Out::default();
    let mut term = Terminal::with_options(
        CrosstermBackend::new(out.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )
    .expect("terminal");
    let mut screen = Screen::new();
    for (n, (rows, reversed)) in frames.iter().enumerate() {
        let style = if *reversed {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        let done = term
            .draw(|f| {
                for (y, text) in rows.iter().enumerate() {
                    let r = Rect::new(0, y as u16, W, 1);
                    f.render_widget(Line::styled(*text, style), r);
                }
            })
            .expect("draw");
        let buf = done.buffer.clone();
        let written = std::mem::take(&mut *out.0.borrow_mut());
        screen.feed(&written);
        for y in 0..H as usize {
            // The buffer as the screen should show it: a wide glyph hides the cell after it.
            let mut want = String::new();
            let mut x = 0;
            while x < W {
                let sym = buf[(x, y as u16)].symbol();
                want.push_str(sym);
                x += sym.width().max(1) as u16;
            }
            assert_eq!(screen.row(y), want, "frame {n}, row {y}: {rows:?}");
        }
    }
}

#[test]
fn vs16_reaction_replacing_text_keeps_the_row_in_place() {
    // Switching chats: a ♻️ reaction row lands where the last chat had text, then is selected
    // (style-only repaint), then the next chat puts text back. Nothing may drift or stick.
    check(&[
        (&["last chat ok", "hehe"], false),
        (&["\u{267B}\u{FE0F} 1", "hehe"], false),
        (&["\u{267B}\u{FE0F} 1", "hehe"], true),
        (&["jk", "hehe"], false),
    ]);
}

#[test]
fn every_vs16_emoji_in_a_row_keeps_the_row_in_place() {
    check(&[
        (&["abcdefghijkl", ""], false),
        (&["\u{2764}\u{FE0F} 2  \u{267B}\u{FE0F} 1", ""], false),
        (&["x\u{2764}\u{FE0F}y\u{267B}\u{FE0F}z", ""], false),
        (&["abcdefghijkl", ""], true),
    ]);
}
