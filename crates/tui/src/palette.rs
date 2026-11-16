//! The `/` command bar: one box that lists commands and, once something is typed, the chats
//! of the active network that match. Pure state; `App` fills the items, `ui` draws them.

use crate::commands::CmdId;
use crate::input::TextInput;

#[derive(Debug, Clone, Default)]
pub(crate) struct Palette {
    pub query: TextInput,
    /// Chosen item.
    pub sel: usize,
    /// First display row shown (headings count as rows).
    pub top: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PalItem {
    Cmd(CmdId),
    /// A chat id of the active network.
    Chat(String),
}

/// One drawn row of the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PalRow {
    Heading(&'static str),
    Item(usize),
}

/// The rows to draw: a heading above each section that has items.
pub(crate) fn rows(items: &[PalItem]) -> Vec<PalRow> {
    let mut out = Vec::new();
    let mut section = None;
    for (i, it) in items.iter().enumerate() {
        let head = match it {
            PalItem::Cmd(_) => "Commands",
            PalItem::Chat(_) => "Chats",
        };
        if section != Some(head) {
            out.push(PalRow::Heading(head));
            section = Some(head);
        }
        out.push(PalRow::Item(i));
    }
    out
}

impl Palette {
    /// Moves the selection, wrapping at both ends.
    pub fn step(&mut self, len: usize, delta: isize) {
        if len > 0 {
            self.sel = (self.sel as isize + delta).rem_euclid(len as isize) as usize;
        }
    }

    /// Page keys: clamped, not wrapped.
    pub fn page(&mut self, len: usize, delta: isize) {
        if len > 0 {
            self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
        }
    }

    /// Scrolls so the selected item's row is inside a window of `rows_shown` rows.
    pub fn keep_visible(&mut self, items: &[PalItem], rows_shown: usize) {
        let shown = rows_shown.max(1);
        let at = rows(items)
            .iter()
            .position(|r| *r == PalRow::Item(self.sel))
            .unwrap_or(0);
        if self.sel == 0 {
            // The first item: show its heading too.
            self.top = 0;
        } else if at < self.top {
            self.top = at;
        } else if at >= self.top + shown {
            self.top = at + 1 - shown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_sit_above_each_section() {
        let items = [
            PalItem::Cmd(CmdId::Help),
            PalItem::Cmd(CmdId::Keys),
            PalItem::Chat("a".into()),
        ];
        assert_eq!(
            rows(&items),
            [
                PalRow::Heading("Commands"),
                PalRow::Item(0),
                PalRow::Item(1),
                PalRow::Heading("Chats"),
                PalRow::Item(2)
            ]
        );
        assert!(rows(&[]).is_empty());
    }

    #[test]
    fn step_wraps_and_page_clamps() {
        let mut p = Palette::default();
        p.step(3, -1);
        assert_eq!(p.sel, 2);
        p.step(3, 1);
        assert_eq!(p.sel, 0);
        p.page(3, 10);
        assert_eq!(p.sel, 2);
        p.page(3, -10);
        assert_eq!(p.sel, 0);
        p.step(0, 1);
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn the_window_follows_the_selection() {
        let items: Vec<PalItem> = (0..10).map(|_| PalItem::Cmd(CmdId::Help)).collect();
        let mut p = Palette {
            sel: 9,
            ..Palette::default()
        };
        p.keep_visible(&items, 4);
        // Item 9 is row 10 (heading is row 0): the window ends on it.
        assert_eq!(p.top, 7);
        p.sel = 0;
        p.keep_visible(&items, 4);
        assert_eq!(p.top, 0);
    }
}
