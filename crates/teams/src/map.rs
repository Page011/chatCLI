//! Pure mapping from Graph's shapes to chatcli's model, plus the small rules around it
//! (unread, token expiry, catch-up gaps). No network, no store: everything here is unit tested.

use base64::Engine;

use chatcli_core::model::{
    parse_iso_ms, ChatKind, ChatMessage, ChatSummary, MessageState, Network,
};

use crate::graph::{Chat, Message};

/// The sidebar shows one line; this keeps a long message from filling the store row.
const PREVIEW_MAX_CHARS: usize = 120;

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

/// None = the chat is not shown: hidden by the user, or a meeting/other chat type.
pub fn chat_summary(chat: &Chat, me_id: &str) -> Option<ChatSummary> {
    let kind = match chat.chat_type.as_str() {
        "oneOnOne" => ChatKind::Direct,
        "group" => ChatKind::Group,
        _ => return None,
    };
    if chat.viewpoint.as_ref().and_then(|v| v.is_hidden) == Some(true) {
        return None;
    }
    let preview = chat.last_message_preview.as_ref();
    let last_ts = preview
        .and_then(|p| p.created_date_time.as_deref())
        .and_then(parse_iso_ms);
    let from_me = preview.is_some_and(|p| user_id(&p.from) == Some(me_id));
    let last_preview = preview
        .map(|p| {
            let text = if p.is_deleted == Some(true) {
                "[deleted]".to_string()
            } else if p.message_type.as_deref().is_some_and(|t| t != "message") {
                "[system event]".to_string()
            } else {
                p.body.as_ref().map(|b| b.text()).unwrap_or_default()
            };
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
    })
}

/// None = the message has no usable timestamp, so it cannot be placed in the history.
pub fn map_message(m: &Message, chat_id: &str, me_id: &str) -> Option<ChatMessage> {
    let ts = m.created_date_time.as_deref().and_then(parse_iso_ms)?;
    Some(ChatMessage {
        network: Network::Teams,
        chat_id: chat_id.to_string(),
        id: m.id.clone(),
        ts,
        sender: m.sender().to_string(),
        from_me: user_id(&m.from) == Some(me_id),
        text: m.text(),
        state: if m.deleted_date_time.is_some() {
            MessageState::Deleted
        } else {
            MessageState::Sent
        },
    })
}

/// The `tid` (tenant id) claim of a JWT access token. The signature is not checked: the value
/// is only an id for a request body, and Graph validates the token itself.
pub fn tid_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("tid")?.as_str().map(str::to_string)
}

/// True when the token is expired or within `margin_ms` of expiring.
pub fn needs_refresh(expires_at_ms: i64, now_ms: i64, margin_ms: i64) -> bool {
    now_ms >= expires_at_ms - margin_ms
}

/// Chats whose newest message on Teams is newer than the newest stored one. Only chats with
/// some stored history count: a chat never opened has nothing to be behind (its history loads
/// when opened). Returns at most `cap` ids and whether more were left out.
pub fn catch_up_chats(
    fresh: &[ChatSummary],
    newest_stored: impl Fn(&str) -> Option<i64>,
    cap: usize,
) -> (Vec<String>, bool) {
    let behind: Vec<String> = fresh
        .iter()
        .filter(|c| match (c.last_ts, newest_stored(&c.id)) {
            (Some(remote), Some(local)) => remote > local,
            _ => false,
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
        }
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
    fn chat_summary_skips_hidden_and_meeting_chats_and_marks_deleted_and_system() {
        assert!(chat_summary(&chat(json!({ "id": "1", "chatType": "meeting" })), "me").is_none());
        assert!(chat_summary(&chat(json!({ "id": "1", "chatType": "channel" })), "me").is_none());
        let hidden =
            chat(json!({ "id": "1", "chatType": "oneOnOne", "viewpoint": { "isHidden": true } }));
        assert!(chat_summary(&hidden, "me").is_none());
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
        assert_eq!(out.text, "hello");
        assert_eq!(out.state, MessageState::Sent);
        assert_eq!(out.ts, parse_iso_ms("2026-11-16T01:00:05.250Z").unwrap());

        let other = message(json!({
            "id": "2", "createdDateTime": "2026-11-16T01:00:06Z", "messageType": "message",
            "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
            "body": { "contentType": "text", "content": "hi" }
        }));
        assert!(!map_message(&other, "19:a", "me").unwrap().from_me);

        let deleted = message(json!({
            "id": "3", "createdDateTime": "2026-11-16T01:00:07Z", "messageType": "message",
            "deletedDateTime": "2026-11-16T02:00:00Z",
            "body": { "contentType": "text", "content": "secret" }
        }));
        let d = map_message(&deleted, "19:a", "me").unwrap();
        assert_eq!(
            (d.state, d.text.as_str()),
            (MessageState::Deleted, "[deleted]")
        );

        let system = message(json!({
            "id": "4", "createdDateTime": "2026-11-16T01:00:08Z",
            "messageType": "systemEventMessage", "from": null,
            "body": { "contentType": "html", "content": "<systemEventMessage/>" }
        }));
        assert_eq!(
            map_message(&system, "19:a", "me").unwrap().text,
            "[system event]"
        );

        let no_time = message(json!({ "id": "5" }));
        assert!(map_message(&no_time, "19:a", "me").is_none());
    }

    #[test]
    fn jwt_tid_is_read_from_the_payload() {
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let token = format!(
            "{}.{}.sig",
            enc(r#"{"alg":"RS256"}"#),
            enc(r#"{"tid":"00000000-0000-0000-0000-000000000000","oid":"x"}"#)
        );
        assert_eq!(
            tid_from_jwt(&token).as_deref(),
            Some("00000000-0000-0000-0000-000000000000")
        );
        assert_eq!(tid_from_jwt("not-a-jwt"), None);
        assert_eq!(
            tid_from_jwt(&format!("a.{}.c", enc("{}"))),
            None,
            "no tid claim"
        );
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

    #[test]
    fn catch_up_picks_only_chats_behind_with_stored_history() {
        let fresh = [
            summary("behind", Some(200)),
            summary("current", Some(100)),
            summary("never-opened", Some(300)),
            summary("empty", None),
        ];
        let newest = |id: &str| match id {
            "behind" => Some(100),
            "current" => Some(100),
            "empty" => Some(50),
            _ => None,
        };
        assert_eq!(
            catch_up_chats(&fresh, newest, 10),
            (vec!["behind".to_string()], false)
        );
        let many: Vec<_> = (0..5)
            .map(|i| summary(&format!("c{i}"), Some(10)))
            .collect();
        let (ids, capped) = catch_up_chats(&many, |_| Some(1), 3);
        assert_eq!((ids.len(), capped), (3, true));
    }
}
