//! A small list that opens above the message box while you type and completes what you typed.
//! The emoji `:shortcode:` popup and the `@mention` popup share it, so nothing here knows about
//! either. Pure: the keys move `selected`, `ui` draws it.

use chatcli_core::model::PersonRef;

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupItem {
    /// Shown first (for emoji, the emoji itself).
    pub label: String,
    /// Shown after the label, dimmed (the shortcode or name).
    pub detail: String,
    /// What replaces the typed token when the item is chosen.
    pub insert: String,
    /// Set for a mention: the person the inserted `@name` stands for.
    pub person: Option<PersonRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub items: Vec<PopupItem>,
    pub selected: usize,
    /// The text that opened it (after the colon), so a refresh with the same text keeps the
    /// selection and a new text starts from the top.
    pub query: String,
    /// Characters (grapheme clusters) before the cursor that choosing replaces, trigger included.
    pub replace: usize,
    /// Heading of the box.
    pub title: &'static str,
}

impl Popup {
    pub fn new(items: Vec<PopupItem>, query: String, replace: usize) -> Popup {
        Popup {
            items,
            selected: 0,
            query,
            replace,
            title: "Emoji",
        }
    }

    /// The same list under another heading ("Mention").
    pub fn titled(mut self, title: &'static str) -> Popup {
        self.title = title;
        self
    }

    /// Up one item, wrapping from the first to the last.
    pub fn up(&mut self) {
        self.selected = crate::list::step(self.selected, self.items.len(), -1);
    }

    /// Down one item, wrapping from the last to the first.
    pub fn down(&mut self) {
        self.selected = crate::list::step(self.selected, self.items.len(), 1);
    }

    pub fn chosen(&self) -> Option<&PopupItem> {
        self.items.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(s: &str) -> PopupItem {
        PopupItem {
            label: s.into(),
            detail: String::new(),
            insert: s.into(),
            person: None,
        }
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut p = Popup::new(vec![item("a"), item("b"), item("c")], "q".into(), 2);
        assert_eq!(p.chosen().unwrap().insert, "a");
        p.up();
        assert_eq!(p.chosen().unwrap().insert, "c");
        p.down();
        p.down();
        assert_eq!(p.chosen().unwrap().insert, "b");
    }

    #[test]
    fn empty_popup_is_harmless() {
        let mut p = Popup::new(Vec::new(), String::new(), 0);
        p.up();
        p.down();
        assert!(p.chosen().is_none());
    }
}
