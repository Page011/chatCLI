//! Receipts on your own messages: how far the other side has received and read them (#71).
//!
//! Each `read` or delivery receipt names the reader and some of your message ids. The newest of
//! those messages becomes the reader's mark in the store (`read_marks`, `delivered_marks`, only
//! ever moving forward). A read counts as delivered too. A direct chat is read (or delivered)
//! up to its newest mark. A group is up to the oldest mark among its members, and not at all
//! while any member has none, as WhatsApp's ticks work. The TUI gets `Event::Receipts`, ticks
//! the newest read message and crosses every sent message newer than `delivered`.
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

use chatcli_core::store::BASELINE_READER;
use whatsapp_rust::wacore::types::events::Receipt;
use whatsapp_rust::wacore::types::presence::ReceiptType;

use super::ops::{wa_log, within};
use super::*;

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
        let newest = r
            .message_ids
            .iter()
            .filter_map(|id| self.store.message(NET, &chat_id, id).ok().flatten())
            .filter(|m| m.from_me)
            .map(|m| m.ts)
            .max();
        // A receipt for a message we never stored (older than our history) says nothing usable.
        let Some(ts) = newest else {
            return;
        };
        let started = self.start_delivery_tracking(&chat_id, now_ms());
        let moved = if read {
            self.store
                .advance_read_mark(NET, &chat_id, &reader.user, ts)
        } else {
            self.store
                .advance_delivered_mark(NET, &chat_id, &reader.user, ts)
        };
        match moved {
            Ok(moved) if moved || started => self.emit_receipts(&chat_id),
            Ok(_) => {}
            Err(e) => self.store_failed("save receipt", &e),
        }
    }

    /// One of our messages at `ts` was stored (sent from here, or from the phone): delivery
    /// tracking starts in its chat if it had not, with everything before it counted delivered.
    pub(super) fn own_message_stored(&mut self, chat_id: &str, ts: i64) {
        if self.start_delivery_tracking(chat_id, ts - 1) {
            self.emit_receipts(chat_id);
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
        let names = self.names.clone();
        let chat_id = chat_id.to_string();
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.members(jid))
                .await
                .map(|m| ops::member_people(m, &names));
            Done::ReadMembers { chat_id, result }
        });
    }

    pub(super) fn on_read_members(
        &mut self,
        chat_id: String,
        result: Result<Vec<PersonRef>, String>,
    ) {
        match result {
            Ok(members) => {
                if let Err(e) = self.store.replace_members(NET, &chat_id, &members) {
                    self.store_failed("save members", &e);
                    return;
                }
                self.emit_receipts(&chat_id);
            }
            Err(e) => log::error(AREA, &format!("group members for receipts: {}", wa_log(&e))),
        }
    }
}
