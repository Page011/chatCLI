//! Loading older messages when the user scrolls past the top of a chat (issues 29 and 30).
//!
//! Only a user action asks for a page, one request at a time, and never again for a chat whose
//! start was reached. The pure rules live here; `app/everyday.rs` wires them to keys and events.

use chatcli_core::model::{ChatMessage, MessageState};

/// Where one chat is in loading its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Older {
    /// Nothing asked, or the last answer had more to give.
    #[default]
    Idle,
    /// A request is out: a second scroll past the top sends nothing.
    Loading,
    /// The backend said nothing older exists: never asked again this session.
    AtStart,
}

impl Older {
    /// May a scroll past the top send a request now?
    pub fn may_ask(self) -> bool {
        self == Older::Idle
    }
}

/// The (ts, id) the backend should load before: the oldest message that really exists on the
/// network. A local send that is still pending or failed has no network id yet.
pub fn request_point(msgs: &[ChatMessage]) -> Option<(i64, String)> {
    msgs.iter()
        .filter(|m| {
            !(m.from_me && matches!(m.state, MessageState::Pending | MessageState::Failed(_)))
        })
        .min_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)))
        .map(|m| (m.ts, m.id.clone()))
}

/// Puts an older batch in front of what is held. Messages already held win (they may have
/// newer reactions or edits), so a page that overlaps what the TUI has adds nothing twice.
/// Returns the merged list, oldest first, and how many messages were new.
pub fn merge_older(held: Vec<ChatMessage>, batch: Vec<ChatMessage>) -> (Vec<ChatMessage>, usize) {
    let have: std::collections::HashSet<&str> = held.iter().map(|m| m.id.as_str()).collect();
    let mut fresh: Vec<ChatMessage> = Vec::new();
    for m in batch {
        if !have.contains(m.id.as_str()) && !fresh.iter().any(|f| f.id == m.id) {
            fresh.push(m);
        }
    }
    let added = fresh.len();
    fresh.extend(held);
    // Stable: equal times keep the older batch first.
    fresh.sort_by_key(|m| m.ts);
    (fresh, added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::model::Network;

    fn msg(id: &str, ts: i64) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            ts,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    #[test]
    fn only_idle_may_ask() {
        assert!(Older::Idle.may_ask());
        assert!(!Older::Loading.may_ask());
        assert!(!Older::AtStart.may_ask());
    }

    #[test]
    fn request_point_is_the_oldest_real_message() {
        assert_eq!(request_point(&[]), None);
        let mut pending = msg("local-1", 1);
        pending.from_me = true;
        pending.state = MessageState::Pending;
        let mut failed = msg("local-2", 2);
        failed.from_me = true;
        failed.state = MessageState::Failed("x".into());
        let held = vec![pending, failed, msg("b", 50), msg("a", 50), msg("c", 60)];
        assert_eq!(request_point(&held), Some((50, "a".to_string())));
        // Only local sends held: nothing to anchor a request on.
        let mut only = msg("local-3", 1);
        only.from_me = true;
        only.state = MessageState::Pending;
        assert_eq!(request_point(&[only]), None);
    }

    #[test]
    fn merging_twice_gives_no_duplicates_and_keeps_order() {
        let held = vec![msg("c", 30), msg("d", 40)];
        let page = vec![msg("a", 10), msg("b", 20), msg("c", 30)];
        let (merged, added) = merge_older(held, page.clone());
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!((ids, added), (vec!["a", "b", "c", "d"], 2));
        let (again, added) = merge_older(merged, page);
        assert_eq!((again.len(), added), (4, 0));
    }

    #[test]
    fn a_held_copy_beats_the_batch_copy() {
        let mut held = msg("a", 10);
        held.text = "edited".into();
        let (merged, _) = merge_older(vec![held], vec![msg("a", 10)]);
        assert_eq!(merged[0].text, "edited");
    }

    #[test]
    fn a_batch_with_its_own_duplicates_adds_each_once() {
        let (merged, added) = merge_older(vec![], vec![msg("a", 1), msg("a", 1), msg("b", 2)]);
        assert_eq!((merged.len(), added), (2, 2));
    }
}
