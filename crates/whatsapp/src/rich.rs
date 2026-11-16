//! Pure mapping for the 0.2 message features: attachments and their fetch refs, quotes,
//! mentions, and the builders for outgoing messages. No I/O, so all of it is unit-tested. The
//! backend (`backend.rs`, `ops.rs`) feeds it and does the storing and sending.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chatcli_core::model::{Attachment, FetchRef, MediaKind, PersonRef, QuotedRef};
use whatsapp_rust::prelude::*;
use whatsapp_rust::serde_json::{self, json, Value};
use whatsapp_rust::wacore_binary::JidExt;

use crate::map::{content, parse_jid, with_caption, Content, Names, OWN_LABEL};

/// A quote preview is one line of at most this many characters (the model's rule).
pub const QUOTE_PREVIEW_CHARS: usize = 120;

// ---------------------------------------------------------------------------------------------
// Fetch refs: how an attachment is downloaded later
// ---------------------------------------------------------------------------------------------

/// Everything needed to download and decrypt one WhatsApp attachment. The media key lives here,
/// so `Debug` never prints it and the stored `FetchRef` string is never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Fetch {
    pub kind: MediaKind,
    pub direct_path: String,
    pub media_key: Vec<u8>,
    pub enc_sha256: Vec<u8>,
    /// Hash of the decrypted file; empty when the sender gave none (then it is not checked).
    pub sha256: Vec<u8>,
    pub length: u64,
}

impl std::fmt::Debug for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fetch(<redacted>)")
    }
}

/// Bumped when the JSON layout changes; an unknown version simply does not parse.
const REF_VERSION: u64 = 1;

pub fn fetch_ref(f: &Fetch) -> FetchRef {
    let v = json!({
        "v": REF_VERSION,
        "kind": f.kind.key(),
        "path": f.direct_path,
        "key": B64.encode(&f.media_key),
        "enc": B64.encode(&f.enc_sha256),
        "sha": B64.encode(&f.sha256),
        "len": f.length,
    });
    FetchRef::new(v.to_string())
}

/// None for an empty or damaged ref, an unknown version, or a kind that cannot be downloaded.
pub fn parse_fetch_ref(r: &FetchRef) -> Option<Fetch> {
    let v: Value = serde_json::from_str(r.as_str()).ok()?;
    if v.get("v")?.as_u64()? != REF_VERSION {
        return None;
    }
    let kind = MediaKind::from_key(v.get("kind")?.as_str()?);
    if matches!(kind, MediaKind::Other) {
        return None;
    }
    let bytes = |key: &str| B64.decode(v.get(key)?.as_str()?).ok();
    let direct_path = v.get("path")?.as_str()?.to_string();
    let media_key = bytes("key")?;
    if direct_path.is_empty() || media_key.is_empty() {
        return None;
    }
    Some(Fetch {
        kind,
        direct_path,
        media_key,
        enc_sha256: bytes("enc")?,
        sha256: bytes("sha")?,
        length: v.get("len")?.as_u64()?,
    })
}

// ---------------------------------------------------------------------------------------------
// Reading a message
// ---------------------------------------------------------------------------------------------

/// What a message carries besides its text.
#[derive(Debug, Default, PartialEq)]
pub struct Details {
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<QuotedRef>,
    /// Mentioned JIDs (device suffix removed), in the order the sender listed them.
    pub mentioned: Vec<String>,
}

pub fn details(msg: &wa::Message, chat: &Jid, names: &Names) -> Details {
    let base = msg.get_base_message();
    let ctx = context_of(base);
    Details {
        attachments: attachments(base),
        reply_to: ctx.and_then(|c| quoted(c, chat, names)),
        mentioned: ctx.map(mentioned_jids).unwrap_or_default(),
    }
}

/// The `ContextInfo` of the first body type that can carry one (quote and mentions live here).
pub fn context_of(base: &wa::Message) -> Option<&wa::ContextInfo> {
    base.extended_text_message
        .as_option()
        .and_then(|m| m.context_info.as_option())
        .or_else(|| {
            base.image_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.video_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.document_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.audio_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
}

pub fn mentioned_jids(ctx: &wa::ContextInfo) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in &ctx.mentioned_jid {
        if let Some(j) = parse_jid(raw) {
            let s = j.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// One line, whitespace collapsed, at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// The message a reply quotes. The sender id is the quoted author's JID (in a direct chat with
/// none given, the chat's own address). An unreadable quote keeps its id with an empty preview:
/// the backend fills the preview from the store, else the TUI shows a placeholder.
pub fn quoted(ctx: &wa::ContextInfo, chat: &Jid, names: &Names) -> Option<QuotedRef> {
    let id = ctx.stanza_id.clone().filter(|s| !s.is_empty())?;
    let sender_id = ctx
        .participant
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| (!chat.is_group()).then(|| chat.to_string()))
        .unwrap_or_default();
    let sender = match parse_jid(&sender_id) {
        Some(j) if names.is_own(&j.user) => OWN_LABEL.to_string(),
        Some(j) => names.label_with(&j, None),
        None => String::new(),
    };
    let preview = ctx
        .quoted_message
        .as_option()
        .map(|q| match content(q) {
            Content::Text(t) => one_line(&t, QUOTE_PREVIEW_CHARS),
            _ => String::new(),
        })
        .unwrap_or_default();
    Some(QuotedRef {
        id,
        sender,
        sender_id,
        preview,
    })
}

/// Attachments of a message in network order. A media message whose download details are
/// missing still gets an entry (so the chip shows) with an empty fetch ref.
pub fn attachments(base: &wa::Message) -> Vec<Attachment> {
    let mut out = Vec::new();
    if let Some(m) = base.image_message.as_option() {
        out.push(attachment(
            MediaKind::Image,
            None,
            m.mimetype.as_deref(),
            m.file_length,
            Keys {
                path: m.direct_path.as_deref(),
                key: m.media_key.as_deref(),
                enc: m.file_enc_sha256.as_deref(),
                sha: m.file_sha256.as_deref(),
            },
        ));
    }
    let video = base
        .video_message
        .as_option()
        .or_else(|| base.ptv_message.as_option());
    if let Some(m) = video {
        out.push(attachment(
            MediaKind::Video,
            None,
            m.mimetype.as_deref(),
            m.file_length,
            Keys {
                path: m.direct_path.as_deref(),
                key: m.media_key.as_deref(),
                enc: m.file_enc_sha256.as_deref(),
                sha: m.file_sha256.as_deref(),
            },
        ));
    }
    if let Some(m) = base.audio_message.as_option() {
        out.push(attachment(
            MediaKind::Audio,
            None,
            m.mimetype.as_deref(),
            m.file_length,
            Keys {
                path: m.direct_path.as_deref(),
                key: m.media_key.as_deref(),
                enc: m.file_enc_sha256.as_deref(),
                sha: m.file_sha256.as_deref(),
            },
        ));
    }
    if let Some(m) = base.document_message.as_option() {
        let name = m
            .file_name
            .as_deref()
            .or(m.title.as_deref())
            .filter(|n| !n.is_empty());
        out.push(attachment(
            MediaKind::Document,
            name,
            m.mimetype.as_deref(),
            m.file_length,
            Keys {
                path: m.direct_path.as_deref(),
                key: m.media_key.as_deref(),
                enc: m.file_enc_sha256.as_deref(),
                sha: m.file_sha256.as_deref(),
            },
        ));
    }
    out
}

struct Keys<'a> {
    path: Option<&'a str>,
    key: Option<&'a [u8]>,
    enc: Option<&'a [u8]>,
    sha: Option<&'a [u8]>,
}

fn attachment(
    kind: MediaKind,
    name: Option<&str>,
    mime: Option<&str>,
    size: Option<u64>,
    k: Keys<'_>,
) -> Attachment {
    let fetch = match (k.path, k.key) {
        (Some(path), Some(key)) if !path.is_empty() && !key.is_empty() => fetch_ref(&Fetch {
            kind,
            direct_path: path.to_string(),
            media_key: key.to_vec(),
            enc_sha256: k.enc.unwrap_or_default().to_vec(),
            sha256: k.sha.unwrap_or_default().to_vec(),
            length: size.unwrap_or(0),
        }),
        _ => FetchRef::default(),
    };
    Attachment {
        kind,
        name: name.map(str::to_string),
        mime: mime.filter(|m| !m.is_empty()).map(str::to_string),
        size,
        fetch_ref: fetch,
        open_url: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Mentions
// ---------------------------------------------------------------------------------------------

/// Incoming: the text carries `@<number>` for each mention. Rewrites each to `@<name>` (the
/// model's form) and returns the people whose token is in the text, in text order of the list.
/// A number that is not a mention is left alone, and so is a longer number that merely starts
/// with a mentioned one.
pub fn normalise_mentions(
    text: &str,
    mentioned: &[String],
    names: &Names,
) -> (String, Vec<PersonRef>) {
    let people: Vec<(String, PersonRef)> = mentioned
        .iter()
        .filter_map(|id| {
            let jid = parse_jid(id)?;
            let name = if names.is_own(&jid.user) {
                OWN_LABEL.to_string()
            } else {
                names.label_with(&jid, None)
            };
            Some((
                jid.user.to_string(),
                PersonRef {
                    id: id.clone(),
                    name,
                },
            ))
        })
        .collect();
    if people.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut found: Vec<usize> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        match people
            .iter()
            .position(|(u, _)| digits > 0 && *u == after[..digits])
        {
            Some(i) => {
                out.push('@');
                out.push_str(&people[i].1.name);
                if !found.contains(&i) {
                    found.push(i);
                }
            }
            None => {
                out.push('@');
                out.push_str(&after[..digits]);
            }
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    found.sort_unstable();
    let persons = found.into_iter().map(|i| people[i].1.clone()).collect();
    (out, persons)
}

/// True when one of the mentioned JIDs is one of our own addresses.
pub fn mentions_me(mentioned: &[String], names: &Names) -> bool {
    mentioned
        .iter()
        .filter_map(|id| parse_jid(id))
        .any(|j| names.is_own(&j.user))
}

/// Outgoing mentions applied to the typed text.
#[derive(Debug, PartialEq)]
pub struct AppliedMentions {
    /// The text as sent: each used `@name` replaced by `@<user part of the id>`.
    pub wire: String,
    /// The `mentioned_jid` list, without duplicates.
    pub jids: Vec<String>,
    /// The mentions whose token was found, for the stored message.
    pub applied: Vec<PersonRef>,
}

/// Replaces the first not-yet-used `@name` of each mention, in list order, with the network
/// form. A name only matches at the end of a word, so `@Al` never eats the start of `@Alex`. A
/// mention whose token is missing from the text is dropped; the text goes out as typed.
pub fn apply_mentions(text: &str, mentions: &[PersonRef]) -> AppliedMentions {
    let mut taken: Vec<(usize, usize, usize)> = Vec::new();
    for (i, m) in mentions.iter().enumerate() {
        if m.name.is_empty() || m.id.is_empty() {
            continue;
        }
        let token = format!("@{}", m.name);
        let mut from = 0;
        while let Some(pos) = text[from..].find(&token) {
            let start = from + pos;
            let end = start + token.len();
            let word_end = text[end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            let overlaps = taken.iter().any(|&(s, e, _)| start < e && s < end);
            if word_end && !overlaps {
                taken.push((start, end, i));
                break;
            }
            from = start + 1; // '@' is one byte, so this stays on a character boundary
        }
    }
    taken.sort_unstable();
    let mut wire = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end, i) in &taken {
        wire.push_str(&text[at..start]);
        wire.push('@');
        wire.push_str(&crate::map::user_of(&mentions[i].id));
        at = end;
    }
    wire.push_str(&text[at..]);
    let mut jids: Vec<String> = Vec::new();
    let mut applied: Vec<PersonRef> = Vec::new();
    let mut used: Vec<usize> = taken.iter().map(|t| t.2).collect();
    used.sort_unstable();
    for i in used {
        if !jids.contains(&mentions[i].id) {
            jids.push(mentions[i].id.clone());
        }
        applied.push(mentions[i].clone());
    }
    AppliedMentions {
        wire,
        jids,
        applied,
    }
}

// ---------------------------------------------------------------------------------------------
// Building a message to send
// ---------------------------------------------------------------------------------------------

/// A quote's `ContextInfo`: the quoted id, its author and a one-line copy of its text (rebuilt
/// from the stored preview, since chatcli keeps text only). An empty preview sends the quote
/// without a copy, which the official clients show as an unavailable message.
pub fn quote_context(q: &QuotedRef, participant: &str, preview: &str) -> wa::ContextInfo {
    let quoted_message = (!preview.is_empty()).then(|| {
        Box::new(wa::Message {
            conversation: Some(preview.to_string()),
            ..Default::default()
        })
    });
    wa::ContextInfo {
        stanza_id: Some(q.id.clone()),
        participant: Some(participant.to_string()),
        quoted_message: quoted_message
            .map(MessageField::from_box)
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// A text message: plain, or with a quote and/or mentions in one `ContextInfo`.
pub fn text_message(
    text: &str,
    quote: Option<wa::ContextInfo>,
    mentions: &[PersonRef],
) -> (wa::Message, Vec<PersonRef>) {
    let applied = apply_mentions(text, mentions);
    let mut ctx = quote;
    if !applied.jids.is_empty() {
        ctx.get_or_insert_with(Default::default).mentioned_jid = applied.jids.clone();
    }
    let message = match ctx {
        Some(c) => wa::Message::text_with_context(applied.wire, c),
        None => wa::Message::text(applied.wire),
    };
    (message, applied.applied)
}

/// The CDN details of a finished upload.
#[derive(Clone)]
pub struct Uploaded {
    pub url: String,
    pub direct_path: String,
    pub media_key: [u8; 32],
    pub file_enc_sha256: [u8; 32],
    pub file_sha256: [u8; 32],
    pub file_length: u64,
    pub media_key_timestamp: i64,
}

impl std::fmt::Debug for Uploaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Uploaded(<redacted>)")
    }
}

fn caption_of(caption: &str) -> Option<String> {
    (!caption.is_empty()).then(|| caption.to_string())
}

pub fn image_message(
    up: &Uploaded,
    mime: &str,
    width: u32,
    height: u32,
    caption: &str,
) -> wa::Message {
    wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            url: Some(up.url.clone()),
            mimetype: Some(mime.to_string()),
            caption: caption_of(caption),
            file_sha256: Some(up.file_sha256.to_vec()),
            file_length: Some(up.file_length),
            height: Some(height),
            width: Some(width),
            media_key: Some(up.media_key.to_vec()),
            file_enc_sha256: Some(up.file_enc_sha256.to_vec()),
            direct_path: Some(up.direct_path.clone()),
            media_key_timestamp: Some(up.media_key_timestamp),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn document_message(up: &Uploaded, name: &str, mime: &str, caption: &str) -> wa::Message {
    wa::Message {
        document_message: MessageField::some(wa::message::DocumentMessage {
            url: Some(up.url.clone()),
            mimetype: Some(mime.to_string()),
            title: Some(name.to_string()),
            file_sha256: Some(up.file_sha256.to_vec()),
            file_length: Some(up.file_length),
            media_key: Some(up.media_key.to_vec()),
            file_name: Some(name.to_string()),
            file_enc_sha256: Some(up.file_enc_sha256.to_vec()),
            direct_path: Some(up.direct_path.clone()),
            media_key_timestamp: Some(up.media_key_timestamp),
            caption: caption_of(caption),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The attachment of a message we just sent, so the user can open their own file later.
pub fn sent_attachment(
    kind: MediaKind,
    name: Option<&str>,
    mime: &str,
    up: &Uploaded,
) -> Attachment {
    Attachment {
        kind,
        name: name.map(str::to_string),
        mime: Some(mime.to_string()),
        size: Some(up.file_length),
        fetch_ref: fetch_ref(&Fetch {
            kind,
            direct_path: up.direct_path.clone(),
            media_key: up.media_key.to_vec(),
            enc_sha256: up.file_enc_sha256.to_vec(),
            sha256: up.file_sha256.to_vec(),
            length: up.file_length,
        }),
        open_url: None,
    }
}

/// The text marker the pane shows for a media message (the same one incoming media gets).
pub fn media_marker(kind: MediaKind, name: &str, caption: &str) -> String {
    let marker = match kind {
        MediaKind::Image => "[image]".to_string(),
        _ if name.is_empty() => "[document]".to_string(),
        _ => format!("[document: {name}]"),
    };
    with_caption(&marker, Some(caption))
}

/// MIME type for a document by its file extension; unknown ones go out as plain bytes.
pub fn mime_for_name(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rtf" => "application/rtf",
        "vcf" => "text/vcard",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// The file name a recipient sees: the last path piece only, never a directory of ours.
pub fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "file".to_string())
}

// ---------------------------------------------------------------------------------------------
// Reactors
// ---------------------------------------------------------------------------------------------

/// The id one person reacts under: our own reactions share `REACTOR_ME`, everyone else is their
/// phone-number user part, so one person who shows up under a phone number and a LID still
/// counts once.
pub fn reactor_id(sender: &Jid, from_me: bool, names: &Names) -> String {
    if from_me {
        return chatcli_core::store::REACTOR_ME.to_string();
    }
    let user = sender.user.to_string();
    if sender.is_lid() {
        if let Some(pn) = names.linked(&user) {
            return pn.to_string();
        }
    }
    user
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jid(s: &str) -> Jid {
        s.parse().unwrap()
    }

    fn person(id: &str, name: &str) -> PersonRef {
        PersonRef {
            id: id.into(),
            name: name.into(),
        }
    }

    const ALEX: &str = "15550100001@s.whatsapp.net";
    const SAM: &str = "15550100002@s.whatsapp.net";

    fn fetch() -> Fetch {
        Fetch {
            kind: MediaKind::Image,
            direct_path: "/v/t62/abc".into(),
            media_key: vec![7; 32],
            enc_sha256: vec![1; 32],
            sha256: vec![2; 32],
            length: 4096,
        }
    }

    #[test]
    fn fetch_ref_round_trips_and_never_prints_the_key() {
        let f = fetch();
        let r = fetch_ref(&f);
        assert_eq!(parse_fetch_ref(&r), Some(f.clone()));
        assert_eq!(format!("{f:?}"), "Fetch(<redacted>)");
        assert_eq!(format!("{r:?}"), "FetchRef(<redacted>)");
        // The same media always gives the same ref, so the cache file name is stable.
        assert_eq!(fetch_ref(&f), r);
    }

    #[test]
    fn damaged_or_unknown_refs_do_not_parse() {
        assert_eq!(parse_fetch_ref(&FetchRef::default()), None);
        assert_eq!(parse_fetch_ref(&FetchRef::new("not json")), None);
        assert_eq!(parse_fetch_ref(&FetchRef::new(r#"{"v":2}"#)), None);
        let mut f = fetch();
        f.kind = MediaKind::Other;
        assert_eq!(parse_fetch_ref(&fetch_ref(&f)), None);
        f.kind = MediaKind::Image;
        f.media_key.clear();
        assert_eq!(parse_fetch_ref(&fetch_ref(&f)), None);
    }

    fn image(path: Option<&str>, key: Option<Vec<u8>>) -> wa::Message {
        wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                mimetype: Some("image/jpeg".into()),
                file_length: Some(2048),
                direct_path: path.map(str::to_string),
                media_key: key,
                file_enc_sha256: Some(vec![1; 32]),
                file_sha256: Some(vec![2; 32]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn image_video_audio_and_document_each_give_one_attachment() {
        let img = attachments(&image(Some("/v/a"), Some(vec![9; 32])));
        assert_eq!(img.len(), 1);
        assert_eq!(img[0].kind, MediaKind::Image);
        assert_eq!(
            (img[0].mime.as_deref(), img[0].size, img[0].name.as_deref()),
            (Some("image/jpeg"), Some(2048), None)
        );
        let f = parse_fetch_ref(&img[0].fetch_ref).unwrap();
        assert_eq!((f.direct_path.as_str(), f.length), ("/v/a", 2048));
        assert_eq!(f.media_key, vec![9; 32]);

        let video = wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                mimetype: Some("video/mp4".into()),
                direct_path: Some("/v/b".into()),
                media_key: Some(vec![3; 32]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = attachments(&video);
        assert_eq!((v.len(), v[0].kind), (1, MediaKind::Video));

        let note = wa::Message {
            ptv_message: MessageField::some(wa::message::VideoMessage {
                direct_path: Some("/v/c".into()),
                media_key: Some(vec![3; 32]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(attachments(&note)[0].kind, MediaKind::Video);

        let voice = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                direct_path: Some("/v/d".into()),
                media_key: Some(vec![3; 32]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(attachments(&voice)[0].kind, MediaKind::Audio);

        let doc = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("plan.pdf".into()),
                mimetype: Some("application/pdf".into()),
                file_length: Some(10),
                direct_path: Some("/v/e".into()),
                media_key: Some(vec![3; 32]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = attachments(&doc);
        assert_eq!(
            (d.len(), d[0].kind, d[0].name.as_deref()),
            (1, MediaKind::Document, Some("plan.pdf"))
        );
    }

    #[test]
    fn media_without_download_details_keeps_a_chip_but_no_ref() {
        let a = attachments(&image(None, None));
        assert_eq!(a.len(), 1);
        assert!(a[0].fetch_ref.is_empty());
        assert!(attachments(&wa::Message::default()).is_empty());
        // Stickers and the like carry no attachment: their text marker is enough.
        let sticker = wa::Message {
            sticker_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert!(attachments(&sticker).is_empty());
    }

    #[test]
    fn attachments_inside_an_ephemeral_wrapper_are_found() {
        let wrapped = wa::Message {
            ephemeral_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(image(Some("/v/a"), Some(vec![9; 32]))),
            }),
            ..Default::default()
        };
        let names = Names::default();
        let d = details(&wrapped, &jid(ALEX), &names);
        assert_eq!(d.attachments.len(), 1);
    }

    #[test]
    fn a_reply_reads_the_quote_with_sender_and_one_line_preview() {
        let mut names = Names::default();
        names.set_book("15550100002", "Sam Sample");
        let ctx = wa::ContextInfo {
            stanza_id: Some("Q1".into()),
            participant: Some(SAM.into()),
            quoted_message: MessageField::some(wa::Message {
                conversation: Some(format!("two\nlines {}", "x".repeat(300))),
                ..Default::default()
            }),
            ..Default::default()
        };
        let group = jid("120363000000000000@g.us");
        let q = quoted(&ctx, &group, &names).unwrap();
        assert_eq!(q.id, "Q1");
        assert_eq!(q.sender, "Sam Sample");
        assert_eq!(q.sender_id, SAM);
        assert!(q.preview.starts_with("two lines x"));
        assert_eq!(q.preview.chars().count(), QUOTE_PREVIEW_CHARS);
        assert!(!q.preview.contains('\n'));
    }

    #[test]
    fn a_quote_of_our_own_message_says_you_and_a_missing_body_gives_an_empty_preview() {
        let mut names = Names::default();
        names.set_own(vec!["15550100009".into()]);
        let ctx = wa::ContextInfo {
            stanza_id: Some("Q2".into()),
            participant: Some("15550100009@s.whatsapp.net".into()),
            ..Default::default()
        };
        let q = quoted(&ctx, &jid(ALEX), &names).unwrap();
        assert_eq!((q.sender.as_str(), q.preview.as_str()), ("You", ""));
        // No quoted id, no quote.
        assert_eq!(
            quoted(&wa::ContextInfo::default(), &jid(ALEX), &names),
            None
        );
        // A direct chat with no participant falls back to the chat's own address.
        let bare = wa::ContextInfo {
            stanza_id: Some("Q3".into()),
            ..Default::default()
        };
        assert_eq!(quoted(&bare, &jid(ALEX), &names).unwrap().sender_id, ALEX);
        // In a group there is nobody to fall back to.
        let g = jid("120363000000000000@g.us");
        assert_eq!(quoted(&bare, &g, &names).unwrap().sender_id, "");
    }

    #[test]
    fn details_read_the_reply_the_mentions_and_the_attachment_together() {
        let names = Names::default();
        let msg = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("hi @15550100001".into()),
                context_info: MessageField::some(wa::ContextInfo {
                    stanza_id: Some("Q1".into()),
                    participant: Some(SAM.into()),
                    mentioned_jid: vec!["15550100001:3@s.whatsapp.net".into(), ALEX.into()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = details(&msg, &jid("120363000000000000@g.us"), &names);
        assert_eq!(d.reply_to.unwrap().id, "Q1");
        // The device suffix is dropped and the duplicate collapses.
        assert_eq!(d.mentioned, vec![ALEX.to_string()]);
        assert!(d.attachments.is_empty());
    }

    #[test]
    fn incoming_mentions_read_as_at_name() {
        let mut names = Names::default();
        names.set_book("15550100001", "Alex Example");
        names.set_own(vec!["15550100009".into()]);
        let mentioned = vec![ALEX.to_string(), "15550100009@s.whatsapp.net".to_string()];
        let (text, people) = normalise_mentions(
            "hey @15550100001 and @15550100009, not @155501000012 or user@host",
            &mentioned,
            &names,
        );
        assert_eq!(
            text,
            "hey @Alex Example and @You, not @155501000012 or user@host"
        );
        assert_eq!(
            people,
            vec![
                person(ALEX, "Alex Example"),
                person("15550100009@s.whatsapp.net", "You")
            ]
        );
        assert!(mentions_me(&mentioned, &names));
        assert!(!mentions_me(&[ALEX.to_string()], &names));
        // Nothing mentioned: text untouched.
        assert_eq!(
            normalise_mentions("plain @15550100001", &[], &names).0,
            "plain @15550100001"
        );
    }

    #[test]
    fn a_hidden_number_mention_falls_back_to_the_label() {
        let names = Names::default();
        let (text, people) =
            normalise_mentions("ping @100200300", &["100200300@lid".to_string()], &names);
        assert_eq!(text, "ping @Unknown contact");
        assert_eq!(people.len(), 1);
    }

    #[test]
    fn outgoing_mentions_replace_the_first_unused_token_in_list_order() {
        let alex = person(ALEX, "Alex");
        let sam = person(SAM, "Sam");
        let a = apply_mentions("@Alex and @Sam, @Alex again", &[alex.clone(), sam.clone()]);
        assert_eq!(a.wire, "@15550100001 and @15550100002, @Alex again");
        assert_eq!(a.jids, vec![ALEX.to_string(), SAM.to_string()]);
        assert_eq!(a.applied, vec![alex.clone(), sam]);

        // The same person listed twice uses the second token for the second entry.
        let twice = apply_mentions("@Alex @Alex", &[alex.clone(), alex.clone()]);
        assert_eq!(twice.wire, "@15550100001 @15550100001");
        assert_eq!(twice.jids, vec![ALEX.to_string()]);
    }

    #[test]
    fn outgoing_mentions_drop_a_missing_token_and_respect_word_ends() {
        let al = person("15550100003@s.whatsapp.net", "Al");
        let alex = person(ALEX, "Alex");
        // "@Al" must not match inside "@Alex"; it has no token of its own, so it is dropped.
        let a = apply_mentions("hello @Alex", &[al.clone(), alex.clone()]);
        assert_eq!(a.wire, "hello @15550100001");
        assert_eq!(a.applied, vec![alex.clone()]);
        // Missing token: text goes out as typed.
        let none = apply_mentions("no tokens here", std::slice::from_ref(&alex));
        assert_eq!(none.wire, "no tokens here");
        assert!(none.jids.is_empty() && none.applied.is_empty());
        // Punctuation after the name is a word end.
        assert_eq!(
            apply_mentions("thanks @Alex.", &[alex]).wire,
            "thanks @15550100001."
        );
        // Non-ASCII names and an empty list are safe.
        let ana = person("15550100004@s.whatsapp.net", "Ana Maria Jos\u{e9}");
        assert_eq!(
            apply_mentions("hi @Ana Maria Jos\u{e9}!", &[ana]).wire,
            "hi @15550100004!"
        );
        assert_eq!(apply_mentions("x", &[]).wire, "x");
    }

    #[test]
    fn a_text_message_is_plain_unless_it_has_a_quote_or_mentions() {
        let (plain, applied) = text_message("hi", None, &[]);
        assert_eq!(plain.conversation.as_deref(), Some("hi"));
        assert!(applied.is_empty());

        let q = QuotedRef {
            id: "Q1".into(),
            sender: "Sam".into(),
            sender_id: SAM.into(),
            preview: "the plan".into(),
        };
        let ctx = quote_context(&q, SAM, "the plan");
        let (m, _) = text_message("agreed", Some(ctx), &[]);
        let ext = m.extended_text_message.as_option().unwrap();
        assert_eq!(ext.text.as_deref(), Some("agreed"));
        let c = ext.context_info.as_option().unwrap();
        assert_eq!(c.stanza_id.as_deref(), Some("Q1"));
        assert_eq!(c.participant.as_deref(), Some(SAM));
        assert_eq!(
            c.quoted_message
                .as_option()
                .unwrap()
                .conversation
                .as_deref(),
            Some("the plan")
        );

        let (m, applied) = text_message("hi @Alex", None, &[person(ALEX, "Alex")]);
        let ext = m.extended_text_message.as_option().unwrap();
        assert_eq!(ext.text.as_deref(), Some("hi @15550100001"));
        assert_eq!(
            ext.context_info.as_option().unwrap().mentioned_jid,
            vec![ALEX.to_string()]
        );
        assert_eq!(applied.len(), 1);

        // Quote and mention share one ContextInfo.
        let ctx = quote_context(&q, SAM, "");
        assert!(
            ctx.quoted_message.is_unset(),
            "an empty preview sends no copy"
        );
        let (m, _) = text_message("@Alex ok", Some(ctx), &[person(ALEX, "Alex")]);
        let c = m
            .extended_text_message
            .as_option()
            .unwrap()
            .context_info
            .as_option()
            .unwrap();
        assert_eq!(c.stanza_id.as_deref(), Some("Q1"));
        assert_eq!(c.mentioned_jid.len(), 1);
    }

    fn uploaded() -> Uploaded {
        Uploaded {
            url: "https://example.invalid/m".into(),
            direct_path: "/v/up".into(),
            media_key: [5; 32],
            file_enc_sha256: [6; 32],
            file_sha256: [7; 32],
            file_length: 1234,
            media_key_timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn the_image_message_carries_every_upload_field() {
        let m = image_message(&uploaded(), "image/png", 640, 480, "sunset");
        let i = m.image_message.as_option().unwrap();
        assert_eq!(i.url.as_deref(), Some("https://example.invalid/m"));
        assert_eq!(i.direct_path.as_deref(), Some("/v/up"));
        assert_eq!(i.mimetype.as_deref(), Some("image/png"));
        assert_eq!(i.caption.as_deref(), Some("sunset"));
        assert_eq!((i.width, i.height), (Some(640), Some(480)));
        assert_eq!(i.file_length, Some(1234));
        assert_eq!(i.media_key.as_deref(), Some(&[5u8; 32][..]));
        assert_eq!(i.file_enc_sha256.as_deref(), Some(&[6u8; 32][..]));
        assert_eq!(i.file_sha256.as_deref(), Some(&[7u8; 32][..]));
        assert_eq!(i.media_key_timestamp, Some(1_700_000_000));
        // No caption: the field is absent, not an empty string.
        let bare = image_message(&uploaded(), "image/png", 1, 1, "");
        assert_eq!(bare.image_message.as_option().unwrap().caption, None);
        // What we send reads back as the same attachment the receiver builds.
        let back = attachments(&m);
        assert_eq!(back[0].kind, MediaKind::Image);
        let own = sent_attachment(MediaKind::Image, None, "image/png", &uploaded());
        assert_eq!(
            parse_fetch_ref(&back[0].fetch_ref),
            parse_fetch_ref(&own.fetch_ref)
        );
    }

    #[test]
    fn the_document_message_has_name_mime_size_and_upload_fields() {
        let m = document_message(&uploaded(), "plan.pdf", "application/pdf", "");
        let d = m.document_message.as_option().unwrap();
        assert_eq!(d.file_name.as_deref(), Some("plan.pdf"));
        assert_eq!(d.title.as_deref(), Some("plan.pdf"));
        assert_eq!(d.mimetype.as_deref(), Some("application/pdf"));
        assert_eq!(d.file_length, Some(1234));
        assert_eq!(d.direct_path.as_deref(), Some("/v/up"));
        assert_eq!(d.caption, None);
        assert_eq!(content(&m), Content::Text("[document: plan.pdf]".into()));
        let with_caption = document_message(&uploaded(), "plan.pdf", "application/pdf", "see p2");
        assert_eq!(
            content(&with_caption),
            Content::Text("[document: plan.pdf] see p2".into())
        );
    }

    #[test]
    fn markers_match_what_incoming_media_gets() {
        assert_eq!(media_marker(MediaKind::Image, "", ""), "[image]");
        assert_eq!(media_marker(MediaKind::Image, "x.png", "hi"), "[image] hi");
        assert_eq!(
            media_marker(MediaKind::Document, "plan.pdf", ""),
            "[document: plan.pdf]"
        );
        assert_eq!(media_marker(MediaKind::Document, "", "c"), "[document] c");
    }

    #[test]
    fn mime_and_display_names() {
        assert_eq!(mime_for_name("a.PDF"), "application/pdf");
        assert_eq!(mime_for_name("noext"), "application/octet-stream");
        assert_eq!(mime_for_name("a.weird"), "application/octet-stream");
        assert_eq!(
            display_name(std::path::Path::new("C:\\Users\\x\\secret dir\\plan.pdf")),
            if cfg!(windows) {
                "plan.pdf"
            } else {
                "C:\\Users\\x\\secret dir\\plan.pdf"
            }
        );
        assert_eq!(display_name(std::path::Path::new("")), "file");
    }

    #[test]
    fn one_person_is_one_reactor_whatever_their_address() {
        let mut names = Names::default();
        assert_eq!(
            reactor_id(&jid(ALEX), true, &names),
            chatcli_core::store::REACTOR_ME
        );
        assert_eq!(reactor_id(&jid(ALEX), false, &names), "15550100001");
        assert_eq!(
            reactor_id(&jid("100200300@lid"), false, &names),
            "100200300"
        );
        names.link("15550100001", "100200300");
        assert_eq!(
            reactor_id(&jid("100200300@lid"), false, &names),
            "15550100001"
        );
        assert_eq!(reactor_id(&jid(ALEX), false, &names), "15550100001");
    }
}
