//! Receipts on your own messages: how far the other side has received and read them (#71).
//!
//! Each `read` or delivery receipt names the reader and some of your message ids. The newest of
//! those messages becomes the reader's mark in the store (`read_marks`, `delivered_marks`, only
//! ever moving forward). A read counts as delivered too. A direct chat is read (or delivered)
//! up to its newest mark. A group is up to the oldest mark among its members, and not at all
//! while any member has none, as WhatsApp's ticks work. The TUI gets `Event::Receipts` and marks
//! the newest read message, and each later one as delivered or not yet.
//!
//! A voice note is read like any message when the chat is seen; `played` receipts are ignored,
//! because WhatsApp sends them even for someone who turned read receipts off.
//!
//! Messages from before delivery was tracked have no receipts and must not show as
//! undelivered. So each chat gets a baseline the first time it matters (your first message in
//! it, or its first receipt): your messages up to then count as delivered.
//!
//! A reader is stored by user part (phone number or LID, whichever the receipt named) and
//! matched to a member by either address, through the phone-number/LID links in `names`.
//!
//! The library decrypts messages in a per-chat queue but hands receipts over at once, so on
//! a reconnect a receipt can name one of our messages before that message is stored (#83).
//! Such a receipt is parked by message id and applied when the message is stored, wherever
//! it comes from (the offline queue, a send confirmation, a history chunk).

use chatcli_core::store::BASELINE_READER;
use whatsapp_rust::wacore::types::events::Receipt;
use whatsapp_rust::wacore::types::presence::ReceiptType;

use super::ops::{wa_log, within};
use super::*;

/// Most message ids kept waiting for their message; past it the oldest is forgotten. A
/// receipt for a message that never arrives (older than our history) would otherwise stay.
const PARKED_MAX: usize = 2000;

/// One receipt waiting for its message: who, in which chat (as the receipt named it), and
/// whether it was a read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Waiting {
    chat_id: String,
    reader: String,
    read: bool,
}

/// Receipts waiting for their message, by message id, oldest id first.
#[derive(Default)]
pub(crate) struct Parked {
    by_id: HashMap<String, Vec<Waiting>>,
    order: VecDeque<String>,
}

impl Parked {
    fn park(&mut self, id: &str, w: Waiting) {
        if let Some(list) = self.by_id.get_mut(id) {
            if !list.contains(&w) {
                list.push(w);
            }
            return;
        }
        if self.order.len() >= PARKED_MAX {
            if let Some(old) = self.order.pop_front() {
                self.by_id.remove(&old);
            }
        }
        self.by_id.insert(id.to_string(), vec![w]);
        self.order.push_back(id.to_string());
    }

    fn take(&mut self, id: &str) -> Vec<Waiting> {
        let Some(list) = self.by_id.remove(id) else {
            return Vec::new();
        };
        self.order.retain(|o| o != id);
        list
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

impl Backend {
    pub(super) fn on_receipt(&mut self, r: &Receipt) {
        let read = match r.r#type {
            ReceiptType::Read => true,
            ReceiptType::Delivered => false,
            _ => return,
        };
        let src = &r.source;
        // Our own other device reading their messages is `read-self`; this is a second guard.
        let reader = src.sender.to_non_ad();
        if src.is_from_me || self.names.is_own(&reader.user) {
            return;
        }
        let raw = src.chat.to_non_ad();
        if !map::is_supported_chat(&raw) {
            return;
        }
        let chat_id = self.canonical_id(&raw);
        let mut newest = None;
        for id in &r.message_ids {
            match self.store.message(NET, &chat_id, id) {
                Ok(Some(m)) if m.from_me => newest = newest.max(Some(m.ts)),
                // Their message: a receipt from someone else about it says nothing.
                Ok(Some(_)) => {}
                // Not stored yet: still being decrypted, or a message older than our history
                // (that one never comes and is forgotten in time).
                Ok(None) => self.parked_receipts.park(
                    id,
                    Waiting {
                        chat_id: chat_id.clone(),
                        reader: reader.user.to_string(),
                        read,
                    },
                ),
                Err(e) => self.store_failed("read message", &e),
            }
        }
        let Some(ts) = newest else {
            return;
        };
        let started = self.start_delivery_tracking(&chat_id, now_ms());
        if self.advance_mark(&chat_id, &reader.user, read, ts) || started {
            self.emit_receipts(&chat_id);
        }
    }

    /// Moves the reader's read (or delivered) mark forward to `ts`. True when it moved.
    fn advance_mark(&self, chat_id: &str, reader: &str, read: bool, ts: i64) -> bool {
        let moved = if read {
            self.store.advance_read_mark(NET, chat_id, reader, ts)
        } else {
            self.store.advance_delivered_mark(NET, chat_id, reader, ts)
        };
        moved.unwrap_or_else(|e| {
            self.store_failed("save receipt", &e);
            false
        })
    }

    /// One of our messages, `id` at `ts`, was stored (sent from here, or from the phone):
    /// delivery tracking starts in its chat if it had not, with everything before it counted
    /// delivered, and receipts that arrived for it before it did are applied now.
    pub(super) fn own_message_stored(&mut self, chat_id: &str, id: &str, ts: i64) {
        let started = self.start_delivery_tracking(chat_id, ts - 1);
        let moved = self.apply_parked(chat_id, id, ts);
        if started || moved {
            self.emit_receipts(chat_id);
        }
    }

    /// Applies the receipts parked for our message `id` at `ts`, now stored in `chat_id` (the
    /// stored chat wins over the one the receipt named: a LID chat may have merged into its
    /// phone-number twin since). True when a mark moved.
    fn apply_parked(&mut self, chat_id: &str, id: &str, ts: i64) -> bool {
        let mut moved = false;
        for w in self.parked_receipts.take(id) {
            moved |= self.advance_mark(chat_id, &w.reader, w.read, ts);
        }
        moved
    }

    /// A history chunk was written: parked receipts whose message it carried are applied.
    pub(super) fn apply_parked_from_history(&mut self) {
        if self.parked_receipts.is_empty() {
            return;
        }
        let waiting: Vec<(String, String)> = self
            .parked_receipts
            .by_id
            .iter()
            .filter_map(|(id, ws)| ws.first().map(|w| (id.clone(), w.chat_id.clone())))
            .collect();
        let mut chats = HashSet::new();
        for (id, receipt_chat) in waiting {
            // The chunk may have linked a LID chat to its phone number since the receipt came.
            let chat_id = match receipt_chat.parse::<Jid>() {
                Ok(jid) => self.canonical_id(&jid),
                Err(_) => receipt_chat,
            };
            match self.store.message(NET, &chat_id, &id) {
                Ok(Some(m)) if m.from_me => {
                    let started = self.start_delivery_tracking(&m.chat_id, now_ms());
                    if self.apply_parked(&m.chat_id, &id, m.ts) || started {
                        chats.insert(m.chat_id);
                    }
                }
                // Someone else's message after all: nothing to apply, ever.
                Ok(Some(_)) => {
                    self.parked_receipts.take(&id);
                }
                Ok(None) => {}
                Err(e) => self.store_failed("read message", &e),
            }
        }
        for chat_id in chats {
            self.emit_receipts(&chat_id);
        }
    }

    /// Sets the chat's delivery baseline to `before` when it has none. True when set now.
    fn start_delivery_tracking(&self, chat_id: &str, before: i64) -> bool {
        self.store
            .set_delivery_baseline(NET, chat_id, before)
            .unwrap_or_else(|e| {
                self.store_failed("save delivery baseline", &e);
                false
            })
    }

    /// Tells the TUI how far the chat is read and delivered, when either is known.
    pub(super) fn emit_receipts(&mut self, chat_id: &str) {
        let (read, delivered) = self.receipts(chat_id);
        if read.is_some() || delivered.is_some() {
            let since = self
                .store
                .delivered_marks(NET, chat_id)
                .unwrap_or_default()
                .into_iter()
                .find(|(reader, _)| reader == BASELINE_READER)
                .map(|(_, ts)| ts);
            self.emit(Out::Receipts {
                network: NET,
                chat_id: chat_id.to_string(),
                read,
                delivered,
                since,
            });
        }
    }

    /// (read, delivered): the time of your newest message everyone else in the chat has read,
    /// and has received. In a group the member list is fetched once a run (in the background,
    /// answered by `Done::ReadMembers`), so someone who joined or left since it was saved counts
    /// correctly from then on.
    fn receipts(&mut self, chat_id: &str) -> (Option<i64>, Option<i64>) {
        let marks = self
            .store
            .read_marks(NET, chat_id)
            .and_then(|r| Ok((r, self.store.delivered_marks(NET, chat_id)?)));
        let (read, delivered) = match marks {
            Ok(m) => m,
            Err(e) => {
                self.store_failed("read receipt marks", &e);
                return (None, None);
            }
        };
        if read.is_empty() && delivered.is_empty() {
            return (None, None);
        }
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return (None, None);
        };
        // Your chat with yourself: no one else sends receipts there, so every message would
        // look undelivered for ever.
        if !jid.is_group() && self.names.is_own(&jid.user) {
            return (None, None);
        }
        let mark = |marks: &[(String, i64)], user: &str| {
            marks
                .iter()
                .find(|(reader, _)| reader == user)
                .map(|(_, ts)| *ts)
        };
        let baseline = mark(&delivered, BASELINE_READER);
        if !jid.is_group() {
            let r = read.iter().map(|(_, ts)| *ts).max();
            let d = delivered.iter().map(|(_, ts)| *ts).max();
            return (r, d.max(r));
        }
        if self.read_members_asked.insert(chat_id.to_string()) {
            self.fetch_read_members(chat_id, jid);
        }
        let members = self.store.members(NET, chat_id, 1024).unwrap_or_default();
        if members.is_empty() {
            return (None, None);
        }
        // Each member's marks under either of their addresses.
        let per_member: Vec<(Option<i64>, Option<i64>)> = members
            .iter()
            .map(|p| {
                let user = p.id.split('@').next().unwrap_or_default();
                let alt = self.names.linked(user);
                let r = mark(&read, user).max(alt.and_then(|a| mark(&read, a)));
                let d = mark(&delivered, user).max(alt.and_then(|a| mark(&delivered, a)));
                (r, d.max(r).max(baseline))
            })
            .collect();
        // Option orders None first, so one member without a mark makes the minimum None.
        let r = per_member.iter().map(|(r, _)| *r).min().flatten();
        let d = per_member.iter().map(|(_, d)| *d).min().flatten();
        (r, d)
    }

    /// Reads the group's members for receipts. Unlike `LoadMembers` nothing goes to the TUI:
    /// a failure is only logged, and the saved list (if any) keeps answering.
    fn fetch_read_members(&mut self, chat_id: &str, jid: Jid) {
        let Some(wa) = self.wa() else {
            // Offline: try again on the next receipt.
            self.read_members_asked.remove(chat_id);
            return;
        };
        let (gen, chat_id) = (self.gen, chat_id.to_string());
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.members(jid)).await;
            Done::ReadMembers {
                gen,
                chat_id,
                result,
            }
        });
    }

    pub(super) fn on_read_members(
        &mut self,
        gen: u64,
        chat_id: String,
        result: Result<Vec<crate::wa::Member>, String>,
    ) {
        // From a session that has since stopped (sign-out, another account): not ours to save.
        // The group may be asked again in this one.
        if gen != self.gen {
            self.read_members_asked.remove(&chat_id);
            return;
        }
        match result {
            Ok(members) => {
                // A member the group names by LID may read by phone number: learn the pair, or
                // their receipts would never match them.
                let mut learned = false;
                for m in &members {
                    learned |= self.link_jids(&m.jid, m.pn.as_ref());
                }
                if learned {
                    self.schedule_retitle();
                }
                let people = ops::member_people(members, &self.names);
                if let Err(e) = self.store.replace_members(NET, &chat_id, &people) {
                    self.store_failed("save members", &e);
                    return;
                }
                self.emit_receipts(&chat_id);
            }
            Err(e) => {
                // Asked again on the next receipt or open, not never.
                self.read_members_asked.remove(&chat_id);
                log::error(AREA, &format!("group members for receipts: {}", wa_log(&e)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(reader: &str) -> Waiting {
        Waiting {
            chat_id: "c".into(),
            reader: reader.into(),
            read: true,
        }
    }

    #[test]
    fn parked_receipts_are_capped_oldest_first_and_kept_once() {
        let mut p = Parked::default();
        p.park("a", w("x"));
        p.park("a", w("x"));
        p.park("a", w("y"));
        assert_eq!(p.take("a"), [w("x"), w("y")]);
        assert!(p.take("a").is_empty());
        for i in 0..=PARKED_MAX {
            p.park(&i.to_string(), w("x"));
        }
        assert_eq!(p.by_id.len(), PARKED_MAX);
        assert!(p.take("0").is_empty(), "the oldest is forgotten");
        assert_eq!(p.take(&PARKED_MAX.to_string()), [w("x")]);
    }
}
