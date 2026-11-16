//! Typing indicators (issue 52, WhatsApp only): who is typing in a chat now, and when to tell the
//! network that you are. Pure: the clock is a parameter and nothing here owns a timer. The
//! incoming indicator expires on the TUI's existing 1 s tick.

/// How long "is typing" stays after the last sign of it.
pub const SHOW_MS: i64 = 10_000;
/// At most one outgoing "composing" per chat in this time.
pub const SEND_EVERY_MS: i64 = 5_000;

/// The people typing in one chat, each with the time their indicator ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Typers {
    entries: Vec<(String, i64)>,
}

impl Typers {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `sender` is typing as of `now_ms`: shown for `SHOW_MS` from now.
    pub fn start(&mut self, sender: &str, now_ms: i64) {
        let until = now_ms + SHOW_MS;
        match self.entries.iter_mut().find(|e| e.0 == sender) {
            Some(e) => e.1 = until,
            None => self.entries.push((sender.to_string(), until)),
        }
    }

    /// `sender` paused, or sent a message.
    pub fn stop(&mut self, sender: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.0 != sender);
        self.entries.len() != before
    }

    /// Drops what has expired. True when something went (the screen needs a redraw).
    pub fn prune(&mut self, now_ms: i64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.1 > now_ms);
        self.entries.len() != before
    }

    /// `Alex is typing...`, `Alex and Sam are typing...`, `Several people are typing...`.
    pub fn text(&self) -> Option<String> {
        match self.entries.as_slice() {
            [] => None,
            [(a, _)] => Some(format!("{a} is typing...")),
            [(a, _), (b, _)] => Some(format!("{a} and {b} are typing...")),
            _ => Some("Several people are typing...".to_string()),
        }
    }
}

/// May the TUI send "composing" now? `last` is when it last did for this chat.
pub fn may_send_composing(last: Option<i64>, now_ms: i64) -> bool {
    last.is_none_or(|t| now_ms - t >= SEND_EVERY_MS || now_ms < t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_text_names_the_sender() {
        let mut t = Typers::default();
        assert_eq!(t.text(), None);
        t.start("Riley Sample", 0);
        assert_eq!(t.text().as_deref(), Some("Riley Sample is typing..."));
        t.start("Sam Sample", 0);
        assert_eq!(
            t.text().as_deref(),
            Some("Riley Sample and Sam Sample are typing...")
        );
        t.start("Alex Example", 0);
        assert_eq!(t.text().as_deref(), Some("Several people are typing..."));
    }

    #[test]
    fn it_clears_on_pause_and_after_ten_seconds() {
        let mut t = Typers::default();
        t.start("a", 1_000);
        assert!(!t.prune(1_000 + SHOW_MS - 1), "still showing");
        assert!(t.prune(1_000 + SHOW_MS), "gone at ten seconds");
        assert!(t.is_empty());
        t.start("a", 0);
        assert!(t.stop("a"));
        assert!(!t.stop("a"), "nothing left to clear");
    }

    #[test]
    fn a_new_sign_of_typing_restarts_the_ten_seconds() {
        let mut t = Typers::default();
        t.start("a", 0);
        t.start("a", 8_000);
        assert!(!t.prune(12_000));
        assert!(t.prune(18_000));
    }

    #[test]
    fn composing_is_sent_at_most_once_per_five_seconds() {
        assert!(may_send_composing(None, 100));
        assert!(!may_send_composing(Some(100), 100 + SEND_EVERY_MS - 1));
        assert!(may_send_composing(Some(100), 100 + SEND_EVERY_MS));
        // A clock that went backwards must not block sending for ever.
        assert!(may_send_composing(Some(10_000), 5_000));
    }
}
