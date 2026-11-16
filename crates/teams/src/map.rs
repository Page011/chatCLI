//! Pure mapping from Graph's shapes to chatcli's model, plus the small rules around it
//! (unread, token expiry, catch-up gaps). No network, no store: everything here is unit tested.

use std::collections::HashMap;

use base64::Engine;
use serde_json::Value;

use chatcli_core::model::{
    parse_iso_ms, Attachment, Capabilities, ChatFlags, ChatKind, ChatMessage, ChatSummary,
    FetchRef, MediaKind, MessageState, Network, PersonRef, QuotedRef,
};
use chatcli_core::settings::TeamsSettings;
use chatcli_core::store::{ReactionRow, REACTOR_ME};

use crate::graph::{Chat, Message, MsgAttachment};

/// The sidebar shows one line; this keeps a long message from filling the store row.
const PREVIEW_MAX_CHARS: usize = 120;
/// A reaction longer than this is not one emoji: it shows as the custom marker.
const EMOJI_MAX_CHARS: usize = 16;

/// Refresh an access token this long before it expires.
pub const TOKEN_MARGIN_MS: i64 = 60_000;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn user_id(from: &Option<crate::graph::From>) -> Option<&str> {
    from.as_ref()?.user.as_ref()?.id.as_deref()
}

/// "Sender: text", with "You" for the signed-in user.
pub fn preview_line(sender: &str, from_me: bool, text: &str) -> String {
    let who = if from_me { "You" } else { sender };
    clip(&format!("{who}: {text}"), PREVIEW_MAX_CHARS)
}

/// 1 when the last message is newer than the user's read mark and not their own, else 0.
/// A missing read mark means never read. Milliseconds, so two messages in one second differ.
pub fn unread_for(last_ms: Option<i64>, last_from_me: bool, read_ms: Option<i64>) -> u32 {
    match (last_ms, last_from_me, read_ms) {
        (None, _, _) | (_, true, _) => 0,
        (Some(_), false, None) => 1,
        (Some(last), false, Some(read)) => u32::from(last > read),
    }
}

/// Unread count after a message arrived live. Only a genuinely new message from someone else
/// counts: an edit, a redelivery or the user's own message must not.
pub fn unread_after(current: u32, from_me: bool, is_new: bool) -> u32 {
    if is_new && !from_me {
        current.saturating_add(1)
    } else {
        current
    }
}

/// A 1:1 or group chat the user hid. Teams unhides it when a new message arrives, so a
/// notification for it is worth a re-list; meeting and other chat types never are.
pub fn is_hidden_chat(chat: &Chat) -> bool {
    matches!(chat.chat_type.as_str(), "oneOnOne" | "group")
        && chat.viewpoint.as_ref().and_then(|v| v.is_hidden) == Some(true)
}

/// The plain text of a chat's last-message preview. The same text rules as a full message, so
/// a stored message and its preview read the same when nothing changed.
fn preview_text(chat: &Chat) -> Option<(String, bool)> {
    let p = chat.last_message_preview.as_ref()?;
    let deleted = p.is_deleted == Some(true);
    let text = if deleted {
        "[deleted]".to_string()
    } else if p.message_type.as_deref().is_some_and(|t| t != "message") {
        "[system event]".to_string()
    } else {
        p.body.as_ref().map(|b| b.text()).unwrap_or_default()
    };
    Some((text, deleted))
}

/// None = the chat is not shown: a meeting or other chat type. A chat the user hid IS shown, with
/// `flags.hidden` set, so the TUI's "show hidden" can list it.
pub fn chat_summary(chat: &Chat, me_id: &str) -> Option<ChatSummary> {
    let kind = match chat.chat_type.as_str() {
        "oneOnOne" => ChatKind::Direct,
        "group" => ChatKind::Group,
        _ => return None,
    };
    let preview = chat.last_message_preview.as_ref();
    let last_ts = preview
        .and_then(|p| p.created_date_time.as_deref())
        .and_then(parse_iso_ms);
    let from_me = preview.is_some_and(|p| user_id(&p.from) == Some(me_id));
    let last_preview = preview
        .map(|p| {
            let text = preview_text(chat).map(|(t, _)| t).unwrap_or_default();
            let sender = p.from.as_ref().map_or("(system)", |f| f.name());
            preview_line(sender, from_me, &text)
        })
        .unwrap_or_default();
    let read_ms = chat
        .viewpoint
        .as_ref()
        .and_then(|v| v.last_message_read_date_time.as_deref())
        .and_then(parse_iso_ms);
    Some(ChatSummary {
        network: Network::Teams,
        id: chat.id.clone(),
        title: chat.title(me_id),
        kind,
        last_ts,
        last_preview,
        unread: unread_for(last_ts, from_me, read_ms),
        flags: ChatFlags {
            hidden: is_hidden_chat(chat),
            ..ChatFlags::default()
        },
        ..ChatSummary::blank(Network::Teams)
    })
}

/// What the chat list says about a chat's last message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLast {
    pub id: String,
    pub deleted: bool,
    pub text: String,
}

pub fn remote_last(chat: &Chat) -> Option<RemoteLast> {
    let id = chat.last_message_preview.as_ref()?.id.clone()?;
    let (text, deleted) = preview_text(chat)?;
    Some(RemoteLast { id, deleted, text })
}

/// The loose id of a mention (a number in Graph's answers) as the text the `<at id>` carries.
fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn json_content(att: &MsgAttachment) -> Option<Value> {
    match att.content.as_ref()? {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

/// The quote of a `messageReference` attachment. Sender and preview may be missing: they are
/// then empty, and the TUI shows a placeholder.
fn quote_from(att: &MsgAttachment, me_id: &str) -> QuotedRef {
    let content = json_content(att).unwrap_or(Value::Null);
    let id = content
        .get("messageId")
        .and_then(Value::as_str)
        .or(att.id.as_deref())
        .unwrap_or_default()
        .to_string();
    let user = content.pointer("/messageSender/user");
    let sender_id = user
        .and_then(|u| u.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sender = if !sender_id.is_empty() && sender_id == me_id {
        "You".to_string()
    } else {
        user.and_then(|u| u.get("displayName"))
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|n| !n.is_empty())
            .unwrap_or_default()
    };
    let preview = content
        .get("messagePreview")
        .and_then(Value::as_str)
        .map(|p| clip(&one_line(p), PREVIEW_MAX_CHARS))
        .unwrap_or_default();
    QuotedRef {
        id,
        sender,
        sender_id,
        preview,
    }
}

/// How chatcli fetches one attachment later. Stored as JSON in the attachment's `FetchRef`.
/// It holds ids and the file link, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchSpec {
    /// An inline picture in the message body.
    Hosted {
        chat_id: String,
        message_id: String,
        hosted_id: String,
    },
    /// A file in OneDrive or SharePoint, by its link.
    File { url: String },
}

impl FetchSpec {
    pub fn to_ref(&self) -> FetchRef {
        FetchRef::new(match self {
            FetchSpec::Hosted {
                chat_id,
                message_id,
                hosted_id,
            } => serde_json::json!({ "k": "hosted", "c": chat_id, "m": message_id, "h": hosted_id }),
            FetchSpec::File { url } => serde_json::json!({ "k": "file", "u": url }),
        }
        .to_string())
    }

    pub fn from_ref(r: &FetchRef) -> Option<FetchSpec> {
        let v: Value = serde_json::from_str(r.as_str()).ok()?;
        let s = |key: &str| v.get(key)?.as_str().map(str::to_string);
        match v.get("k")?.as_str()? {
            "hosted" => Some(FetchSpec::Hosted {
                chat_id: s("c")?,
                message_id: s("m")?,
                hosted_id: s("h")?,
            }),
            "file" => Some(FetchSpec::File { url: s("u")? }),
            _ => None,
        }
    }
}

/// Picture, video, audio or document, from the file name's extension.
pub fn kind_for_name(name: &str) -> MediaKind {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" => MediaKind::Image,
        "mp4" | "mov" | "webm" | "3gp" => MediaKind::Video,
        "mp3" | "m4a" | "ogg" | "opus" | "wav" | "aac" => MediaKind::Audio,
        _ => MediaKind::Document,
    }
}

fn file_attachment(att: &MsgAttachment) -> Option<Attachment> {
    let url = att.content_url.as_deref()?;
    let open_url = chatcli_core::open::web_url(url)?;
    let name = att.name.as_deref().map(one_line).filter(|n| !n.is_empty());
    Some(Attachment {
        kind: name.as_deref().map_or(MediaKind::Document, kind_for_name),
        name,
        mime: None,
        size: None,
        fetch_ref: FetchSpec::File {
            url: open_url.clone(),
        }
        .to_ref(),
        open_url: Some(open_url),
    })
}

/// None = the message has no usable timestamp, so it cannot be placed in the history.
pub fn map_message(m: &Message, chat_id: &str, me_id: &str) -> Option<ChatMessage> {
    let ts = m.created_date_time.as_deref().and_then(parse_iso_ms)?;
    let sender_id = m
        .from
        .as_ref()
        .and_then(|f| f.user.as_ref().or(f.application.as_ref()))
        .and_then(|i| i.id.clone())
        .unwrap_or_default();
    let deleted = m.deleted_date_time.is_some();
    let mut out = ChatMessage {
        network: Network::Teams,
        chat_id: chat_id.to_string(),
        id: m.id.clone(),
        ts,
        sender: m.sender().to_string(),
        from_me: user_id(&m.from) == Some(me_id),
        state: if deleted {
            MessageState::Deleted
        } else {
            MessageState::Sent
        },
        sender_id,
        ..ChatMessage::blank(Network::Teams)
    };
    // A deleted message keeps nothing: no text, files, quote or reactions.
    if deleted {
        return Some(out);
    }
    if m.message_type.as_deref().is_some_and(|t| t != "message") {
        out.text = "[system event]".to_string();
        return Some(out);
    }
    let references: Vec<&MsgAttachment> = m
        .attachments
        .iter()
        .filter(|a| a.content_type.as_deref() == Some("messageReference"))
        .collect();
    let drop: Vec<&str> = references.iter().filter_map(|a| a.id.as_deref()).collect();
    let parsed = match &m.body {
        Some(b) if b.content_type.as_deref() == Some("html") => {
            crate::html::parse(b.content.as_deref().unwrap_or_default(), &drop)
        }
        Some(b) => crate::html::Parsed {
            text: one_line(b.content.as_deref().unwrap_or_default()),
            ..Default::default()
        },
        None => crate::html::Parsed::default(),
    };
    out.text = parsed.text;
    out.edited = m.last_edited_date_time.is_some();
    out.reply_to = references.first().map(|a| quote_from(a, me_id));
    out.attachments = parsed
        .hosted
        .iter()
        .map(|hosted_id| Attachment {
            kind: MediaKind::Image,
            name: None,
            mime: None,
            size: None,
            fetch_ref: FetchSpec::Hosted {
                chat_id: chat_id.to_string(),
                message_id: m.id.clone(),
                hosted_id: hosted_id.clone(),
            }
            .to_ref(),
            open_url: None,
        })
        .collect();
    out.attachments.extend(
        m.attachments
            .iter()
            .filter(|a| a.content_type.as_deref() == Some("reference"))
            .filter_map(file_attachment),
    );
    // Only mentions of people, and only with the name the body shows ("@" + that name).
    for mention in &m.mentions {
        let Some(uid) = mention
            .mentioned
            .as_ref()
            .and_then(|x| x.user.as_ref())
            .and_then(|u| u.id.clone())
        else {
            continue;
        };
        let Some(name) = mention
            .id
            .as_ref()
            .and_then(id_text)
            .and_then(|id| parsed.at.iter().find(|(k, _)| *k == id))
            .map(|(_, name)| name.clone())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let person = PersonRef { id: uid, name };
        if !out.mentions.contains(&person) {
            out.mentions.push(person);
        }
    }
    out.mentions_me = out.mentions.iter().any(|p| p.id == me_id);
    Some(out)
}

/// One emoji for a reaction type: unicode as is, the six legacy names as their emoji, anything
/// else (`custom`, empty, too long to be one emoji) as `*`.
pub fn reaction_emoji(reaction_type: &str) -> String {
    let t = reaction_type.trim();
    let cp = |points: &[u32]| points.iter().filter_map(|c| char::from_u32(*c)).collect();
    match t.to_ascii_lowercase().as_str() {
        "like" => cp(&[0x1F44D]),
        "heart" => cp(&[0x2764, 0xFE0F]),
        "laugh" => cp(&[0x1F606]),
        "surprised" => cp(&[0x1F62E]),
        "sad" => cp(&[0x1F641]),
        "angry" => cp(&[0x1F620]),
        "" | "custom" => "*".to_string(),
        _ if t.chars().count() > EMOJI_MAX_CHARS => "*".to_string(),
        _ => t.to_string(),
    }
}

/// The store key of one reaction: the person plus the emoji, so a person who holds several
/// reactions (Graph allows it) counts once under each. The signed-in user is `REACTOR_ME`.
pub fn reactor_key(who: &str, emoji: &str) -> String {
    format!("{who}#{emoji}")
}

/// The full reaction list of a fetched message, ready for `Store::replace_reactions`.
pub fn reaction_rows(m: &Message, me_id: &str) -> Vec<ReactionRow> {
    let mut rows: Vec<ReactionRow> = Vec::new();
    for (i, r) in m.reactions.iter().enumerate() {
        let emoji = reaction_emoji(r.reaction_type.as_deref().unwrap_or_default());
        let uid = r
            .user
            .as_ref()
            .and_then(|u| u.user.as_ref())
            .and_then(|u| u.id.as_deref());
        let mine = uid == Some(me_id);
        let who = match uid {
            Some(_) if mine => REACTOR_ME.to_string(),
            Some(id) => id.to_string(),
            // No id: count it on its own rather than merge unknown people.
            None => format!("?{i}"),
        };
        let reactor = reactor_key(&who, &emoji);
        if rows.iter().any(|row| row.reactor == reactor) {
            continue;
        }
        rows.push(ReactionRow {
            reactor,
            emoji,
            from_me: mine,
        });
    }
    rows
}

/// True when `scope` is among the granted ones (names compare without case).
pub fn has_scope(granted: &[String], scope: &str) -> bool {
    granted.iter().any(|g| g.eq_ignore_ascii_case(scope))
}

/// What Teams can do now: the token's scopes and the user's settings decide the file actions and
/// replies; everything else needs only `Chat.ReadWrite`.
pub fn capabilities(granted: &[String], settings: &TeamsSettings) -> Capabilities {
    Capabilities {
        reply: has_scope(granted, "ChatMessage.Send"),
        react: true,
        edit: true,
        delete: true,
        mentions: true,
        send_image: true,
        send_file: settings.files_write && has_scope(granted, "Files.ReadWrite"),
        fetch_media: true,
        fetch_files: settings.files_read && has_scope(granted, "Files.Read.All"),
        load_older: true,
        typing: false,
        pin: true,
        mute: true,
        hide: true,
    }
}

/// The `tid` (tenant id) claim of a JWT access token. The signature is not checked: the value
/// is only an id for a request body, and Graph validates the token itself.
pub fn tid_from_jwt(token: &str) -> Option<String> {
    jwt_claims(token)?.get("tid")?.as_str().map(str::to_string)
}

/// The `scp` claim of a JWT access token: the delegated scopes it carries.
pub fn scopes_from_jwt(token: &str) -> Option<Vec<String>> {
    let scp = jwt_claims(token)?.get("scp")?.as_str()?.to_string();
    Some(scp.split_whitespace().map(str::to_string).collect())
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// True when the token is expired or within `margin_ms` of expiring.
pub fn needs_refresh(expires_at_ms: i64, now_ms: i64, margin_ms: i64) -> bool {
    now_ms >= expires_at_ms - margin_ms
}

/// What the store holds for a chat, for the catch-up decision.
#[derive(Debug, Clone, Default)]
pub struct Local {
    /// Time of the newest message Teams itself reported. None = nothing stored.
    pub newest_ts: Option<i64>,
    /// The stored copy of the message the chat list names as the last one, when stored.
    pub last: Option<ChatMessage>,
}

/// True when the stored copy of the chat's last message differs from the chat list's preview.
/// The preview has no edit time, so an edit shows only in its text; a quote is built from an
/// attachment the preview does not carry, so a reply is compared by its deleted flag only.
// Limit: an edit to an older message, one that is not the chat's last, does not show in the chat
// list at all; the `updated` notification is the only way to hear of it.
fn last_changed(remote: &RemoteLast, local: &ChatMessage) -> bool {
    if remote.deleted != (local.state == MessageState::Deleted) {
        return true;
    }
    !remote.deleted
        && local.state == MessageState::Sent
        && local.reply_to.is_none()
        && remote.text != local.text
}

/// Chats to fetch after a gap: those with stored history whose newest message on Teams is newer
/// than the newest stored one, or whose last message was edited or deleted while chatcli was
/// away (a creation time alone cannot show that). A chat never opened has nothing to be behind
/// (its history loads when opened). Returns at most `cap` ids and whether more were left out.
pub fn catch_up_chats(
    fresh: &[ChatSummary],
    remote: &HashMap<String, RemoteLast>,
    local: impl Fn(&str, Option<&str>) -> Local,
    cap: usize,
) -> (Vec<String>, bool) {
    let behind: Vec<String> = fresh
        .iter()
        .filter(|c| {
            let last = remote.get(&c.id);
            let stored = local(&c.id, last.map(|l| l.id.as_str()));
            let Some(newest) = stored.newest_ts else {
                return false;
            };
            let newer = c.last_ts.is_some_and(|remote_ts| remote_ts > newest);
            let changed = match (last, stored.last.as_ref()) {
                (Some(r), Some(l)) => last_changed(r, l),
                _ => false,
            };
            newer || changed
        })
        .map(|c| c.id.clone())
        .collect();
    let capped = behind.len() > cap;
    (behind.into_iter().take(cap).collect(), capped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chat(v: serde_json::Value) -> Chat {
        serde_json::from_value(v).unwrap()
    }

    fn message(v: serde_json::Value) -> Message {
        serde_json::from_value(v).unwrap()
    }

    fn summary(id: &str, ts: Option<i64>) -> ChatSummary {
        ChatSummary {
            network: Network::Teams,
            id: id.into(),
            title: id.into(),
            kind: ChatKind::Direct,
            last_ts: ts,
            last_preview: String::new(),
            unread: 0,
            ..ChatSummary::blank(Network::Teams)
        }
    }

    fn emoji(cp: u32) -> String {
        char::from_u32(cp).unwrap().to_string()
    }

    #[test]
    fn unread_rule() {
        assert_eq!(unread_for(Some(10), false, Some(5)), 1);
        assert_eq!(
            unread_for(Some(5), false, Some(5)),
            0,
            "read mark equals last message"
        );
        assert_eq!(unread_for(Some(5), false, Some(9)), 0);
        assert_eq!(unread_for(Some(10), false, None), 1, "never read");
        assert_eq!(
            unread_for(Some(10), true, Some(5)),
            0,
            "own message is never unread"
        );
        assert_eq!(unread_for(None, false, None), 0, "no messages");
    }

    #[test]
    fn live_unread_counts_only_new_messages_from_others() {
        assert_eq!(unread_after(2, false, true), 3);
        assert_eq!(unread_after(2, true, true), 2, "own message");
        assert_eq!(unread_after(2, false, false), 2, "edit or redelivery");
        assert_eq!(unread_after(u32::MAX, false, true), u32::MAX);
    }

    #[test]
    fn chat_summary_maps_title_preview_time_and_unread() {
        let c = chat(json!({
            "id": "19:a@unq.gbl.spaces", "chatType": "oneOnOne",
            "members": [
                { "displayName": "Alex Example", "userId": "me" },
                { "displayName": "Sam Sample", "userId": "sam" }
            ],
            "lastMessagePreview": {
                "createdDateTime": "2026-11-16T01:00:05.000Z", "messageType": "message",
                "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                "body": { "contentType": "html", "content": "<p>build <b>done</b>?</p>" }
            },
            "viewpoint": { "isHidden": false, "lastMessageReadDateTime": "2026-11-16T00:59:00Z" }
        }));
        let s = chat_summary(&c, "me").unwrap();
        assert_eq!(s.title, "Sam Sample");
        assert_eq!(s.kind, ChatKind::Direct);
        assert_eq!(s.last_ts, parse_iso_ms("2026-11-16T01:00:05.000Z"));
        assert_eq!(s.last_preview, "Sam Sample: build done?");
        assert_eq!(s.unread, 1);
        assert!(!s.flags.hidden);
    }

    #[test]
    fn chat_summary_own_last_message_reads_you_and_is_read() {
        let c = chat(json!({
            "id": "1", "chatType": "group", "topic": "Project team",
            "lastMessagePreview": {
                "createdDateTime": "2026-11-16T01:00:05Z", "messageType": "message",
                "from": { "user": { "id": "me", "displayName": "Alex Example" } },
                "body": { "contentType": "text", "content": "yes, shipped" }
            },
            "viewpoint": { "lastMessageReadDateTime": "0001-01-01T00:00:00Z" }
        }));
        let s = chat_summary(&c, "me").unwrap();
        assert_eq!(s.kind, ChatKind::Group);
        assert_eq!(s.last_preview, "You: yes, shipped");
        assert_eq!(s.unread, 0);
    }

    #[test]
    fn chat_summary_skips_meeting_chats_keeps_hidden_ones_flagged() {
        assert!(chat_summary(&chat(json!({ "id": "1", "chatType": "meeting" })), "me").is_none());
        assert!(chat_summary(&chat(json!({ "id": "1", "chatType": "channel" })), "me").is_none());
        let hidden =
            chat(json!({ "id": "1", "chatType": "oneOnOne", "viewpoint": { "isHidden": true } }));
        let s = chat_summary(&hidden, "me").expect("a hidden chat stays in the list");
        assert!(s.flags.hidden, "flagged so 'show hidden' can list it");
        assert!(is_hidden_chat(&hidden));
        assert!(!is_hidden_chat(&chat(
            json!({ "id": "1", "chatType": "meeting", "viewpoint": { "isHidden": true } })
        )));
        assert!(!is_hidden_chat(&chat(
            json!({ "id": "1", "chatType": "group" })
        )));
        let deleted = chat(json!({
            "id": "2", "chatType": "oneOnOne",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:05Z", "isDeleted": true,
                "from": { "user": { "id": "x", "displayName": "Sam Sample" } } }
        }));
        assert_eq!(
            chat_summary(&deleted, "me").unwrap().last_preview,
            "Sam Sample: [deleted]"
        );
        let system = chat(json!({
            "id": "3", "chatType": "group",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:05Z",
                "messageType": "systemEventMessage" }
        }));
        assert_eq!(
            chat_summary(&system, "me").unwrap().last_preview,
            "(system): [system event]"
        );
        let empty = chat(json!({ "id": "4", "chatType": "oneOnOne" }));
        let s = chat_summary(&empty, "me").unwrap();
        assert_eq!(
            (s.last_ts, s.unread, s.last_preview.as_str()),
            (None, 0, "")
        );
    }

    #[test]
    fn long_previews_are_clipped() {
        let line = preview_line("Sam Sample", false, &"x".repeat(500));
        assert_eq!(line.chars().count(), PREVIEW_MAX_CHARS);
        assert!(line.ends_with("..."));
    }

    #[test]
    fn message_mapping() {
        let m = message(json!({
            "id": "1612289765949", "createdDateTime": "2026-11-16T01:00:05.250Z",
            "messageType": "message",
            "from": { "user": { "id": "me", "displayName": "Alex Example" } },
            "body": { "contentType": "text", "content": "hello" }
        }));
        let out = map_message(&m, "19:a", "me").unwrap();
        assert!(out.from_me);
        assert_eq!(out.sender, "Alex Example");
        assert_eq!(out.sender_id, "me");
        assert_eq!(out.text, "hello");
        assert_eq!(out.state, MessageState::Sent);
        assert!(!out.edited && out.attachments.is_empty() && out.reply_to.is_none());
        assert_eq!(out.ts, parse_iso_ms("2026-11-16T01:00:05.250Z").unwrap());

        let other = message(json!({
            "id": "2", "createdDateTime": "2026-11-16T01:00:06Z", "messageType": "message",
            "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
            "body": { "contentType": "text", "content": "hi" }
        }));
        assert!(!map_message(&other, "19:a", "me").unwrap().from_me);

        let system = message(json!({
            "id": "4", "createdDateTime": "2026-11-16T01:00:08Z",
            "messageType": "systemEventMessage", "from": null,
            "body": { "contentType": "html", "content": "<systemEventMessage/>" }
        }));
        let s = map_message(&system, "19:a", "me").unwrap();
        assert_eq!(
            (s.text.as_str(), s.sender_id.as_str()),
            ("[system event]", "")
        );

        let no_time = message(json!({ "id": "5" }));
        assert!(map_message(&no_time, "19:a", "me").is_none());
    }

    #[test]
    fn a_deleted_message_keeps_no_text_files_quote_or_reactions() {
        let deleted = message(json!({
            "id": "3", "createdDateTime": "2026-11-16T01:00:07Z", "messageType": "message",
            "deletedDateTime": "2026-11-16T02:00:00Z",
            "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
            "body": { "contentType": "html", "content": "<img src=\".../hostedContents/x/$value\">secret" },
            "reactions": [{ "reactionType": "like", "user": { "user": { "id": "sam" } } }],
            "attachments": [{ "id": "1", "contentType": "reference", "contentUrl": "https://example.invalid/a.pdf", "name": "a.pdf" }]
        }));
        let d = map_message(&deleted, "19:a", "me").unwrap();
        assert_eq!(d.state, MessageState::Deleted);
        assert!(d.text.is_empty() && d.attachments.is_empty() && d.reply_to.is_none());
        assert!(d.reactions.is_empty() && d.mentions.is_empty());
    }

    #[test]
    fn an_edited_message_is_marked() {
        let m = message(json!({
            "id": "1", "createdDateTime": "2026-11-16T01:00:05Z",
            "lastEditedDateTime": "2026-11-16T01:05:00Z",
            "body": { "contentType": "text", "content": "fixed" }
        }));
        assert!(map_message(&m, "c", "me").unwrap().edited);
    }

    /// The example response of `replyWithQuote` in Microsoft Learn.
    #[test]
    fn a_reply_reads_the_quote_and_strips_its_tag() {
        let m = message(json!({
            "id": "1729091934196", "createdDateTime": "2024-10-16T15:18:54.196Z",
            "messageType": "message",
            "from": { "user": { "id": "00000000-0000-0000-0000-000000000004", "displayName": "Sam Sample" } },
            "body": { "contentType": "html",
                      "content": "<p></p>\n<attachment id=\"1728422677844\"></attachment>\ntest reply\n<p></p>" },
            "attachments": [{
                "id": "1728422677844", "contentType": "messageReference", "contentUrl": null,
                "content": "{\"messageId\":\"1728422677844\",\"messagePreview\":\"Hello World   Pat\\nSample\",\"messageSender\":{\"application\":null,\"device\":null,\"user\":{\"userIdentityType\":\"aadUser\",\"id\":\"pat-id\",\"displayName\":\"Pat Sample\"}}}",
                "name": null
            }],
            "mentions": [], "reactions": []
        }));
        let out = map_message(&m, "19:a", "me").unwrap();
        assert_eq!(out.text, "test reply");
        let q = out.reply_to.unwrap();
        assert_eq!(
            (
                q.id.as_str(),
                q.sender.as_str(),
                q.sender_id.as_str(),
                q.preview.as_str()
            ),
            (
                "1728422677844",
                "Pat Sample",
                "pat-id",
                "Hello World Pat Sample"
            )
        );
        assert!(out.attachments.is_empty(), "a quote is not an attachment");
        // A quote of the signed-in user's own message reads "You".
        let mine = message(json!({
            "id": "2", "createdDateTime": "2024-10-16T15:18:54Z",
            "body": { "contentType": "html", "content": "<attachment id=\"9\"></attachment>ok" },
            "attachments": [{ "id": "9", "contentType": "messageReference",
                "content": "{\"messageId\":\"9\",\"messagePreview\":\"x\",\"messageSender\":{\"user\":{\"id\":\"me\",\"displayName\":\"Alex Example\"}}}" }]
        }));
        assert_eq!(
            map_message(&mine, "c", "me")
                .unwrap()
                .reply_to
                .unwrap()
                .sender,
            "You"
        );
    }

    #[test]
    fn a_quote_with_unreadable_content_still_has_an_id_and_no_preview() {
        let m = message(json!({
            "id": "2", "createdDateTime": "2024-10-16T15:18:54Z",
            "body": { "contentType": "html", "content": "<attachment id=\"77\"></attachment>ok" },
            "attachments": [{ "id": "77", "contentType": "messageReference", "content": "not json" }]
        }));
        let q = map_message(&m, "c", "me").unwrap().reply_to.unwrap();
        assert_eq!(
            (q.id.as_str(), q.sender.as_str(), q.preview.as_str()),
            ("77", "", "")
        );
    }

    #[test]
    fn a_long_quote_preview_is_one_line_of_at_most_120_chars() {
        let long = "word ".repeat(100);
        let m = message(json!({
            "id": "2", "createdDateTime": "2024-10-16T15:18:54Z",
            "body": { "contentType": "html", "content": "<attachment id=\"7\"></attachment>ok" },
            "attachments": [{ "id": "7", "contentType": "messageReference",
                "content": json!({ "messageId": "7", "messagePreview": long }).to_string() }]
        }));
        let q = map_message(&m, "c", "me").unwrap().reply_to.unwrap();
        assert_eq!(q.preview.chars().count(), 120);
    }

    #[test]
    fn inline_images_and_files_each_give_one_attachment() {
        let m = message(json!({
            "id": "1612289765949", "createdDateTime": "2026-11-16T01:00:05Z",
            "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
            "body": { "contentType": "html", "content":
                "<p>look <img src=\"https://graph.microsoft.com/v1.0/chats/19:a/messages/1612289765949/hostedContents/aWQ9MQ/$value\" width=\"200\"></p><attachment id=\"11111111-1111-1111-1111-111111111111\"></attachment>" },
            "attachments": [
                { "id": "11111111-1111-1111-1111-111111111111", "contentType": "reference",
                  "contentUrl": "https://contoso.example.invalid/personal/alex/Documents/Report%20Q3.xlsx",
                  "name": "Report Q3.xlsx" },
                { "id": "x", "contentType": "application/vnd.microsoft.card.adaptive", "content": "{}" }
            ]
        }));
        let out = map_message(&m, "19:a", "me").unwrap();
        assert_eq!(out.text, "look [image] [attachment]");
        assert_eq!(
            out.attachments.len(),
            2,
            "one picture, one file, the card ignored"
        );
        let (img, file) = (&out.attachments[0], &out.attachments[1]);
        assert_eq!(img.kind, MediaKind::Image);
        assert_eq!(
            FetchSpec::from_ref(&img.fetch_ref),
            Some(FetchSpec::Hosted {
                chat_id: "19:a".into(),
                message_id: "1612289765949".into(),
                hosted_id: "aWQ9MQ".into()
            })
        );
        assert!(img.open_url.is_none());
        assert_eq!(file.kind, MediaKind::Document);
        assert_eq!(file.name.as_deref(), Some("Report Q3.xlsx"));
        assert_eq!(
            file.open_url.as_deref(),
            Some("https://contoso.example.invalid/personal/alex/Documents/Report%20Q3.xlsx")
        );
        assert_eq!(
            FetchSpec::from_ref(&file.fetch_ref),
            Some(FetchSpec::File {
                url: file.open_url.clone().unwrap()
            })
        );
        // No bytes and no token anywhere in what is stored.
        assert!(
            !format!("{img:?}").contains("aWQ9MQ"),
            "Debug never prints the ref"
        );
    }

    #[test]
    fn a_file_with_an_unsafe_link_gets_no_attachment() {
        let m = message(json!({
            "id": "1", "createdDateTime": "2026-11-16T01:00:05Z",
            "body": { "contentType": "html", "content": "<attachment id=\"a\"></attachment>" },
            "attachments": [
                { "id": "a", "contentType": "reference", "contentUrl": "file:///c:/x.exe", "name": "x.exe" },
                { "id": "b", "contentType": "reference", "contentUrl": null, "name": "y" }
            ]
        }));
        assert!(map_message(&m, "c", "me").unwrap().attachments.is_empty());
    }

    #[test]
    fn media_kind_follows_the_file_extension() {
        assert_eq!(kind_for_name("a.PNG"), MediaKind::Image);
        assert_eq!(kind_for_name("a.mp4"), MediaKind::Video);
        assert_eq!(kind_for_name("a.m4a"), MediaKind::Audio);
        assert_eq!(kind_for_name("a.docx"), MediaKind::Document);
        assert_eq!(kind_for_name("noext"), MediaKind::Document);
    }

    #[test]
    fn fetch_refs_round_trip_and_reject_junk() {
        for spec in [
            FetchSpec::File {
                url: "https://example.invalid/a".into(),
            },
            FetchSpec::Hosted {
                chat_id: "c".into(),
                message_id: "m".into(),
                hosted_id: "h".into(),
            },
        ] {
            assert_eq!(FetchSpec::from_ref(&spec.to_ref()), Some(spec));
        }
        assert_eq!(FetchSpec::from_ref(&FetchRef::new("nope")), None);
        assert_eq!(
            FetchSpec::from_ref(&FetchRef::new(r#"{"k":"other"}"#)),
            None
        );
        assert_eq!(
            FetchSpec::from_ref(&FetchRef::new(r#"{"k":"hosted","c":"c"}"#)),
            None
        );
    }

    #[test]
    fn mentions_use_the_body_name_and_flag_the_signed_in_user() {
        let m = message(json!({
            "id": "1", "createdDateTime": "2026-11-16T01:00:05Z",
            "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
            "body": { "contentType": "html", "content":
                "<p>hi <at id=\"0\">Alex Example</at> and <at id=\"1\">Jordan Placeholder</at> and <at id=\"2\">Everyone</at></p>" },
            "mentions": [
                { "id": 0, "mentionText": "Alex Example",
                  "mentioned": { "user": { "id": "me", "displayName": "Alex Example", "userIdentityType": "aadUser" } } },
                { "id": 1, "mentionText": "Jordan",
                  "mentioned": { "user": { "id": "jp", "displayName": "Jordan Placeholder" } } },
                { "id": 2, "mentionText": "Everyone", "mentioned": { "conversation": { "id": "19:a" } } }
            ]
        }));
        let out = map_message(&m, "19:a", "me").unwrap();
        assert_eq!(
            out.text,
            "hi @Alex Example and @Jordan Placeholder and @Everyone"
        );
        assert_eq!(
            out.mentions,
            vec![
                PersonRef {
                    id: "me".into(),
                    name: "Alex Example".into()
                },
                PersonRef {
                    id: "jp".into(),
                    name: "Jordan Placeholder".into()
                },
            ],
            "the group mention stays in the text but is not a person"
        );
        assert!(out.mentions_me);
        for mention in &out.mentions {
            assert!(out.text.contains(&format!("@{}", mention.name)));
        }
        let none = message(json!({
            "id": "2", "createdDateTime": "2026-11-16T01:00:05Z",
            "body": { "contentType": "html", "content": "<at id=\"0\">Sam Sample</at>" },
            "mentions": [{ "id": 0, "mentioned": { "user": { "id": "sam" } } }]
        }));
        let out = map_message(&none, "c", "me").unwrap();
        assert!(!out.mentions_me && out.mentions.len() == 1);
    }

    #[test]
    fn reactions_map_legacy_names_unicode_and_custom() {
        assert_eq!(reaction_emoji("like"), emoji(0x1F44D));
        assert_eq!(reaction_emoji("LIKE"), emoji(0x1F44D));
        assert_eq!(
            reaction_emoji("heart"),
            format!("{}{}", emoji(0x2764), emoji(0xFE0F))
        );
        assert_eq!(reaction_emoji("laugh"), emoji(0x1F606));
        assert_eq!(reaction_emoji("surprised"), emoji(0x1F62E));
        assert_eq!(reaction_emoji("sad"), emoji(0x1F641));
        assert_eq!(reaction_emoji("angry"), emoji(0x1F620));
        assert_eq!(reaction_emoji(&emoji(0x1F680)), emoji(0x1F680));
        assert_eq!(reaction_emoji("custom"), "*");
        assert_eq!(reaction_emoji(""), "*");
        assert_eq!(reaction_emoji(&"x".repeat(40)), "*");
    }

    #[test]
    fn reaction_rows_key_by_person_and_emoji() {
        let m = message(json!({
            "id": "1", "createdDateTime": "2026-11-16T01:00:05Z",
            "reactions": [
                { "reactionType": "like", "user": { "user": { "id": "sam" } } },
                { "reactionType": "like", "user": { "user": { "id": "me" } } },
                { "reactionType": "heart", "user": { "user": { "id": "me" } } },
                { "reactionType": "like", "user": { "user": { "id": "sam" } } },
                { "reactionType": "custom", "user": { "user": null } },
                { "reactionType": "custom", "user": { "user": null } }
            ]
        }));
        let rows = reaction_rows(&m, "me");
        let keys: Vec<&str> = rows.iter().map(|r| r.reactor.as_str()).collect();
        let like = emoji(0x1F44D);
        assert_eq!(
            keys.len(),
            5,
            "the repeat by sam is dropped, the two unknowns both count"
        );
        assert!(keys.contains(&format!("sam#{like}").as_str()));
        assert!(keys.contains(&format!("@me#{like}").as_str()));
        assert_eq!(
            rows.iter().filter(|r| r.from_me).count(),
            2,
            "my two reactions"
        );
        assert!(rows
            .iter()
            .all(|r| r.from_me == r.reactor.starts_with("@me#")));
    }

    #[test]
    fn capabilities_follow_scopes_and_settings() {
        let granted = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let base = granted("offline_access User.Read Chat.ReadWrite ChatMessage.Send");
        let off = TeamsSettings::default();
        let c = capabilities(&base, &off);
        assert!(c.reply && c.react && c.edit && c.delete && c.mentions && c.send_image);
        assert!(c.fetch_media && c.load_older && c.pin && c.mute && c.hide);
        assert!(!c.send_file && !c.fetch_files && !c.typing);
        // The scope alone is not enough: the setting is the user's opt-in.
        let both = granted("Chat.ReadWrite ChatMessage.Send Files.Read.All Files.ReadWrite");
        let c = capabilities(&both, &off);
        assert!(!c.send_file && !c.fetch_files);
        let on = TeamsSettings {
            files_read: true,
            files_write: true,
        };
        let c = capabilities(&both, &on);
        assert!(c.send_file && c.fetch_files);
        // A setting without consent gives nothing, and an old sign-in cannot reply.
        let c = capabilities(&granted("offline_access User.Read Chat.ReadWrite"), &on);
        assert!(!c.send_file && !c.fetch_files && !c.reply);
        assert!(!capabilities(&[], &on).reply);
        assert!(has_scope(&granted("chatmessage.send"), "ChatMessage.Send"));
    }

    #[test]
    fn jwt_tid_and_scopes_are_read_from_the_payload() {
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let token = format!(
            "{}.{}.sig",
            enc(r#"{"alg":"RS256"}"#),
            enc(
                r#"{"tid":"00000000-0000-0000-0000-000000000000","oid":"x","scp":"User.Read Chat.ReadWrite ChatMessage.Send"}"#
            )
        );
        assert_eq!(
            tid_from_jwt(&token).as_deref(),
            Some("00000000-0000-0000-0000-000000000000")
        );
        assert_eq!(
            scopes_from_jwt(&token).unwrap(),
            ["User.Read", "Chat.ReadWrite", "ChatMessage.Send"]
        );
        assert_eq!(tid_from_jwt("not-a-jwt"), None);
        assert_eq!(scopes_from_jwt("not-a-jwt"), None);
        assert_eq!(
            tid_from_jwt(&format!("a.{}.c", enc("{}"))),
            None,
            "no tid claim"
        );
        assert_eq!(scopes_from_jwt(&format!("a.{}.c", enc("{}"))), None);
        assert_eq!(tid_from_jwt("a.!!!.c"), None, "payload is not base64");
        assert_eq!(tid_from_jwt(&format!("a.{}.c", enc("not json"))), None);
    }

    #[test]
    fn token_refresh_is_due_a_minute_early() {
        let expires = 1_000_000;
        assert!(!needs_refresh(expires, expires - 61_000, TOKEN_MARGIN_MS));
        assert!(needs_refresh(expires, expires - 60_000, TOKEN_MARGIN_MS));
        assert!(needs_refresh(expires, expires + 1, TOKEN_MARGIN_MS));
    }

    fn last(id: &str, deleted: bool, text: &str) -> RemoteLast {
        RemoteLast {
            id: id.into(),
            deleted,
            text: text.into(),
        }
    }

    fn stored(text: &str, state: MessageState) -> ChatMessage {
        ChatMessage {
            id: "m".into(),
            text: text.into(),
            state,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    #[test]
    fn catch_up_picks_only_chats_behind_with_stored_history() {
        let fresh = [
            summary("behind", Some(200)),
            summary("current", Some(100)),
            summary("never-opened", Some(300)),
            summary("empty", None),
        ];
        let newest = |id: &str, _: Option<&str>| Local {
            newest_ts: match id {
                "behind" | "current" => Some(100),
                "empty" => Some(50),
                _ => None,
            },
            last: None,
        };
        assert_eq!(
            catch_up_chats(&fresh, &HashMap::new(), newest, 10),
            (vec!["behind".to_string()], false)
        );
        let many: Vec<_> = (0..5)
            .map(|i| summary(&format!("c{i}"), Some(10)))
            .collect();
        let (ids, capped) = catch_up_chats(
            &many,
            &HashMap::new(),
            |_, _| Local {
                newest_ts: Some(1),
                last: None,
            },
            3,
        );
        assert_eq!((ids.len(), capped), (3, true));
    }

    /// CodeRabbit 4: the newest creation time is unchanged when the last message is edited or
    /// deleted while chatcli is away, so the time alone leaves the cached copy stale.
    #[test]
    fn catch_up_also_picks_a_chat_whose_last_message_was_edited_or_deleted() {
        let fresh = [
            summary("edited", Some(100)),
            summary("deleted", Some(100)),
            summary("same", Some(100)),
            summary("reply", Some(100)),
            summary("unstored-last", Some(100)),
        ];
        let remote: HashMap<String, RemoteLast> = [
            ("edited", last("m", false, "new words")),
            ("deleted", last("m", true, "[deleted]")),
            ("same", last("m", false, "same words")),
            ("reply", last("m", false, "test reply")),
            ("unstored-last", last("m", false, "x")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let local = |id: &str, _: Option<&str>| Local {
            newest_ts: Some(100),
            last: match id {
                "edited" => Some(stored("old words", MessageState::Sent)),
                "deleted" => Some(stored("old words", MessageState::Sent)),
                "same" => Some(stored("same words", MessageState::Sent)),
                // A quote is built from an attachment the preview lacks: text is not compared.
                "reply" => Some(ChatMessage {
                    reply_to: Some(QuotedRef {
                        id: "q".into(),
                        sender: String::new(),
                        sender_id: String::new(),
                        preview: String::new(),
                    }),
                    ..stored("different", MessageState::Sent)
                }),
                _ => None,
            },
        };
        let (ids, capped) = catch_up_chats(&fresh, &remote, local, 10);
        assert_eq!(ids, ["edited", "deleted"]);
        assert!(!capped);
        // Already deleted here and there: nothing to fetch.
        let remote_deleted: HashMap<String, RemoteLast> =
            [("a".to_string(), last("m", true, "[deleted]"))].into();
        let (ids, _) = catch_up_chats(
            &[summary("a", Some(100))],
            &remote_deleted,
            |_, _| Local {
                newest_ts: Some(100),
                last: Some(stored("", MessageState::Deleted)),
            },
            10,
        );
        assert!(ids.is_empty());
    }

    #[test]
    fn remote_last_reads_id_text_and_deleted_flag() {
        let c = chat(json!({
            "id": "1", "chatType": "oneOnOne",
            "lastMessagePreview": { "id": "m9", "createdDateTime": "2026-11-16T01:00:05Z",
                "body": { "contentType": "html", "content": "<p>hi <at id=\"0\">Sam</at></p>" } }
        }));
        assert_eq!(remote_last(&c), Some(last("m9", false, "hi @Sam")));
        assert_eq!(
            remote_last(&chat(json!({ "id": "2", "chatType": "oneOnOne" }))),
            None
        );
        let no_id = chat(json!({ "id": "3", "chatType": "oneOnOne",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:05Z" } }));
        assert_eq!(remote_last(&no_id), None);
    }
}
