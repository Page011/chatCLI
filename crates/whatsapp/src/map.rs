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

/// Display names by JID user part: the phone's address book wins over push names (the name a
/// contact chose for themselves).
#[derive(Debug, Clone, Default)]
pub struct Names {
    book: HashMap<String, String>,
    push: HashMap<String, String>,
}

impl Names {
    pub fn set_book(&mut self, user: &str, name: &str) -> bool {
        set(&mut self.book, user, name)
    }

    pub fn set_push(&mut self, user: &str, name: &str) -> bool {
        set(&mut self.push, user, name)
    }

    /// `hint` is the push name carried by the message itself.
    pub fn label_with(&self, jid: &Jid, hint: Option<&str>) -> String {
        let user = jid.user.as_str();
        self.book
            .get(user)
            .cloned()
            .or_else(|| hint.filter(|h| !h.is_empty()).map(str::to_string))
            .or_else(|| self.push.get(user).cloned())
            .unwrap_or_else(|| fallback_label(jid))
    }

    /// A better title for a direct chat than `current`, or None to leave it alone. An address
    /// book name always wins; a push name only replaces a bare number.
    pub fn better_title(&self, jid: &Jid, current: &str) -> Option<String> {
        if jid.is_group() {
            return None;
        }
        let user = jid.user.as_str();
        if let Some(b) = self.book.get(user) {
            return (b != current).then(|| b.clone());
        }
        if is_fallback_title(current) {
            return self
                .push
                .get(user)
                .filter(|p| p.as_str() != current)
                .cloned();
        }
        None
    }
}

fn set(map: &mut HashMap<String, String>, user: &str, name: &str) -> bool {
    if user.is_empty() || name.is_empty() || map.get(user).is_some_and(|n| n == name) {
        return false;
    }
    map.insert(user.to_string(), name.to_string());
    true
}

/// Who wrote a message, as shown in the pane and in group previews.
pub fn sender_label(from_me: bool, sender: &Jid, push_name: &str, names: &Names) -> String {
    if from_me {
        OWN_LABEL.to_string()
    } else {
        names.label_with(&sender.to_non_ad(), Some(push_name))
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
    /// The sender edited message `target`.
    Edit {
        target: String,
        text: String,
    },
    /// The sender deleted message `target` for everyone.
    Revoke {
        target: String,
    },
    /// Protocol traffic, reactions and the like: nothing to show.
    Skip,
}

pub fn content(msg: &wa::Message) -> Content {
    let base = msg.get_base_message();
    if let Some(p) = base.protocol_message.as_option() {
        return protocol(p);
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

fn with_caption(marker: &str, caption: Option<&str>) -> String {
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
        Some(Type::MessageEdit) => match p.edited_message.as_option().map(content) {
            Some(Content::Text(text)) => Content::Edit { target, text },
            _ => Content::Skip,
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
    /// Conversations left out because their id was not a usable JID.
    pub skipped: usize,
}

fn parse_jid(s: &str) -> Option<Jid> {
    s.parse::<Jid>().ok().map(|j| j.to_non_ad())
}

/// The conversation's JID, preferring its phone-number form when the phone gave one.
fn conversation_jid(conv: &wa::Conversation) -> Option<Jid> {
    let jid = parse_jid(&conv.id)?;
    let pn = conv.pn_jid.as_deref().and_then(parse_jid);
    Some(canonical_chat(&jid, pn.as_ref()))
}

pub fn map_history(h: &wa::HistorySync, names: &Names) -> HistoryBatch {
    let mut out = HistoryBatch::default();
    let mut local = names.clone();
    for p in &h.pushnames {
        if let (Some(id), Some(name)) = (&p.id, &p.pushname) {
            let user = user_of(id);
            local.set_push(&user, name);
            out.push_names.push((user, name.clone()));
        }
    }
    for conv in &h.conversations {
        let Some(jid) = conversation_jid(conv) else {
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

        let mut newest: Option<(i64, String)> = None;
        for hm in &conv.messages {
            let Some(w) = hm.message.as_option() else {
                continue;
            };
            let Some(m) = map_web_message(w, &jid, &local) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(ts, _)| m.ts >= *ts) {
                newest = Some((m.ts, preview(&m.sender, &m.text, group || m.from_me)));
            }
            out.messages.push(m);
        }
        let conv_ts = conv
            .conversation_timestamp
            .or(conv.last_msg_timestamp)
            .filter(|t| *t > 0)
            .map(|t| t as i64 * 1000);
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
            },
            explicit_title: explicit.is_some(),
        });
    }
    out
}

fn map_web_message(w: &wa::WebMessageInfo, chat: &Jid, names: &Names) -> Option<ChatMessage> {
    let key = w.key.as_option()?;
    let id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let Content::Text(text) = content(w.message.as_option()?) else {
        return None;
    };
    let ts = w.message_timestamp? as i64 * 1000;
    let push = w.push_name.as_deref().unwrap_or("");
    let sender_jid = if chat.is_group() {
        key.participant
            .as_deref()
            .or(w.participant.as_deref())
            .and_then(parse_jid)
            .unwrap_or_else(|| chat.clone())
    } else {
        chat.clone()
    };
    Some(ChatMessage {
        network: Network::WhatsApp,
        chat_id: chat.to_string(),
        id,
        ts,
        sender: sender_label(from_me, &sender_jid, push, names),
        from_me,
        text,
        state: MessageState::Sent,
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
                text: "fixed".into()
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
        let b = map_history(&h, &Names::default());
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
        let b = map_history(&h, &Names::default());
        assert_eq!(b.chats[0].summary.id, "15550100001@s.whatsapp.net");
        assert_eq!(b.chats[0].summary.title, "+15550100001");
        assert_eq!(b.chats[0].summary.last_ts, Some(1_700_000_000_000));
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
