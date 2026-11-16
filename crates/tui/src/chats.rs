//! The chat list's pure rules: order (pinned first), which chat `n` jumps to, and the text marks
//! that say a chat is pinned, muted or hidden. No state, no terminal.

use chatcli_core::model::ChatSummary;

/// Pinned chats first, then newest first. Stable, so the order inside each group stays the
/// backend's (chats without a message keep their place at the end of their group).
pub fn sort_chats(chats: &mut [ChatSummary]) {
    chats.sort_by_key(|c| {
        (
            std::cmp::Reverse(c.flags.pinned),
            std::cmp::Reverse(c.last_ts),
        )
    });
}

/// Muted right now (a mute with an end time in the past is over).
pub fn is_muted(c: &ChatSummary, now_ms: i64) -> bool {
    c.flags.is_muted(now_ms)
}

/// The unread count that goes in a tab's total: muted and hidden chats are left out.
pub fn counts_toward_total(c: &ChatSummary, now_ms: i64) -> bool {
    !c.flags.hidden && !is_muted(c, now_ms)
}

/// Position of the next entry of `eligible` that is true, starting after `from` and going in
/// `dir` (1 or -1), wrapping around. The entry at `from` itself is the last one tried, so one
/// eligible chat that is already selected stays selected. `None` when nothing is eligible.
pub fn next_eligible(eligible: &[bool], from: usize, dir: isize) -> Option<usize> {
    let len = eligible.len();
    if len == 0 {
        return None;
    }
    let step = if dir < 0 { len - 1 } else { 1 };
    (1..=len)
        .map(|k| (from + k * step) % len)
        .find(|&i| eligible[i])
}

/// Text marks drawn after a chat's title: `pin`, `mute`, `hidden`. Words, never colour alone.
pub fn marks(c: &ChatSummary, now_ms: i64) -> String {
    let mut out: Vec<&str> = Vec::new();
    if c.flags.pinned {
        out.push("pin");
    }
    if is_muted(c, now_ms) {
        out.push("mute");
    }
    if c.flags.hidden {
        out.push("hidden");
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::model::{ChatFlags, Network, MUTE_FOREVER};

    fn chat(id: &str, ts: Option<i64>, pinned: bool) -> ChatSummary {
        ChatSummary {
            id: id.into(),
            last_ts: ts,
            flags: ChatFlags {
                pinned,
                ..ChatFlags::default()
            },
            ..ChatSummary::blank(Network::Teams)
        }
    }

    fn ids(chats: &[ChatSummary]) -> Vec<&str> {
        chats.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn pinned_first_and_order_inside_each_group_is_unchanged() {
        let mut chats = vec![
            chat("new", Some(900), false),
            chat("old-pin", Some(100), true),
            chat("mid", Some(500), false),
            chat("new-pin", Some(800), true),
            chat("none", None, false),
        ];
        sort_chats(&mut chats);
        assert_eq!(ids(&chats), ["new-pin", "old-pin", "new", "mid", "none"]);
        // Sorting again changes nothing.
        let before = ids(&chats).join(",");
        sort_chats(&mut chats);
        assert_eq!(ids(&chats).join(","), before);
    }

    #[test]
    fn chats_without_a_message_keep_the_backends_order() {
        let mut chats = vec![chat("b", None, false), chat("a", None, false)];
        sort_chats(&mut chats);
        assert_eq!(ids(&chats), ["b", "a"]);
    }

    #[test]
    fn next_eligible_wraps_both_ways_and_skips_the_rest() {
        let e = [false, true, false, false, true];
        assert_eq!(next_eligible(&e, 0, 1), Some(1));
        assert_eq!(next_eligible(&e, 1, 1), Some(4));
        assert_eq!(next_eligible(&e, 4, 1), Some(1), "wraps past the end");
        assert_eq!(next_eligible(&e, 1, -1), Some(4), "wraps before the start");
        assert_eq!(next_eligible(&e, 4, -1), Some(1));
        assert_eq!(next_eligible(&e, 3, -1), Some(1));
        assert_eq!(next_eligible(&e, 2, 1), Some(4));
    }

    #[test]
    fn next_eligible_with_one_or_none() {
        assert_eq!(next_eligible(&[false, true, false], 1, 1), Some(1));
        assert_eq!(next_eligible(&[false, true, false], 1, -1), Some(1));
        assert_eq!(next_eligible(&[false, false], 0, 1), None);
        assert_eq!(next_eligible(&[], 0, 1), None);
        assert_eq!(next_eligible(&[true], 0, 1), Some(0));
    }

    #[test]
    fn muted_and_hidden_chats_are_left_out_of_the_total() {
        let mut c = chat("a", Some(1), false);
        assert!(counts_toward_total(&c, 1_000));
        c.flags.muted_until = Some(MUTE_FOREVER);
        assert!(!counts_toward_total(&c, 1_000));
        c.flags.muted_until = Some(500);
        assert!(counts_toward_total(&c, 1_000), "an ended mute counts again");
        c.flags.muted_until = None;
        c.flags.hidden = true;
        assert!(!counts_toward_total(&c, 1_000));
    }

    #[test]
    fn marks_are_words() {
        let mut c = chat("a", Some(1), true);
        assert_eq!(marks(&c, 0), "pin");
        c.flags.muted_until = Some(MUTE_FOREVER);
        c.flags.hidden = true;
        assert_eq!(marks(&c, 0), "pin mute hidden");
        assert_eq!(marks(&chat("b", None, false), 0), "");
    }
}
