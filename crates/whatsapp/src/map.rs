//! Pure mapping from WhatsApp protobuf and JIDs to chatcli's model. No I/O, so all of it is
//! unit-tested. The backend (`backend.rs`) feeds it and does the storing and emitting.

use std::collections::HashMap;

use chatcli_core::model::{ChatKind, ChatMessage, ChatSummary, MessageState, Network};
use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore_binary::JidExt;

/// Label of our own messages in `ChatMessage::sender` and previews.
pub const OWN_LABEL: &str = "You";

/// Shown for a message we cannot map. Better a visible gap than a silently missing message.
const UNSUPPORTED: &str = "[unsupported message]";

const PREVIEW_CHARS: usize = 200;

// ---------------------------------------------------------------------------------------------
// Chats and names
// ---------------------------------------------------------------------------------------------

/// Status, broadcast lists and channels are out of scope (direct and group chats only).
pub fn is_supported_chat(jid: &Jid) -> bool {
    !(jid.is_status_broadcast() || jid.is_newsletter() || jid.is_broadcast_list())
}

pub fn chat_kind(jid: &Jid) -> ChatKind {
    if jid.is_group() {
        ChatKind::Group
    } else {
        ChatKind::Direct
    }
}

/// One id per conversation. WhatsApp addresses the same person as a phone-number JID or a LID;
/// the phone-number form is preferred (it is what the user knows and sends to). `alt` is the
/// other address of the same peer when the library knows it.
pub fn canonical_chat(chat: &Jid, alt: Option<&Jid>) -> Jid {
    let chat = chat.to_non_ad();
    match alt {
        Some(a) if chat.is_lid() && a.is_pn() => a.to_non_ad(),
        _ => chat,
    }
}

/// Last resort title: the phone number, or a placeholder for a hidden (LID) number.
pub fn fallback_label(jid: &Jid) -> String {
    if jid.is_pn() {
        format!("+{}", jid.user)
    } else {
        "Unknown contact".to_string()
    }
}

pub fn is_fallback_title(title: &str) -> bool {
    title == "Unknown contact"
        || (title.starts_with('+') && title[1..].chars().all(|c| c.is_ascii_digit()))
}

/// The user part of a JID string ("15550100000@s.whatsapp.net" -> "15550100000").
pub fn user_of(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split([':', '.']).next().unwrap_or(user).to_string()
}

/// Row kinds of a saved name (`Names::take_pending`, `Names::load`).
const BOOK: &str = "book";
const PUSH: &str = "push";
const LINK: &str = "link";

/// `(kind, JID user part, value)`: one saved fact about a contact.
pub type NameRow = (String, String, String);

/// Display names by JID user part: the phone's address book wins over push names (the name a
/// contact chose for themselves). The same person can be addressed by a phone number or a LID,
/// so `link` joins the two user parts and every lookup tries both. Changes are queued in
/// `pending` so the backend can save them: WhatsApp sends address-book names once, and without
/// a saved copy a restart forgets them.
#[derive(Debug, Clone, Default)]
pub struct Names {
    book: HashMap<String, String>,
    push: HashMap<String, String>,
    link: HashMap<String, String>,
    /// User parts of our own phone-number and LID addresses: who counts as "me" in mentions
    /// and quotes. Not saved; learned from the session on every start.
    own: Vec<String>,
    pending: Vec<NameRow>,
}

impl Names {
    pub fn set_book(&mut self, user: &str, name: &str) -> bool {
        set(&mut self.book, &mut self.pending, BOOK, user, name)
    }

    pub fn set_push(&mut self, user: &str, name: &str) -> bool {
        set(&mut self.push, &mut self.pending, PUSH, user, name)
    }

    pub fn set_own(&mut self, users: Vec<String>) {
        self.own = users.into_iter().filter(|u| !u.is_empty()).collect();
    }

    /// Our phone-number user part, the first of `set_own`'s list.
    pub fn own_pn(&self) -> Option<&str> {
        self.own.first().map(String::as_str)
    }

    /// True when `user` is one of our own addresses (or the other address of one of them).
    pub fn is_own(&self, user: &str) -> bool {
        self.own
            .iter()
            .any(|o| o == user || self.link.get(o.as_str()).is_some_and(|other| other == user))
    }

    /// Records that two user parts (a phone number and a LID) are the same person.
    pub fn link(&mut self, a: &str, b: &str) -> bool {
        if a.is_empty() || b.is_empty() || a == b || self.link.get(a).is_some_and(|o| o == b) {
            return false;
        }
        // One partner per address: drop the reverse entries of the old partners, or the old
        // partner keeps resolving to this person's names.
        for user in [a, b] {
            if let Some(old) = self.link.remove(user) {
                self.link.remove(&old);
            }
        }
        self.link.insert(a.to_string(), b.to_string());
        self.link.insert(b.to_string(), a.to_string());
        self.pending
            .push((LINK.into(), a.to_string(), b.to_string()));
        true
    }

    /// The other address of the same person, if known.
    pub fn linked(&self, user: &str) -> Option<&str> {
        self.link.get(user).map(String::as_str)
    }

    /// Restores saved rows without queueing them for saving again.
    pub fn load(&mut self, rows: impl IntoIterator<Item = NameRow>) {
        for (kind, user, value) in rows {
            match kind.as_str() {
                BOOK => {
                    self.book.insert(user, value);
                }
                PUSH => {
                    self.push.insert(user, value);
                }
                LINK => {
                    self.link.insert(user.clone(), value.clone());
                    self.link.insert(value, user);
                }
                _ => {}
            }
        }
    }

    /// Changes since the last call, for saving.
    pub fn take_pending(&mut self) -> Vec<NameRow> {
        std::mem::take(&mut self.pending)
    }

    fn find<'a>(&'a self, map: &'a HashMap<String, String>, user: &str) -> Option<&'a String> {
        map.get(user)
            .or_else(|| self.link.get(user).and_then(|other| map.get(other)))
    }

    /// `hint` is the push name carried by the message itself.
    pub fn label_with(&self, jid: &Jid, hint: Option<&str>) -> String {
        let user = jid.user.as_str();
        self.find(&self.book, user)
            .cloned()
            .or_else(|| hint.filter(|h| !h.is_empty()).map(str::to_string))
            .or_else(|| self.find(&self.push, user).cloned())
            .unwrap_or_else(|| fallback_label(jid))
    }

    /// A better title for a direct chat than `current`, or None to leave it alone. An address
    /// book name always wins; a push name only replaces a bare number.
    pub fn better_title(&self, jid: &Jid, current: &str) -> Option<String> {
        if jid.is_group() {
            return None;
        }
        let user = jid.user.as_str();
        if let Some(b) = self.find(&self.book, user) {
            return (b != current).then(|| b.clone());
        }
        if is_fallback_title(current) {
            return self
                .find(&self.push, user)
                .filter(|p| p.as_str() != current)
                .cloned();
        }
        None
    }
}

fn set(
    map: &mut HashMap<String, String>,
    pending: &mut Vec<NameRow>,
    kind: &str,
    user: &str,
    name: &str,
) -> bool {
    if user.is_empty() || name.is_empty() || map.get(user).is_some_and(|n| n == name) {
        return false;
    }
    map.insert(user.to_string(), name.to_string());
    pending.push((kind.to_string(), user.to_string(), name.to_string()));
    true
}

/// The user parts a contact (address book) record names, and its (phone number, LID) pair when
/// both addresses are known. `jid` is the record's own address; `lid` and `pn` are the other
/// addresses the record carries.
pub fn contact_addresses(
    jid: &Jid,
    lid: Option<&str>,
    pn: Option<&str>,
) -> (Vec<String>, Option<(String, String)>) {
    let mut users = vec![jid.user.to_string()];
    let lid = lid
        .map(user_of)
        .or_else(|| jid.is_lid().then(|| jid.user.to_string()));
    let pn = pn
        .map(user_of)
        .or_else(|| jid.is_pn().then(|| jid.user.to_string()));
    for u in lid.iter().chain(pn.iter()) {
        if !users.contains(u) {
            users.push(u.clone());
        }
    }
    let link = lid.zip(pn).filter(|(l, p)| l != p).map(|(l, p)| (p, l));
    (users, link)
}

/// One log line for "app state sync incomplete", and whether it is an error.
///
/// `skipped` is not harmless: the library files a collection there when another holder had it
/// reserved, "so this call did nothing for it", and it then leaves the collection out of its
/// retry list ("will not be retried"). `critical_unblock_low` holds the address-book contacts,
/// so a skip there means no contact names arrive on this connection. Only an all-empty
/// report is "nothing failed".
pub fn app_state_report(
    fatal: &[String],
    retryable: &[String],
    skipped: &[String],
) -> (String, bool) {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    let mut text = format!(
        "app state sync incomplete: refused by WhatsApp [{}], will retry [{}], skipped and not retried [{}]",
        list(fatal),
        list(retryable),
        list(skipped)
    );
    if skipped.iter().any(|n| n == CONTACTS_COLLECTION) {
        text.push_str(&format!(
            "; contacts not synced on this connection ({CONTACTS_COLLECTION} skipped, the library will not retry it), so names may be missing until chatcli is restarted or re-linked"
        ));
    } else if fatal.is_empty() && retryable.is_empty() && skipped.is_empty() {
        text.push_str("; nothing failed");
    }
    (
        text,
        !fatal.is_empty() || !retryable.is_empty() || !skipped.is_empty(),
    )
}

/// The app-state collection the library files address-book contact records under.
const CONTACTS_COLLECTION: &str = "critical_unblock_low";

/// The contact name that the stored messages of a direct chat already carry: the most frequent
/// incoming sender label that is a real name (not "You", a bare number or "Unknown contact").
/// Ties go to the alphabetically first so the answer is stable.
pub fn name_from_senders(counts: &[(String, u64)]) -> Option<String> {
    counts
        .iter()
        .filter(|(s, _)| !s.is_empty() && s != OWN_LABEL && !is_fallback_title(s))
        .max_by(|(a, an), (b, bn)| an.cmp(bn).then_with(|| b.cmp(a)))
        .map(|(s, _)| s.clone())
}

/// Who wrote a message, as shown in the pane and in group previews.
pub fn sender_label(from_me: bool, sender: &Jid, push_name: &str, names: &Names) -> String {
    if from_me {
        OWN_LABEL.to_string()
    } else {
        names.label_with(&sender.to_non_ad(), Some(push_name))
    }
}

/// What a deleted message shows in the sidebar; its stored text is empty (the pane draws the
/// same words itself).
pub const DELETED_PREVIEW: &str = "[message deleted]";

/// The text of a message as the sidebar preview shows it.
pub fn preview_text(m: &ChatMessage) -> &str {
    if m.state == MessageState::Deleted {
        DELETED_PREVIEW
    } else {
        &m.text
    }
}

/// A mute from the phone as the model stores it: None = not muted, `MUTE_FOREVER` = no end.
/// WhatsApp marks "for good" with an end of -1 (or none); an end already past is not a mute.
pub fn mute_until(muted: bool, end_ms: Option<i64>, now: i64) -> Option<i64> {
    if !muted {
        return None;
    }
    match end_ms {
        Some(e) if e > 0 => (e > now).then_some(e),
        _ => Some(chatcli_core::model::MUTE_FOREVER),
    }
}

/// Sidebar preview: one line, "Sender: text" where the sender matters (groups and our own
/// messages), plain text for the other side of a direct chat.
pub fn preview(sender: &str, text: &str, show_sender: bool) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().take(PREVIEW_CHARS).collect();
    if show_sender {
        format!("{sender}: {line}")
    } else {
        line
    }
}

// ---------------------------------------------------------------------------------------------
// Message content
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// The sender edited message `target`. `mentioned` is the edited text's mention list.
    Edit {
        target: String,
        text: String,
        mentioned: Vec<String>,
    },
    /// The sender deleted message `target` for everyone.
    Revoke {
        target: String,
    },
    /// The sender reacted to message `target`; an empty `emoji` removes their reaction.
    Reaction {
        target: String,
        emoji: String,
    },
    /// Protocol traffic and the like: nothing to show.
    Skip,
}

pub fn content(msg: &wa::Message) -> Content {
    let base = msg.get_base_message();
    if let Some(p) = base.protocol_message.as_option() {
        return protocol(p);
    }
    if let Some(r) = base.reaction_message.as_option() {
        // A reaction without a target id cannot be applied to anything.
        return match r.key.as_option().and_then(|k| k.id.clone()) {
            Some(target) => Content::Reaction {
                target,
                emoji: r.text.clone().unwrap_or_default(),
            },
            None => Content::Skip,
        };
    }
    if let Some(t) = base.text_content() {
        if !t.is_empty() {
            return Content::Text(t.to_string());
        }
    }
    let caption = base.get_caption();
    let text = if base.image_message.is_set() {
        with_caption("[image]", caption)
    } else if base.video_message.is_set() || base.ptv_message.is_set() {
        with_caption("[video]", caption)
    } else if let Some(a) = base.audio_message.as_option() {
        if a.ptt == Some(true) {
            "[voice message]"
        } else {
            "[audio]"
        }
        .to_string()
    } else if let Some(d) = base.document_message.as_option() {
        let marker = match d.file_name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => format!("[document: {name}]"),
            None => "[document]".to_string(),
        };
        with_caption(&marker, caption)
    } else if base.sticker_message.is_set() || base.lottie_sticker_message.is_set() {
        "[sticker]".to_string()
    } else if base.location_message.is_set() || base.live_location_message.is_set() {
        "[location]".to_string()
    } else if base.contact_message.is_set() || base.contacts_array_message.is_set() {
        "[contact]".to_string()
    } else if base.poll_creation_message.is_set()
        || base.poll_creation_message_v2.is_set()
        || base.poll_creation_message_v3.is_set()
    {
        "[poll]".to_string()
    } else if is_silent(base) {
        return Content::Skip;
    } else {
        UNSUPPORTED.to_string()
    };
    Content::Text(text)
}

pub(crate) fn with_caption(marker: &str, caption: Option<&str>) -> String {
    match caption.filter(|c| !c.is_empty()) {
        Some(c) => format!("{marker} {c}"),
        None => marker.to_string(),
    }
}

fn protocol(p: &wa::message::ProtocolMessage) -> Content {
    use wa::message::protocol_message::Type;
    let Some(target) = p.key.as_option().and_then(|k| k.id.clone()) else {
        return Content::Skip;
    };
    match p.r#type {
        Some(Type::Revoke) => Content::Revoke { target },
        Some(Type::MessageEdit) => match p.edited_message.as_option() {
            Some(edited) => match content(edited) {
                Content::Text(text) => Content::Edit {
                    target,
                    text,
                    mentioned: crate::rich::context_of(edited.get_base_message())
                        .map(crate::rich::mentioned_jids)
                        .unwrap_or_default(),
                },
                _ => Content::Skip,
            },
            None => Content::Skip,
        },
        _ => Content::Skip,
    }
}

/// True when the message carries only invisible plumbing. Compared after clearing the plumbing
/// fields, so a sender-key message that rides along with unknown real content still shows up as
/// unsupported instead of vanishing.
fn is_silent(m: &wa::Message) -> bool {
    let mut rest = m.clone();
    rest.sender_key_distribution_message = Default::default();
    rest.fast_ratchet_key_sender_key_distribution_message = Default::default();
    rest.reaction_message = Default::default();
    rest.enc_reaction_message = Default::default();
    rest.poll_update_message = Default::default();
    rest.keep_in_chat_message = Default::default();
    rest.pin_in_chat_message = Default::default();
    rest.placeholder_message = Default::default();
    rest.secret_encrypted_message = Default::default();
    rest.message_history_notice = Default::default();
    rest.album_message = Default::default();
    rest.message_context_info = Default::default();
    rest == wa::Message::default()
}

// ---------------------------------------------------------------------------------------------
// Message times
// ---------------------------------------------------------------------------------------------
//
// A message we sent from the phone is stamped by the PHONE's clock inside the history sync
// (`message_timestamp`); live messages carry the server's time instead. A phone whose clock was
// set ahead delivers own messages dated years in the future, and one such message sorts its
// chat above all real activity. WhatsApp's own clients and whatsmeow/Baileys (as far as known
// from memory, not re-checked here) store the field as given, so the guard lives here.

/// How far ahead of the local clock a message time may be and still count as real. Covers an
/// honest clock skew between this PC and WhatsApp's servers.
pub const FUTURE_SKEW_MS: i64 = 10 * 60 * 1000;

/// Nothing sent over WhatsApp is older than 2009; an earlier time is garbage, not history.
pub const EARLIEST_SANE_MS: i64 = 1_230_768_000_000;

/// The start-up repair of stored rows rewrites times for good (the original is not kept), so it
/// only touches rows this far ahead: years-wrong values, never a PC clock that started a few
/// minutes slow. Live and history paths keep the tight `FUTURE_SKEW_MS`.
pub const STORED_REPAIR_SKEW_MS: i64 = 24 * 60 * 60 * 1000;

pub fn is_future(ts: i64, now: i64) -> bool {
    ts > now.saturating_add(FUTURE_SKEW_MS)
}

/// A live message's time: the server's, unless it is in the future, then now. Live stanzas
/// carry the server clock so this should never fire; it is the last line of defence.
pub fn live_ts(ts: i64, now: i64) -> i64 {
    if is_future(ts, now) {
        now
    } else {
        ts
    }
}

/// Seconds (the protobuf unit) to ms, saturating instead of wrapping on garbage values.
fn secs_to_ms(secs: u64) -> i64 {
    secs.min(i64::MAX as u64 / 1000) as i64 * 1000
}

/// The first time anyone confirmed the message (delivered, read or played). Those timestamps
/// are stamped when the receipt reaches WhatsApp, so they are not the sender phone's clock.
fn earliest_receipt(w: &wa::WebMessageInfo) -> Option<i64> {
    w.user_receipt
        .iter()
        .flat_map(|r| [r.receipt_timestamp, r.read_timestamp, r.played_timestamp])
        .flatten()
        .filter_map(|t| u64::try_from(t).ok())
        .filter(|t| *t > 0)
        .map(secs_to_ms)
        .min()
}

/// What a history message says about its own time.
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    /// The message's own time, ms.
    pub ts: i64,
    /// Earliest receipt time, ms.
    pub receipt: Option<i64>,
    /// `HistorySyncMsg::msg_order_id`: the phone's own order of the conversation.
    pub order: Option<u64>,
}

/// Replacement times for messages dated in the future; everything else keeps its time.
///
/// Per future message, in this order:
/// 1. its earliest receipt time (server-stamped, a little after the real send time);
/// 2. else right after the previous message in `order` (ts + 1 ms, so a run keeps its order);
/// 3. else the next message in `order`;
/// 4. else the conversation's own time;
/// 5. else `EARLIEST_SANE_MS` (date unknown: the chat sinks instead of floating to the top),
///    stepping 1 ms per message so the run keeps its order.
///
/// A pick is then squeezed between the previous and next believable messages in `order`, so the
/// message never jumps outside its neighbours. Every result is at most `now`.
///
/// `order` is trusted only as a sequence. Its direction is not documented, so it is reversed
/// when the believable messages say it runs newest first.
pub fn repair_future(stamps: &[Stamp], conv_ts: Option<i64>, now: i64) -> Vec<i64> {
    let sane = |t: Option<i64>| t.filter(|t| *t >= EARLIEST_SANE_MS && !is_future(*t, now));
    let mut out: Vec<i64> = stamps.iter().map(|s| s.ts).collect();
    if !stamps.iter().any(|s| is_future(s.ts, now)) {
        return out;
    }

    let mut ordered: Vec<usize> = (0..stamps.len())
        .filter(|&i| stamps[i].order.is_some())
        .collect();
    ordered.sort_by_key(|&i| (stamps[i].order, i));
    let believable: Vec<i64> = ordered
        .iter()
        .map(|&i| stamps[i].ts)
        .filter(|t| !is_future(*t, now))
        .collect();
    let rising = believable.windows(2).filter(|w| w[1] > w[0]).count();
    let falling = believable.windows(2).filter(|w| w[1] < w[0]).count();
    if falling > rising {
        ordered.reverse();
    }

    // The nearest believable time after each position, so a run of bad times is bounded above.
    let mut next = vec![None; ordered.len()];
    let mut ahead = None;
    for (k, &i) in ordered.iter().enumerate().rev() {
        next[k] = ahead;
        if !is_future(stamps[i].ts, now) {
            ahead = Some(stamps[i].ts);
        }
    }
    let mut floor: Option<i64> = None;
    for (k, &i) in ordered.iter().enumerate() {
        let s = stamps[i];
        if !is_future(s.ts, now) {
            floor = Some(s.ts);
            continue;
        }
        let lo = floor.map(|f| f.saturating_add(1));
        let mut ts = sane(s.receipt)
            .or(lo)
            .or(next[k])
            .or(sane(conv_ts))
            .unwrap_or(EARLIEST_SANE_MS);
        if let Some(lo) = lo {
            ts = ts.max(lo);
        }
        if let Some(hi) = next[k] {
            ts = ts.min(hi);
        }
        ts = ts.min(now);
        out[i] = ts;
        floor = Some(ts);
    }
    for (i, s) in stamps.iter().enumerate() {
        if s.order.is_none() && is_future(s.ts, now) {
            out[i] = sane(s.receipt)
                .or(sane(conv_ts))
                .unwrap_or(EARLIEST_SANE_MS + i as i64)
                .min(now);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// History sync
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryChat {
    pub summary: ChatSummary,
    /// The phone sent a real name for this chat (not a number we made up).
    pub explicit_title: bool,
}

#[derive(Debug, Default)]
pub struct HistoryBatch {
    pub chats: Vec<HistoryChat>,
    pub messages: Vec<ChatMessage>,
    /// (JID user part, push name) pairs learned from this chunk.
    pub push_names: Vec<(String, String)>,
    /// (JID user part, address book name) pairs learned from this chunk.
    pub book_names: Vec<(String, String)>,
    /// (phone number user part, LID user part) pairs: the same person under two addresses.
    pub links: Vec<(String, String)>,
    /// Conversations left out because their id was not a usable JID.
    pub skipped: usize,
    /// Messages whose own time was in the future and got a replacement (see `repair_future`).
    pub future_fixed: usize,
}

pub(crate) fn parse_jid(s: &str) -> Option<Jid> {
    s.parse::<Jid>().ok().map(|j| j.to_non_ad())
}

/// The conversation's JID, preferring its phone-number form: the one the phone gave, else one
/// learned from a LID to phone-number link.
fn conversation_jid(conv: &wa::Conversation, names: &Names) -> Option<Jid> {
    let jid = parse_jid(&conv.id)?;
    let pn = conv.pn_jid.as_deref().and_then(parse_jid).or_else(|| {
        jid.is_lid()
            .then(|| names.linked(&jid.user).map(Jid::pn))
            .flatten()
    });
    Some(canonical_chat(&jid, pn.as_ref()))
}

/// Feeds one (address, address) pair of the same person into `local` and the batch.
fn learn_link(a: &str, b: &str, local: &mut Names, out: &mut HistoryBatch) {
    let (a, b) = (user_of(a), user_of(b));
    if local.link(&a, &b) {
        out.links.push((a, b));
    }
}

pub fn map_history(h: &wa::HistorySync, names: &Names, now: i64) -> HistoryBatch {
    let mut out = HistoryBatch::default();
    let mut local = names.clone();
    for p in &h.pushnames {
        if let (Some(id), Some(name)) = (&p.id, &p.pushname) {
            let user = user_of(id);
            local.set_push(&user, name);
            out.push_names.push((user, name.clone()));
        }
    }
    // Address book contacts and the phone's own LID table arrive inside the history chunk. The
    // contacts are the only copy of those names a fresh link ever gets, so they are not optional.
    for m in &h.phone_number_to_lid_mappings {
        if let (Some(pn), Some(lid)) = (&m.pn_jid, &m.lid_jid) {
            learn_link(pn, lid, &mut local, &mut out);
        }
    }
    for c in &h.inline_contacts {
        let name = [c.full_name.as_deref(), c.first_name.as_deref()]
            .into_iter()
            .flatten()
            .find(|n| !n.is_empty());
        if let (Some(pn), Some(lid)) = (&c.pn_jid, &c.lid_jid) {
            learn_link(pn, lid, &mut local, &mut out);
        }
        if let Some(name) = name {
            for id in [c.pn_jid.as_deref(), c.lid_jid.as_deref()]
                .into_iter()
                .flatten()
            {
                let user = user_of(id);
                if local.set_book(&user, name) {
                    out.book_names.push((user, name.to_string()));
                }
            }
        }
    }
    for conv in &h.conversations {
        if parse_jid(&conv.id).is_some_and(|j| j.is_lid() || j.is_pn()) {
            // A direct conversation names its other address in lid_jid or pn_jid.
            if let Some(other) = conv.lid_jid.as_ref().or(conv.pn_jid.as_ref()) {
                learn_link(&conv.id, other, &mut local, &mut out);
            }
        }
        let Some(jid) = conversation_jid(conv, &local) else {
            out.skipped += 1;
            continue;
        };
        if !is_supported_chat(&jid) {
            continue;
        }
        let group = jid.is_group();
        let explicit = conv
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .or(conv.display_name.as_deref().filter(|n| !n.is_empty()));
        let title = match explicit {
            Some(t) => t.to_string(),
            None if group => "Group".to_string(),
            None => local.label_with(&jid, None),
        };

        let conv_ts = conv
            .conversation_timestamp
            .or(conv.last_msg_timestamp)
            .filter(|t| *t > 0)
            .map(secs_to_ms)
            .filter(|t| !is_future(*t, now));

        // Every message with a time takes part in the repair (even ones that map to nothing, such
        // as reactions), because its neighbours' times bound a replacement.
        let timed: Vec<(&wa::WebMessageInfo, Option<u64>)> = conv
            .messages
            .iter()
            .filter_map(|hm| Some((hm.message.as_option()?, hm.msg_order_id)))
            .filter(|(w, _)| w.message_timestamp.is_some())
            .collect();
        let stamps: Vec<Stamp> = timed
            .iter()
            .map(|(w, order)| Stamp {
                ts: secs_to_ms(w.message_timestamp.unwrap_or(0)),
                receipt: earliest_receipt(w),
                order: *order,
            })
            .collect();
        let fixed = repair_future(&stamps, conv_ts, now);

        let mut newest: Option<(i64, String)> = None;
        for ((w, _), (stamp, ts)) in timed.iter().zip(stamps.iter().zip(fixed)) {
            out.future_fixed += usize::from(ts != stamp.ts);
            let Some(m) = map_web_message(w, &jid, &local, ts) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(t, _)| m.ts >= *t) {
                newest = Some((m.ts, preview(&m.sender, &m.text, group || m.from_me)));
            }
            out.messages.push(m);
        }
        let (last_ts, last_preview) = match newest {
            Some((ts, p)) => (Some(ts), p),
            None => (conv_ts, String::new()),
        };
        if last_ts.is_none() && explicit.is_none() {
            continue; // an empty conversation with no name: nothing to show
        }
        out.chats.push(HistoryChat {
            summary: ChatSummary {
                network: Network::WhatsApp,
                id: jid.to_string(),
                title,
                kind: chat_kind(&jid),
                last_ts,
                last_preview,
                unread: conv.unread_count.unwrap_or(0),
                ..ChatSummary::blank(Network::WhatsApp)
            },
            explicit_title: explicit.is_some(),
        });
    }
    out
}

/// `ts` is the message's time in ms after `repair_future`.
fn map_web_message(
    w: &wa::WebMessageInfo,
    chat: &Jid,
    names: &Names,
    ts: i64,
) -> Option<ChatMessage> {
    let key = w.key.as_option()?;
    let id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let Content::Text(text) = content(w.message.as_option()?) else {
        return None;
    };
    let push = w.push_name.as_deref().unwrap_or("");
    let raw = w.message.as_option()?;
    let sender_jid = if chat.is_group() {
        key.participant
            .as_deref()
            .or(w.participant.as_deref())
            .and_then(parse_jid)
            .unwrap_or_else(|| chat.clone())
    } else {
        chat.clone()
    };
    let d = crate::rich::details(raw, chat, names);
    let (text, mentions) = crate::rich::normalise_mentions(&text, &d.mentioned, names);
    Some(ChatMessage {
        network: Network::WhatsApp,
        chat_id: chat.to_string(),
        id,
        ts,
        sender: sender_label(from_me, &sender_jid, push, names),
        from_me,
        text,
        state: MessageState::Sent,
        // An own message in a direct chat has the peer as `sender_jid`: the id is ours instead.
        sender_id: if from_me {
            names
                .own_pn()
                .map(|u| Jid::pn(u).to_string())
                .unwrap_or_default()
        } else {
            sender_jid.to_non_ad().to_string()
        },
        reply_to: d.reply_to,
        attachments: d.attachments,
        // Own messages never "mention me": the counter is for what others wrote.
        mentions_me: !from_me && crate::rich::mentions_me(&d.mentioned, names),
        mentions,
        ..ChatMessage::blank(Network::WhatsApp)
    })
}

/// Folds a history chat into what the store already holds. History is a snapshot of the past:
/// it never makes a chat look older, and it does not reset unread counts we already track.
pub fn merge_chat(new: HistoryChat, existing: Option<&ChatSummary>) -> ChatSummary {
    let mut chat = new.summary;
    let Some(old) = existing else { return chat };
    if !new.explicit_title {
        chat.title = old.title.clone();
    }
    chat.unread = old.unread;
    if old.last_ts > chat.last_ts {
        chat.last_ts = old.last_ts;
        chat.last_preview = old.last_preview.clone();
    }
    chat
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jid(s: &str) -> Jid {
        s.parse().unwrap()
    }

    fn text(t: &str) -> wa::Message {
        wa::Message {
            conversation: Some(t.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn plain_and_extended_text() {
        assert_eq!(content(&text("hi")), Content::Text("hi".into()));
        let ext = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("link".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(content(&ext), Content::Text("link".into()));
    }

    #[test]
    fn media_markers() {
        let image = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("sunset".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(content(&image), Content::Text("[image] sunset".into()));

        let voice = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(content(&voice), Content::Text("[voice message]".into()));

        let doc = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("plan.pdf".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(content(&doc), Content::Text("[document: plan.pdf]".into()));

        let sticker = wa::Message {
            sticker_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(content(&sticker), Content::Text("[sticker]".into()));
        let location = wa::Message {
            location_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(content(&location), Content::Text("[location]".into()));
        let contact = wa::Message {
            contact_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(content(&contact), Content::Text("[contact]".into()));
        let video = wa::Message {
            video_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(content(&video), Content::Text("[video]".into()));
    }

    #[test]
    fn ephemeral_wrapper_is_unwrapped() {
        let wrapped = wa::Message {
            ephemeral_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(text("secret plans")),
            }),
            ..Default::default()
        };
        assert_eq!(content(&wrapped), Content::Text("secret plans".into()));
    }

    #[test]
    fn edit_and_revoke() {
        use wa::message::protocol_message::Type;
        let key = wa::MessageKey {
            id: Some("ABC".into()),
            ..Default::default()
        };
        let edit = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                key: MessageField::some(key.clone()),
                r#type: Some(Type::MessageEdit),
                edited_message: MessageField::some(text("fixed")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            content(&edit),
            Content::Edit {
                target: "ABC".into(),
                text: "fixed".into(),
                mentioned: vec![]
            }
        );
        let revoke = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                key: MessageField::some(key),
                r#type: Some(Type::Revoke),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            content(&revoke),
            Content::Revoke {
                target: "ABC".into()
            }
        );
    }

    #[test]
    fn plumbing_is_skipped_but_unknown_content_is_not() {
        assert_eq!(content(&wa::Message::default()), Content::Skip);
        let reaction = wa::Message {
            reaction_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(content(&reaction), Content::Skip);
        // Sender key rides along with real content we cannot read: must stay visible.
        let unknown = wa::Message {
            sender_key_distribution_message: MessageField::some(Default::default()),
            order_message: MessageField::some(wa::message::OrderMessage {
                item_count: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(content(&unknown), Content::Text(UNSUPPORTED.into()));
    }

    #[test]
    fn canonical_chat_prefers_phone_number() {
        let lid = jid("100200300@lid");
        let pn = jid("15550100000@s.whatsapp.net");
        assert_eq!(
            canonical_chat(&lid, Some(&pn)).to_string(),
            "15550100000@s.whatsapp.net"
        );
        assert_eq!(canonical_chat(&lid, None).to_string(), "100200300@lid");
        // A device suffix never leaks into the chat id.
        let dev = jid("15550100000:5@s.whatsapp.net");
        assert_eq!(
            canonical_chat(&dev, None).to_string(),
            "15550100000@s.whatsapp.net"
        );
        let group = jid("120363000000000000@g.us");
        assert_eq!(
            canonical_chat(&group, Some(&pn)).to_string(),
            "120363000000000000@g.us"
        );
    }

    #[test]
    fn chat_scope() {
        assert!(is_supported_chat(&jid("15550100000@s.whatsapp.net")));
        assert!(is_supported_chat(&jid("120363000000000000@g.us")));
        assert!(!is_supported_chat(&jid("status@broadcast")));
        assert!(!is_supported_chat(&jid("120363000000000000@newsletter")));
    }

    #[test]
    fn user_part_extraction() {
        assert_eq!(user_of("15550100000@s.whatsapp.net"), "15550100000");
        assert_eq!(user_of("15550100000:12@s.whatsapp.net"), "15550100000");
        assert_eq!(user_of("100200300@lid"), "100200300");
        assert_eq!(user_of("plain"), "plain");
    }

    #[test]
    fn name_priority_and_titles() {
        let alex = jid("15550100001@s.whatsapp.net");
        let mut names = Names::default();
        assert_eq!(names.label_with(&alex, None), "+15550100001");
        assert_eq!(names.label_with(&jid("100@lid"), None), "Unknown contact");
        names.set_push("15550100001", "Alex P");
        assert_eq!(names.label_with(&alex, None), "Alex P");
        assert_eq!(names.label_with(&alex, Some("hint")), "hint");
        names.set_book("15550100001", "Alex Example");
        assert_eq!(names.label_with(&alex, Some("hint")), "Alex Example");

        // A push name replaces a bare number only; the address book always wins.
        let mut n2 = Names::default();
        n2.set_push("15550100001", "Alex P");
        assert_eq!(
            n2.better_title(&alex, "+15550100001"),
            Some("Alex P".into())
        );
        assert_eq!(n2.better_title(&alex, "Alex Example"), None);
        assert_eq!(
            names.better_title(&alex, "Alex P"),
            Some("Alex Example".into())
        );
        assert_eq!(names.better_title(&alex, "Alex Example"), None);
        assert_eq!(
            names.better_title(&jid("120363000000000000@g.us"), "Group"),
            None
        );
    }

    #[test]
    fn previews() {
        assert_eq!(preview("Sam", "thanks", true), "Sam: thanks");
        assert_eq!(preview("Sam", "thanks", false), "thanks");
        assert_eq!(
            preview("Sam", "two\nlines  here", true),
            "Sam: two lines here"
        );
        assert_eq!(
            preview("Sam", &"x".repeat(500), false).chars().count(),
            PREVIEW_CHARS
        );
    }

    fn web_msg(
        id: &str,
        from_me: bool,
        ts: u64,
        body: &str,
        participant: Option<&str>,
    ) -> wa::HistorySyncMsg {
        wa::HistorySyncMsg {
            message: MessageField::some(wa::WebMessageInfo {
                key: MessageField::some(wa::MessageKey {
                    id: Some(id.into()),
                    from_me: Some(from_me),
                    participant: participant.map(str::to_string),
                    ..Default::default()
                }),
                message: MessageField::some(text(body)),
                message_timestamp: Some(ts),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn history_maps_chats_messages_and_names() {
        let h = wa::HistorySync {
            pushnames: vec![wa::Pushname {
                id: Some("15550100002@s.whatsapp.net".into()),
                pushname: Some("Sam S".into()),
            }],
            conversations: vec![
                wa::Conversation {
                    id: "15550100001@s.whatsapp.net".into(),
                    name: Some("Alex Example".into()),
                    unread_count: Some(2),
                    messages: vec![
                        web_msg("m1", false, 1_700_000_000, "is the build done?", None),
                        web_msg("m2", true, 1_700_000_100, "yes, shipped", None),
                    ],
                    ..Default::default()
                },
                wa::Conversation {
                    id: "120363000000000000@g.us".into(),
                    name: Some("Project team".into()),
                    messages: vec![web_msg(
                        "g1",
                        false,
                        1_700_000_200,
                        "thanks",
                        Some("15550100002@s.whatsapp.net"),
                    )],
                    ..Default::default()
                },
                wa::Conversation {
                    id: "status@broadcast".into(),
                    ..Default::default()
                },
                wa::Conversation {
                    id: "not a jid".into(),
                    ..Default::default()
                },
                wa::Conversation {
                    id: "15550100009@s.whatsapp.net".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.skipped, 1);
        assert_eq!(
            b.chats.len(),
            2,
            "status, bad id and the empty unnamed chat are dropped"
        );
        assert_eq!(b.messages.len(), 3);
        assert_eq!(
            b.push_names,
            vec![("15550100002".to_string(), "Sam S".to_string())]
        );

        let alex = &b.chats[0].summary;
        assert_eq!(alex.title, "Alex Example");
        assert_eq!(alex.kind, ChatKind::Direct);
        assert_eq!(alex.last_ts, Some(1_700_000_100_000));
        assert_eq!(alex.last_preview, "You: yes, shipped");
        assert_eq!(alex.unread, 2);
        assert!(b.chats[0].explicit_title);

        let team = &b.chats[1].summary;
        assert_eq!(team.kind, ChatKind::Group);
        assert_eq!(team.last_preview, "Sam S: thanks");
        assert_eq!(b.messages[2].sender, "Sam S");
        assert!(!b.messages[2].from_me);
    }

    #[test]
    fn history_uses_phone_number_for_lid_conversation() {
        let h = wa::HistorySync {
            conversations: vec![wa::Conversation {
                id: "100200300@lid".into(),
                pn_jid: Some("15550100001@s.whatsapp.net".into()),
                conversation_timestamp: Some(1_700_000_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.chats[0].summary.id, "15550100001@s.whatsapp.net");
        assert_eq!(b.chats[0].summary.title, "+15550100001");
        assert_eq!(b.chats[0].summary.last_ts, Some(1_700_000_000_000));
    }

    // ---- message times --------------------------------------------------------------------

    /// "Now" for the tests, a recent timestamp (about 1_794_790_000 s).
    const NOW: i64 = 1_794_790_000_000;
    const SEC: i64 = 1000;

    fn stamp(secs: i64, order: Option<u64>) -> Stamp {
        Stamp {
            ts: secs * SEC,
            receipt: None,
            order,
        }
    }

    #[test]
    fn believable_times_are_left_alone() {
        let s = [stamp(1_794_000_000, Some(1)), stamp(1_794_000_100, Some(2))];
        assert_eq!(repair_future(&s, None, NOW), vec![s[0].ts, s[1].ts]);
        // Inside the skew window is still believable.
        assert!(!is_future(NOW + FUTURE_SKEW_MS, NOW));
        assert!(is_future(NOW + FUTURE_SKEW_MS + 1, NOW));
        assert_eq!(live_ts(NOW + 5, NOW), NOW + 5);
    }

    #[test]
    fn live_future_time_becomes_now() {
        assert_eq!(live_ts(2_057_119_831 * SEC, NOW), NOW);
    }

    #[test]
    fn receipt_time_is_the_first_choice() {
        let mut s = [
            stamp(1_794_000_000, Some(1)),
            stamp(2_057_119_831, Some(2)),
            stamp(1_794_000_900, Some(3)),
        ];
        s[1].receipt = Some(1_794_000_010 * SEC);
        assert_eq!(repair_future(&s, None, NOW)[1], 1_794_000_010 * SEC);
        // A receipt that is itself in the future, or outside the neighbours, does not win.
        s[1].receipt = Some(2_057_119_900 * SEC);
        assert_eq!(repair_future(&s, None, NOW)[1], 1_794_000_000 * SEC + 1);
        s[1].receipt = Some(1_794_100_000 * SEC); // after the next message: squeezed
        assert_eq!(repair_future(&s, None, NOW)[1], 1_794_000_900 * SEC);
    }

    #[test]
    fn a_run_of_future_messages_keeps_its_order_right_after_the_previous_one() {
        // Shaped like the evidence: 4 own messages 53 s apart, all years ahead, in one chat.
        let s = [
            stamp(1_794_000_000, Some(10)),
            stamp(2_459_086_174, Some(11)),
            stamp(2_459_086_190, Some(12)),
            stamp(2_459_086_201, Some(13)),
            stamp(2_459_086_227, Some(14)),
            stamp(1_794_000_500, Some(15)),
        ];
        let out = repair_future(&s, None, NOW);
        assert_eq!(out[0], 1_794_000_000 * SEC);
        assert_eq!(out[5], 1_794_000_500 * SEC);
        for k in 1..=4 {
            assert!(out[k] > out[k - 1], "order kept at {k}");
            assert!(out[k] <= out[5], "never after the next real message");
            assert!(!is_future(out[k], NOW));
        }
        assert_eq!(out[1], 1_794_000_000 * SEC + 1);
    }

    #[test]
    fn order_ids_running_newest_first_are_read_correctly() {
        let s = [
            stamp(1_794_000_500, Some(30)),
            stamp(2_057_119_831, Some(20)),
            stamp(1_794_000_000, Some(10)),
        ];
        let out = repair_future(&s, None, NOW);
        assert_eq!(
            out[1],
            1_794_000_000 * SEC + 1,
            "between the two real messages"
        );
    }

    #[test]
    fn without_neighbours_the_conversation_time_then_now() {
        let only = [stamp(2_173_107_337, None)];
        assert_eq!(
            repair_future(&only, Some(1_794_700_000 * SEC), NOW),
            vec![1_794_700_000 * SEC]
        );
        assert_eq!(repair_future(&only, None, NOW), vec![EARLIEST_SANE_MS]);
        // A conversation time that is itself in the future is ignored.
        assert_eq!(
            repair_future(&only, Some(2_352_421_960 * SEC), NOW),
            vec![EARLIEST_SANE_MS]
        );
        // Order ids present but only future messages: nothing to anchor to either.
        let all_bad = [stamp(2_057_119_831, Some(1)), stamp(2_057_119_858, Some(2))];
        let out = repair_future(&all_bad, None, NOW);
        assert!(out.iter().all(|t| *t <= NOW));
        assert!(out[0] < out[1], "send order survives with no anchor");
        // And without order ids: still distinct, still old.
        let no_order = [stamp(2_057_119_831, None), stamp(2_057_119_858, None)];
        let out = repair_future(&no_order, None, NOW);
        assert!(out[0] < out[1] && out[1] < NOW - 365 * 24 * 3600 * SEC);
    }

    #[test]
    fn first_message_bad_uses_the_next_real_one() {
        let s = [stamp(2_610_505_345, Some(1)), stamp(1_794_000_000, Some(2))];
        assert_eq!(repair_future(&s, None, NOW)[0], 1_794_000_000 * SEC);
    }

    #[test]
    fn huge_garbage_time_does_not_overflow() {
        let h = wa::HistorySync {
            conversations: vec![wa::Conversation {
                id: "15550100001@s.whatsapp.net".into(),
                messages: vec![web_msg("a", true, u64::MAX, "x", None)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.messages[0].ts, EARLIEST_SANE_MS);
    }

    /// Own messages with the phone's clock in the future: the chat must keep sorting by its
    /// real activity, and the receipt time is used where the phone sent one.
    #[test]
    fn history_with_future_own_messages_does_not_float_the_chat() {
        let mut bad = web_msg(
            "own-bad",
            true,
            2_057_119_831,
            "sent from the odd phone",
            None,
        );
        bad.msg_order_id = Some(2);
        bad.message.as_option_mut().unwrap().user_receipt = vec![wa::UserReceipt {
            user_jid: "15550100001@s.whatsapp.net".into(),
            receipt_timestamp: Some(1_794_000_020),
            ..Default::default()
        }];
        let mut before = web_msg("m1", false, 1_794_000_000, "hello", None);
        before.msg_order_id = Some(1);
        let mut after = web_msg("m3", false, 1_794_000_600, "later", None);
        after.msg_order_id = Some(3);
        let h = wa::HistorySync {
            conversations: vec![wa::Conversation {
                id: "15550100001@s.whatsapp.net".into(),
                name: Some("Alex Example".into()),
                // The phone's own conversation time is also from its odd clock.
                conversation_timestamp: Some(2_057_119_858),
                messages: vec![after, bad, before],
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.future_fixed, 1);
        let chat = &b.chats[0].summary;
        assert_eq!(
            chat.last_ts,
            Some(1_794_000_600 * SEC),
            "newest real message wins"
        );
        assert_eq!(chat.last_preview, "later");
        let own = b.messages.iter().find(|m| m.id == "own-bad").unwrap();
        assert_eq!(own.ts, 1_794_000_020 * SEC);
        assert!(b.messages.iter().all(|m| !is_future(m.ts, NOW)));
    }

    #[test]
    fn empty_chat_with_future_conversation_time_gets_no_time() {
        let h = wa::HistorySync {
            conversations: vec![wa::Conversation {
                id: "15550100001@s.whatsapp.net".into(),
                name: Some("Alex Example".into()),
                conversation_timestamp: Some(2_352_421_960),
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.chats[0].summary.last_ts, None);
    }

    // ---- names ----------------------------------------------------------------------------

    #[test]
    fn linked_addresses_share_names_in_both_directions() {
        let lid = jid("100200300@lid");
        let pn = jid("15550100001@s.whatsapp.net");
        let mut names = Names::default();
        assert_eq!(names.label_with(&lid, None), "Unknown contact");
        names.set_push("15550100001", "Alex P");
        assert_eq!(
            names.label_with(&lid, None),
            "Unknown contact",
            "no link yet"
        );
        assert!(names.link("15550100001", "100200300"));
        assert!(
            !names.link("100200300", "15550100001"),
            "same pair twice is no change"
        );
        assert_eq!(names.label_with(&lid, None), "Alex P");
        names.set_book("100200300", "Alex Example");
        assert_eq!(names.label_with(&pn, None), "Alex Example");
        assert_eq!(
            names.better_title(&lid, "Unknown contact"),
            Some("Alex Example".into())
        );
        assert_eq!(names.linked("100200300"), Some("15550100001"));
        assert_eq!(names.linked("15550100001"), Some("100200300"));
    }

    #[test]
    fn names_queue_changes_and_reload_without_requeueing() {
        let mut a = Names::default();
        a.set_book("1", "Alex Example");
        a.set_push("2", "Sam S");
        a.link("3", "2");
        assert!(!a.set_book("1", "Alex Example"), "unchanged is not queued");
        let rows = a.take_pending();
        assert_eq!(rows.len(), 3);
        assert!(a.take_pending().is_empty());
        let mut b = Names::default();
        b.load(rows);
        assert!(b.take_pending().is_empty());
        let lid = jid("3@lid");
        assert_eq!(b.label_with(&lid, None), "Sam S");
        assert_eq!(b.label_with(&jid("1@s.whatsapp.net"), None), "Alex Example");
    }

    #[test]
    fn relinking_an_address_leaves_no_stale_reverse_entry() {
        let mut n = Names::default();
        n.link("a", "c");
        n.link("a", "b");
        assert_eq!(n.linked("a"), Some("b"));
        assert_eq!(n.linked("b"), Some("a"));
        assert_eq!(n.linked("c"), None, "c no longer points at a");
    }

    #[test]
    fn a_real_name_is_never_replaced_by_a_push_name() {
        let mut names = Names::default();
        names.set_push("1", "alexp");
        assert_eq!(
            names.better_title(&jid("1@s.whatsapp.net"), "Alex Example"),
            None
        );
        assert_eq!(
            names.better_title(&jid("1@s.whatsapp.net"), "+1"),
            Some("alexp".into())
        );
    }

    #[test]
    fn contact_record_addresses_and_link() {
        let pn = jid("15550100001@s.whatsapp.net");
        let lid = jid("100200300@lid");
        let (users, link) = contact_addresses(
            &pn,
            Some("100200300@lid"),
            Some("15550100001@s.whatsapp.net"),
        );
        assert_eq!(users, ["15550100001", "100200300"]);
        assert_eq!(link, Some(("15550100001".into(), "100200300".into())));
        // The record's own address counts as one side of the pair.
        let (_, link) = contact_addresses(&lid, None, Some("15550100001@s.whatsapp.net"));
        assert_eq!(link, Some(("15550100001".into(), "100200300".into())));
        let (users, link) = contact_addresses(&pn, None, None);
        assert_eq!((users, link), (vec!["15550100001".to_string()], None));
    }

    #[test]
    fn app_state_report_separates_failures_from_skips() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // A real-world log line shape: nothing fatal, nothing retryable.
        let (text, is_error) = app_state_report(&[], &[], &s(&["critical_unblock_low"]));
        assert!(is_error, "a skipped collection is stranded, not harmless");
        assert!(text.contains("contacts not synced") && !text.contains("nothing failed"));
        // Another skipped collection is still an error, without the contacts wording.
        let (text, is_error) = app_state_report(&[], &[], &s(&["regular_low"]));
        assert!(is_error && !text.contains("contacts"));
        let (text, is_error) = app_state_report(&[], &[], &[]);
        assert!(!is_error && text.contains("nothing failed"));
        let (text, is_error) = app_state_report(&s(&["critical_block"]), &[], &[]);
        assert!(is_error);
        assert!(text.contains("refused by WhatsApp [critical_block]"));
        assert!(app_state_report(&[], &s(&["regular_high"]), &[]).1);
    }

    #[test]
    fn sender_labels_give_a_name_only_when_real() {
        let c = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
            v.iter().map(|(s, n)| (s.to_string(), *n)).collect()
        };
        assert_eq!(
            name_from_senders(&c(&[("+15550100001", 9), ("Alex P", 2), ("Alex Q", 2)])),
            Some("Alex P".into())
        );
        assert_eq!(
            name_from_senders(&c(&[("Unknown contact", 5), ("You", 3), ("+1555", 1)])),
            None
        );
        assert_eq!(name_from_senders(&[]), None);
    }

    #[test]
    fn history_learns_contacts_links_and_resolves_lid_chats() {
        let h = wa::HistorySync {
            // The phone's address book, in the chunk itself.
            inline_contacts: vec![wa::InlineContact {
                pn_jid: Some("15550100001@s.whatsapp.net".into()),
                lid_jid: Some("100200300@lid".into()),
                full_name: Some("Alex Example".into()),
                ..Default::default()
            }],
            phone_number_to_lid_mappings: vec![wa::PhoneNumberToLIDMapping {
                pn_jid: Some("15550100002@s.whatsapp.net".into()),
                lid_jid: Some("400500600@lid".into()),
            }],
            pushnames: vec![wa::Pushname {
                id: Some("15550100002@s.whatsapp.net".into()),
                pushname: Some("Sam S".into()),
            }],
            conversations: vec![
                // A LID conversation with no pn_jid: the contact list resolves it.
                wa::Conversation {
                    id: "100200300@lid".into(),
                    conversation_timestamp: Some(1_794_000_000),
                    ..Default::default()
                },
                // Resolved only through the phone's LID table, named by a phone-number push name.
                wa::Conversation {
                    id: "400500600@lid".into(),
                    conversation_timestamp: Some(1_794_000_100),
                    ..Default::default()
                },
                // Unknown to every table: stays a LID chat and honestly unnamed.
                wa::Conversation {
                    id: "700800900@lid".into(),
                    conversation_timestamp: Some(1_794_000_200),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(b.chats.len(), 3);
        assert_eq!(b.chats[0].summary.id, "15550100001@s.whatsapp.net");
        assert_eq!(b.chats[0].summary.title, "Alex Example");
        assert_eq!(b.chats[1].summary.id, "15550100002@s.whatsapp.net");
        assert_eq!(b.chats[1].summary.title, "Sam S");
        assert_eq!(b.chats[2].summary.id, "700800900@lid");
        assert_eq!(b.chats[2].summary.title, "Unknown contact");
        assert_eq!(b.book_names.len(), 2, "pn and lid user parts both named");
        assert_eq!(b.links.len(), 2);
    }

    #[test]
    fn history_lid_chat_uses_conversation_pn_jid_link_for_group_sender_names() {
        let h = wa::HistorySync {
            pushnames: vec![wa::Pushname {
                id: Some("15550100002@s.whatsapp.net".into()),
                pushname: Some("Sam S".into()),
            }],
            conversations: vec![
                wa::Conversation {
                    id: "400500600@lid".into(),
                    pn_jid: Some("15550100002@s.whatsapp.net".into()),
                    ..Default::default()
                },
                wa::Conversation {
                    id: "120363000000000000@g.us".into(),
                    name: Some("Project team".into()),
                    messages: vec![web_msg(
                        "g1",
                        false,
                        1_794_000_000,
                        "thanks",
                        Some("400500600@lid"),
                    )],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let b = map_history(&h, &Names::default(), NOW);
        assert_eq!(
            b.messages[0].sender, "Sam S",
            "group sender named through the link"
        );
    }

    fn summary(title: &str, ts: Option<i64>, unread: u32) -> ChatSummary {
        ChatSummary {
            network: Network::WhatsApp,
            id: "x".into(),
            title: title.into(),
            kind: ChatKind::Direct,
            last_ts: ts,
            last_preview: format!("p{}", ts.unwrap_or(0)),
            unread,
            ..ChatSummary::blank(Network::WhatsApp)
        }
    }

    #[test]
    fn merge_keeps_newer_state_and_unread() {
        let old = summary("Old name", Some(500), 4);
        let new = HistoryChat {
            summary: summary("+614", Some(100), 0),
            explicit_title: false,
        };
        let m = merge_chat(new, Some(&old));
        assert_eq!(m.title, "Old name");
        assert_eq!(m.last_ts, Some(500));
        assert_eq!(m.last_preview, "p500");
        assert_eq!(m.unread, 4);

        let new = HistoryChat {
            summary: summary("Real name", Some(900), 7),
            explicit_title: true,
        };
        let m = merge_chat(new.clone(), Some(&old));
        assert_eq!(
            (m.title.as_str(), m.last_ts, m.unread),
            ("Real name", Some(900), 4)
        );
        assert_eq!(merge_chat(new, None).unread, 7);
    }
}
