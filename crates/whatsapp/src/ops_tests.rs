//! The 0.2 command handling against a fake client: no connection, no library. The fake records
//! every call, so a test can say what went out on the wire and what did not.

use std::sync::Mutex;

use chatcli_core::media::AvatarLookup;
use chatcli_core::model::{Attachment, ChatKind, EventRx, MediaKind, MUTE_FOREVER};
use chatcli_core::settings::MediaSettings;
use whatsapp_rust::wacore::types::events as wev;
use whatsapp_rust::wacore::types::message::MessageSource;
use whatsapp_rust::wacore::types::presence::{ChatPresence, ChatPresenceMedia, ReceiptType};

use super::tests::{
    backend, backend_with, chat, drain, inbound, seed_chat, stored, text, ALEX, LID, SAM, TEAM,
};
use super::*;
use crate::wa::{FetchError, Member, Progress, UploadInput};

const OWN_PN: &str = "15550100009";
const OWN_JID: &str = "15550100009@s.whatsapp.net";

#[derive(Clone, Debug)]
enum Call {
    Send(Jid, wa::Message),
    React(Jid, wa::MessageKey, String),
    Edit(Jid, String, wa::Message),
    Revoke(Jid, String),
    Upload {
        kind: MediaKind,
        bytes: Option<usize>,
        path: Option<PathBuf>,
    },
    Download(rich::Fetch),
    History {
        chat: Jid,
        id: String,
        from_me: bool,
        ts: i64,
    },
    Typing(Jid, bool),
    Members(Jid),
    MarkRead(Jid),
    Receipts(Jid, Option<Jid>, Vec<String>),
    Flag(Jid, ChatFlag, bool),
    Photo(Jid),
}

#[derive(Clone)]
enum Plan {
    Bytes(Vec<u8>),
    Expired,
    Fail(String),
}

struct FakeWa {
    calls: Mutex<Vec<Call>>,
    /// Some(reason): every call that can fail does.
    fail: Mutex<Option<String>>,
    members: Mutex<Vec<(String, Option<String>)>>,
    download: Mutex<Plan>,
    /// True: `react` never returns.
    hang: Mutex<bool>,
    /// What `profile_picture` answers.
    photo: Mutex<Result<Option<Vec<u8>>, String>>,
    /// When each `profile_picture` call came.
    photo_at: Mutex<Vec<tokio::time::Instant>>,
    /// Some: `profile_picture` waits for a `notify_one` before it answers.
    photo_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl FakeWa {
    fn new() -> Arc<FakeWa> {
        Arc::new(FakeWa {
            calls: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            members: Mutex::new(Vec::new()),
            download: Mutex::new(Plan::Fail("no plan".into())),
            hang: Mutex::new(false),
            photo: Mutex::new(Ok(Some(photo_png()))),
            photo_at: Mutex::new(Vec::new()),
            photo_gate: Mutex::new(None),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn fail_with(&self, reason: &str) {
        *self.fail.lock().unwrap() = Some(reason.to_string());
    }

    fn record(&self, c: Call) -> Result<(), String> {
        self.calls.lock().unwrap().push(c);
        match &*self.fail.lock().unwrap() {
            Some(r) => Err(r.clone()),
            None => Ok(()),
        }
    }
}

#[whatsapp_rust::async_trait]
impl Wa for FakeWa {
    fn own_users(&self) -> Vec<String> {
        vec![OWN_PN.to_string(), "100999".to_string()]
    }

    async fn send(&self, to: Jid, message: wa::Message) -> Result<String, String> {
        self.record(Call::Send(to, message))?;
        Ok(format!("WA{}", self.calls().len()))
    }

    async fn react(&self, chat: Jid, key: wa::MessageKey, emoji: String) -> Result<(), String> {
        if *self.hang.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        self.record(Call::React(chat, key, emoji))
    }

    async fn edit(&self, chat: Jid, id: String, message: wa::Message) -> Result<(), String> {
        self.record(Call::Edit(chat, id, message))
    }

    async fn revoke(&self, chat: Jid, id: String) -> Result<(), String> {
        self.record(Call::Revoke(chat, id))
    }

    async fn upload(
        &self,
        input: UploadInput,
        kind: MediaKind,
        progress: Progress,
    ) -> Result<rich::Uploaded, String> {
        let (bytes, path) = match &input {
            UploadInput::Bytes(b) => (Some(b.len()), None),
            UploadInput::File(p) => (None, Some(p.clone())),
        };
        self.record(Call::Upload { kind, bytes, path })?;
        progress(10, 100);
        progress(100, 100);
        Ok(rich::Uploaded {
            url: "https://example.invalid/up".into(),
            direct_path: "/v/up".into(),
            media_key: [5; 32],
            file_enc_sha256: [6; 32],
            file_sha256: [7; 32],
            file_length: 1234,
            media_key_timestamp: 1_700_000_000,
        })
    }

    async fn download(&self, fetch: rich::Fetch, limit: u64) -> Result<Vec<u8>, FetchError> {
        let _ = self.record(Call::Download(fetch));
        match self.download.lock().unwrap().clone() {
            // Like the real sink: more bytes than the limit is an error, whatever was declared.
            Plan::Bytes(b) if b.len() as u64 > limit => Err(FetchError::TooBig),
            Plan::Bytes(b) => Ok(b),
            Plan::Expired => Err(FetchError::Expired),
            Plan::Fail(e) => Err(FetchError::Other(e)),
        }
    }

    async fn request_history(
        &self,
        chat: Jid,
        oldest_id: String,
        oldest_from_me: bool,
        oldest_ts_ms: i64,
    ) -> Result<String, String> {
        self.record(Call::History {
            chat,
            id: oldest_id,
            from_me: oldest_from_me,
            ts: oldest_ts_ms,
        })?;
        Ok("REQ1".to_string())
    }

    async fn typing(&self, chat: Jid, composing: bool) -> Result<(), String> {
        self.record(Call::Typing(chat, composing))
    }

    async fn members(&self, chat: Jid) -> Result<Vec<Member>, String> {
        self.record(Call::Members(chat))?;
        Ok(self
            .members
            .lock()
            .unwrap()
            .iter()
            .map(|(j, pn)| Member {
                jid: j.parse().unwrap(),
                pn: pn.as_ref().map(|p| p.parse().unwrap()),
            })
            .collect())
    }

    async fn mark_chat_read(&self, chat: Jid) -> Result<(), String> {
        self.record(Call::MarkRead(chat))
    }

    async fn mark_receipts(
        &self,
        chat: Jid,
        sender: Option<Jid>,
        ids: Vec<String>,
    ) -> Result<(), String> {
        self.record(Call::Receipts(chat, sender, ids))
    }

    async fn set_flag(&self, chat: Jid, flag: ChatFlag, on: bool) -> Result<(), String> {
        self.record(Call::Flag(chat, flag, on))
    }

    async fn profile_picture(&self, jid: Jid) -> Result<Option<Vec<u8>>, String> {
        self.photo_at
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        self.record(Call::Photo(jid))?;
        let gate = self.photo_gate.lock().unwrap().clone();
        if let Some(g) = gate {
            g.notified().await;
        }
        self.photo.lock().unwrap().clone()
    }
}

fn photo_png() -> Vec<u8> {
    let mut v = Vec::new();
    image::RgbImage::from_pixel(20, 10, image::Rgb([5, 6, 7]))
        .write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png)
        .unwrap();
    v
}

/// A backend linked to a fresh fake.
fn linked(b: &mut Backend) -> Arc<FakeWa> {
    let fake = FakeWa::new();
    b.attach(fake.clone());
    fake
}

/// Runs the next result a task sent back (a send result or an answer) through the backend.
async fn pump(b: &mut Backend, rx: &mut mpsc::UnboundedReceiver<Msg>) {
    let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a result arrives")
        .expect("channel open");
    b.on_msg(m).await;
}

fn jid(s: &str) -> Jid {
    s.parse().unwrap()
}

fn person(id: &str, name: &str) -> PersonRef {
    PersonRef {
        id: id.into(),
        name: name.into(),
    }
}

fn quote(id: &str, sender: &str, sender_id: &str, preview: &str) -> QuotedRef {
    QuotedRef {
        id: id.into(),
        sender: sender.into(),
        sender_id: sender_id.into(),
        preview: preview.into(),
    }
}

fn send_cmd(
    chat: &str,
    body: &str,
    client: &str,
    reply_to: Option<QuotedRef>,
    mentions: Vec<PersonRef>,
) -> Command {
    Command::Send {
        chat_id: chat.into(),
        text: body.into(),
        client_id: client.into(),
        reply_to,
        mentions,
    }
}

/// A message from `sender` in `chat`, with an optional alternative address for the sender.
fn inbound_with_alt(
    chat: &str,
    sender: &str,
    alt: Option<&str>,
    id: &str,
    ts: i64,
    body: wa::Message,
) -> InboundMessage {
    let source = MessageSource {
        chat: chat.parse().unwrap(),
        sender: sender.parse().unwrap(),
        sender_alt: alt.map(|a| a.parse().unwrap()),
        is_from_me: false,
        is_group: chat.ends_with("@g.us"),
        ..Default::default()
    };
    let info = MessageInfo {
        id: id.to_string(),
        push_name: "Alex Example".into(),
        timestamp: whatsapp_rust::chrono::DateTime::from_timestamp(ts, 0).unwrap(),
        source,
        ..Default::default()
    };
    InboundMessage::builder()
        .message(Arc::new(body))
        .info(Arc::new(info))
        .build()
}

fn reaction(target: &str, emoji: &str) -> wa::Message {
    wa::Message {
        reaction_message: MessageField::some(wa::message::ReactionMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(target.into()),
                ..Default::default()
            }),
            text: Some(emoji.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn protocol(
    kind: wa::message::protocol_message::Type,
    target: &str,
    edited: Option<&str>,
) -> wa::Message {
    wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(target.into()),
                ..Default::default()
            }),
            r#type: Some(kind),
            edited_message: edited
                .map(|t| MessageField::some(text(t)))
                .unwrap_or_default(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A group chat with one message from Sam, ready to reply to, react to or quote.
async fn group_with_sam(b: &mut Backend, erx: &mut EventRx) {
    b.on_message(&inbound(
        TEAM,
        SAM,
        "g1",
        false,
        1_700_000_000,
        text("the plan"),
        "Sam Sample",
    ))
    .await;
    drain(erx);
}

/// A message of ours, stored as sent.
fn own_message(b: &Backend, chat: &str, id: &str, body: &str) {
    b.store
        .upsert_messages(&[ChatMessage {
            from_me: true,
            sender: "You".into(),
            text: body.into(),
            sender_id: OWN_JID.into(),
            ..stored(chat, id, 1_700_000_100_000, true, "You")
        }])
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// #12 reply
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_reply_goes_out_with_the_quote_and_is_stored_with_it() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    group_with_sam(&mut b, &mut erx).await;
    let q = quote("g1", "Sam Sample", SAM, "the plan");
    b.on_command(send_cmd(TEAM, "agreed", "c1", Some(q.clone()), vec![]))
        .await;
    pump(&mut b, &mut rx).await;

    let calls = fake.calls();
    let Call::Send(to, msg) = &calls[0] else {
        panic!("expected a send, got {calls:?}")
    };
    assert_eq!(to, &jid(TEAM));
    let ext = msg
        .extended_text_message
        .as_option()
        .expect("a quote needs the extended form");
    assert_eq!(ext.text.as_deref(), Some("agreed"));
    let ctx = ext.context_info.as_option().unwrap();
    assert_eq!(ctx.stanza_id.as_deref(), Some("g1"));
    assert_eq!(ctx.participant.as_deref(), Some(SAM));
    assert_eq!(
        ctx.quoted_message
            .as_option()
            .unwrap()
            .conversation
            .as_deref(),
        Some("the plan")
    );

    let evs = drain(&mut erx);
    let Some(Out::MessageConfirmed { message, .. }) = evs
        .iter()
        .find(|e| matches!(e, Out::MessageConfirmed { .. }))
    else {
        panic!("no confirmation: {evs:?}")
    };
    assert_eq!(message.reply_to, Some(q.clone()));
    let kept = b.store.message(NET, TEAM, &message.id).unwrap().unwrap();
    assert_eq!(
        kept.reply_to,
        Some(q),
        "the quote is stored, so a reopened chat still shows it"
    );
}

#[tokio::test]
async fn a_reply_to_an_old_group_message_needs_a_known_sender() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    // A row from before 0.2: no sender id stored, and the quote carries none either.
    let q = quote("old", "Sam Sample", "", "");
    b.on_command(send_cmd(TEAM, "ok", "c1", Some(q), vec![]))
        .await;
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::MessageFailed { error, .. }
        if error == "Cannot reply to this older message: its sender is unknown."));
    assert!(
        fake.calls().is_empty(),
        "nothing is sent, and never as a plain message"
    );

    // The sender remembered from this run is enough.
    b.remember_sender(TEAM, "old", jid(SAM));
    let q = quote("old", "Sam Sample", "", "");
    b.on_command(send_cmd(TEAM, "ok", "c2", Some(q), vec![]))
        .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::Send(_, msg) = &calls[0] else {
        panic!()
    };
    let ctx = msg
        .extended_text_message
        .as_option()
        .unwrap()
        .context_info
        .as_option()
        .unwrap();
    assert_eq!(ctx.participant.as_deref(), Some(SAM));
}

#[tokio::test]
async fn a_reply_in_a_direct_chat_names_the_other_side_or_us() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    // Their message, quote carries no sender id (row from before 0.2): the chat is the other side.
    b.on_command(send_cmd(
        ALEX,
        "yes",
        "c1",
        Some(quote("m1", "Alex Example", "", "hello")),
        vec![],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    // Our own message: the participant is our address.
    b.on_command(send_cmd(
        ALEX,
        "again",
        "c2",
        Some(quote("m2", "You", "", "my note")),
        vec![],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    let participants: Vec<String> = fake
        .calls()
        .iter()
        .filter_map(|c| match c {
            Call::Send(_, m) => m
                .extended_text_message
                .as_option()
                .and_then(|e| e.context_info.as_option())
                .and_then(|c| c.participant.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(participants, [ALEX, OWN_JID]);
    drain(&mut erx);
}

#[tokio::test]
async fn an_empty_preview_is_filled_from_the_stored_message() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    group_with_sam(&mut b, &mut erx).await;
    b.on_command(send_cmd(
        TEAM,
        "ok",
        "c1",
        Some(quote("g1", "Sam Sample", SAM, "")),
        vec![],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::Send(_, msg) = &calls[0] else {
        panic!()
    };
    let ctx = msg
        .extended_text_message
        .as_option()
        .unwrap()
        .context_info
        .as_option()
        .unwrap();
    assert_eq!(
        ctx.quoted_message
            .as_option()
            .unwrap()
            .conversation
            .as_deref(),
        Some("the plan")
    );
}

#[tokio::test]
async fn a_received_reply_shows_the_quote_and_a_missing_one_has_a_placeholder_preview() {
    let (mut b, mut erx, _rx) = backend();
    group_with_sam(&mut b, &mut erx).await;
    let reply = |quoted_id: &str, with_body: bool| wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some("sounds good".into()),
            context_info: MessageField::some(wa::ContextInfo {
                stanza_id: Some(quoted_id.into()),
                participant: Some(SAM.into()),
                quoted_message: if with_body {
                    MessageField::some(text("the plan"))
                } else {
                    Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    // The quote carries no text but the quoted message is stored: the preview comes from it.
    b.on_message(&inbound(
        TEAM,
        ALEX,
        "r1",
        false,
        1_700_000_100,
        reply("g1", false),
        "Alex Example",
    ))
    .await;
    // The quoted message is not stored and the quote has no copy: empty preview, the TUI's
    // placeholder shows.
    b.on_message(&inbound(
        TEAM,
        ALEX,
        "r2",
        false,
        1_700_000_200,
        reply("gone", false),
        "Alex Example",
    ))
    .await;
    let m1 = b.store.message(NET, TEAM, "r1").unwrap().unwrap();
    let q1 = m1.reply_to.unwrap();
    assert_eq!(
        (q1.id.as_str(), q1.preview.as_str(), q1.sender_id.as_str()),
        ("g1", "the plan", SAM)
    );
    let m2 = b.store.message(NET, TEAM, "r2").unwrap().unwrap();
    assert_eq!(m2.reply_to.unwrap().preview, "");
}

// ---------------------------------------------------------------------------------------------
// #28 mentions and members
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_mention_is_sent_as_the_number_and_stored_as_typed() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let alex = person(ALEX, "Alex");
    b.on_command(send_cmd(
        TEAM,
        "hi @Alex, ok?",
        "c1",
        None,
        vec![alex.clone()],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::Send(_, msg) = &calls[0] else {
        panic!()
    };
    let ext = msg.extended_text_message.as_option().unwrap();
    assert_eq!(ext.text.as_deref(), Some("hi @15550100001, ok?"));
    assert_eq!(
        ext.context_info.as_option().unwrap().mentioned_jid,
        vec![ALEX.to_string()]
    );
    let evs = drain(&mut erx);
    let confirmed = evs
        .iter()
        .find_map(|e| match e {
            Out::MessageConfirmed { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(confirmed.text, "hi @Alex, ok?");
    assert_eq!(confirmed.mentions, vec![alex]);
}

#[tokio::test]
async fn mentions_are_ignored_in_a_direct_chat_and_a_missing_token_is_dropped() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.on_command(send_cmd(
        ALEX,
        "hi @Alex",
        "c1",
        None,
        vec![person(ALEX, "Alex")],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    b.on_command(send_cmd(
        TEAM,
        "no token",
        "c2",
        None,
        vec![person(ALEX, "Alex")],
    ))
    .await;
    pump(&mut b, &mut rx).await;
    let sent: Vec<wa::Message> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Send(_, m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent[0].conversation.as_deref(),
        Some("hi @Alex"),
        "direct chat: as typed, plain"
    );
    assert_eq!(sent[1].conversation.as_deref(), Some("no token"));
    drain(&mut erx);
}

#[tokio::test]
async fn an_incoming_mention_of_me_is_marked_counted_once_and_cleared_by_reading() {
    let (mut b, mut erx, _rx) = backend();
    let _fake = linked(&mut b);
    b.names.set_book("15550100001", "Alex Example");
    let mention = wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some("ping @15550100009 please".into()),
            context_info: MessageField::some(wa::ContextInfo {
                mentioned_jid: vec![OWN_JID.into()],
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let m = inbound(
        TEAM,
        ALEX,
        "m1",
        false,
        1_700_000_000,
        mention,
        "Alex Example",
    );
    b.on_message(&m).await;
    let evs = drain(&mut erx);
    let Some(Out::Message(got)) = evs.iter().find(|e| matches!(e, Out::Message(_))) else {
        panic!("{evs:?}")
    };
    assert!(got.mentions_me);
    assert_eq!(got.text, "ping @You please");
    assert_eq!(got.mentions, vec![person(OWN_JID, "You")]);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::ChatUpdated(c) if c.unread == 1 && c.mentions == 1)));
    assert_eq!(chat(&b, TEAM).mentions, 1);

    // A redelivery is not a second mention.
    b.on_message(&m).await;
    assert_eq!((chat(&b, TEAM).unread, chat(&b, TEAM).mentions), (1, 1));

    // A message that does not mention me leaves the counter alone.
    b.on_message(&inbound(
        TEAM,
        ALEX,
        "m2",
        false,
        1_700_000_100,
        text("hi all"),
        "Alex Example",
    ))
    .await;
    assert_eq!((chat(&b, TEAM).unread, chat(&b, TEAM).mentions), (2, 1));

    b.on_command(Command::MarkRead {
        chat_id: TEAM.into(),
    })
    .await;
    assert_eq!((chat(&b, TEAM).unread, chat(&b, TEAM).mentions), (0, 0));
}

#[tokio::test]
async fn members_come_from_the_group_are_cached_and_never_include_me() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.names.set_book("15550100001", "Alex Example");
    *fake.members.lock().unwrap() = vec![
        (ALEX.into(), None),
        (SAM.into(), None),
        (OWN_JID.into(), None),
        // A hidden address whose phone number is known by name.
        (LID.into(), Some(ALEX.into())),
    ];
    b.on_command(Command::LoadMembers {
        chat_id: TEAM.into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    let Some(Out::Members { members, error, .. }) =
        evs.iter().find(|e| matches!(e, Out::Members { .. }))
    else {
        panic!("{evs:?}")
    };
    assert_eq!(*error, None);
    assert!(matches!(&fake.calls()[0], Call::Members(j) if j == &jid(TEAM)));
    let names: Vec<&str> = members.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["+15550100002", "Alex Example", "Alex Example"]);
    assert!(!members.iter().any(|p| p.id == OWN_JID));
    assert_eq!(b.store.members(NET, TEAM, 100).unwrap().len(), 3);

    // The group cannot be read now: the saved list answers.
    fake.fail_with("offline");
    b.on_command(Command::LoadMembers {
        chat_id: TEAM.into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Members { members, error: None, .. } if members.len() == 3)));
}

#[tokio::test]
async fn members_fail_in_a_direct_chat_offline_and_with_nothing_saved() {
    let (mut b, mut erx, mut rx) = backend();
    b.on_command(Command::LoadMembers {
        chat_id: TEAM.into(),
    })
    .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Members { members, error: Some(e), .. }
        if members.is_empty() && e.contains("not connected"))
    );
    let fake = linked(&mut b);
    b.on_command(Command::LoadMembers {
        chat_id: ALEX.into(),
    })
    .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Members { error: Some(e), .. }
        if e.contains("group chats only"))
    );
    fake.fail_with("boom");
    b.on_command(Command::LoadMembers {
        chat_id: TEAM.into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Members { members, error: Some(_), .. }
        if members.is_empty())
    );
}

// ---------------------------------------------------------------------------------------------
// #13 reactions
// ---------------------------------------------------------------------------------------------

fn reaction_cmd(chat: &str, id: &str, emoji: &str, remove: bool) -> Command {
    Command::React {
        chat_id: chat.into(),
        message_id: id.into(),
        emoji: emoji.into(),
        remove,
    }
}

#[tokio::test]
async fn reacting_in_a_group_names_the_author_and_updates_the_message() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    group_with_sam(&mut b, &mut erx).await;
    b.on_command(reaction_cmd(TEAM, "g1", "\u{1F44D}", false))
        .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::React(to, key, emoji) = &calls[0] else {
        panic!("{calls:?}")
    };
    assert_eq!(to, &jid(TEAM));
    assert_eq!(
        (
            key.id.as_deref(),
            key.from_me,
            key.participant.as_deref(),
            key.remote_jid.as_deref()
        ),
        (Some("g1"), Some(false), Some(SAM), Some(TEAM))
    );
    assert_eq!(emoji, "\u{1F44D}");
    let evs = drain(&mut erx);
    let Some(Out::MessageChanged(m)) = evs.iter().find(|e| matches!(e, Out::MessageChanged(_)))
    else {
        panic!("{evs:?}")
    };
    assert_eq!(
        m.reactions,
        vec![chatcli_core::model::Reaction {
            emoji: "\u{1F44D}".into(),
            count: 1,
            mine: true
        }]
    );
    assert!(
        !evs.iter().any(|e| matches!(e, Out::Message(_))),
        "a reaction never notifies"
    );

    // Choosing it again removes it: an empty emoji on the wire.
    b.on_command(reaction_cmd(TEAM, "g1", "\u{1F44D}", true))
        .await;
    pump(&mut b, &mut rx).await;
    assert!(matches!(&fake.calls()[1], Call::React(_, _, e) if e.is_empty()));
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(m) if m.reactions.is_empty())));
}

#[tokio::test]
async fn reacting_to_our_own_message_needs_no_participant_and_a_new_emoji_replaces_the_old() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    own_message(&b, TEAM, "mine", "ship it");
    b.on_command(reaction_cmd(TEAM, "mine", "\u{1F44D}", false))
        .await;
    pump(&mut b, &mut rx).await;
    b.on_command(reaction_cmd(TEAM, "mine", "\u{2764}", false))
        .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::React(_, key, _) = &calls[0] else {
        panic!()
    };
    assert_eq!((key.from_me, key.participant.clone()), (Some(true), None));
    let m = b.store.message(NET, TEAM, "mine").unwrap().unwrap();
    assert_eq!(m.reactions.len(), 1, "one reaction per user");
    assert_eq!(m.reactions[0].emoji, "\u{2764}");
    drain(&mut erx);
}

#[tokio::test]
async fn reaction_failures_are_a_notice_and_change_nothing() {
    let (mut b, mut erx, mut rx) = backend();
    // Offline.
    own_message(&b, TEAM, "mine", "ship it");
    b.on_command(reaction_cmd(TEAM, "mine", "\u{1F44D}", false))
        .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Notice { is_error: true, text, .. } if text.contains("not connected"))
    );
    let fake = linked(&mut b);
    // An old group message whose sender is unknown.
    b.store
        .upsert_messages(&[stored(TEAM, "old", 1000, false, "Sam Sample")])
        .unwrap();
    b.on_command(reaction_cmd(TEAM, "old", "\u{1F44D}", false))
        .await;
    assert!(matches!(&drain(&mut erx)[0], Out::Notice { text, .. }
        if text == "Cannot react to this older message: its sender is unknown."));
    // The message is gone.
    b.on_command(reaction_cmd(TEAM, "nope", "\u{1F44D}", false))
        .await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::Notice { is_error: true, .. }
    ));
    assert!(fake.calls().is_empty());
    // The server refuses.
    fake.fail_with("rate limited");
    b.on_command(reaction_cmd(TEAM, "mine", "\u{1F44D}", false))
        .await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Notice { is_error: true, text, .. } if text.contains("rate limited"))
    );
    assert!(b
        .store
        .message(NET, TEAM, "mine")
        .unwrap()
        .unwrap()
        .reactions
        .is_empty());
}

#[tokio::test]
async fn incoming_reactions_count_replace_remove_and_unknown_targets_are_dropped() {
    let (mut b, mut erx, _rx) = backend();
    group_with_sam(&mut b, &mut erx).await;
    let from = |sender: &str, id: &str, emoji: &str| {
        inbound(
            TEAM,
            sender,
            id,
            false,
            1_700_000_300,
            reaction("g1", emoji),
            "",
        )
    };
    b.on_message(&from(ALEX, "x1", "\u{1F44D}")).await;
    b.on_message(&from("15550100003@s.whatsapp.net", "x2", "\u{1F44D}"))
        .await;
    let evs = drain(&mut erx);
    assert!(!evs.iter().any(|e| matches!(e, Out::Message(_))));
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!(
        (
            m.reactions[0].emoji.as_str(),
            m.reactions[0].count,
            m.reactions[0].mine
        ),
        ("\u{1F44D}", 2, false)
    );
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(x) if x.reactions[0].count == 2)));

    // Alex changes his mind: replaced, not added.
    b.on_message(&from(ALEX, "x3", "\u{2764}")).await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!(m.reactions.len(), 2);
    // An empty emoji removes his reaction.
    b.on_message(&from(ALEX, "x4", "")).await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!((m.reactions.len(), m.reactions[0].count), (1, 1));

    // A reaction to a message that is not stored: dropped, no event, no row.
    drain(&mut erx);
    b.on_message(&inbound(
        TEAM,
        ALEX,
        "x5",
        false,
        1_700_000_400,
        reaction("ghost", "\u{1F44D}"),
        "",
    ))
    .await;
    assert!(drain(&mut erx).is_empty());
    assert!(b.store.message(NET, TEAM, "ghost").unwrap().is_none());
    // The reaction stanza itself never becomes a message.
    assert!(b.store.message(NET, TEAM, "x1").unwrap().is_none());
}

#[tokio::test]
async fn our_own_reaction_from_the_phone_is_mine() {
    let (mut b, mut erx, _rx) = backend();
    group_with_sam(&mut b, &mut erx).await;
    b.on_message(&inbound(
        TEAM,
        OWN_JID,
        "x1",
        true,
        1_700_000_300,
        reaction("g1", "\u{1F44D}"),
        "",
    ))
    .await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert!(m.reactions[0].mine);
    drain(&mut erx);
}

// ---------------------------------------------------------------------------------------------
// #27 edit and delete
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn editing_our_own_message_sends_the_edit_then_updates_the_store() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    own_message(&b, ALEX, "mine", "teh plan");
    b.on_command(Command::Edit {
        chat_id: ALEX.into(),
        message_id: "mine".into(),
        text: "the plan".into(),
    })
    .await;
    // The store changes only after WhatsApp took the edit.
    assert_eq!(
        b.store.message(NET, ALEX, "mine").unwrap().unwrap().text,
        "teh plan"
    );
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::Edit(to, id, msg) = &calls[0] else {
        panic!("{calls:?}")
    };
    assert_eq!((to, id.as_str()), (&jid(ALEX), "mine"));
    assert_eq!(msg.conversation.as_deref(), Some("the plan"));
    let m = b.store.message(NET, ALEX, "mine").unwrap().unwrap();
    assert_eq!((m.text.as_str(), m.edited), ("the plan", true));
    assert!(drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(x) if x.text == "the plan" && x.edited)));
}

#[tokio::test]
async fn a_refused_edit_keeps_the_original_text_and_says_so() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    own_message(&b, ALEX, "mine", "teh plan");
    fake.fail_with("edit window closed");
    b.on_command(Command::Edit {
        chat_id: ALEX.into(),
        message_id: "mine".into(),
        text: "the plan".into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let m = b.store.message(NET, ALEX, "mine").unwrap().unwrap();
    assert_eq!((m.text.as_str(), m.edited), ("teh plan", false));
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Notice { is_error: true, text, .. }
        if text.contains("edit window closed") && text.contains("unchanged"))));
    assert!(!evs.iter().any(|e| matches!(e, Out::MessageChanged(_))));
}

#[tokio::test]
async fn only_our_own_sent_messages_can_be_edited_or_deleted() {
    let (mut b, mut erx, _rx) = backend();
    let fake = linked(&mut b);
    b.store
        .upsert_messages(&[stored(ALEX, "theirs", 1000, false, "Alex Example")])
        .unwrap();
    b.on_command(Command::Edit {
        chat_id: ALEX.into(),
        message_id: "theirs".into(),
        text: "x".into(),
    })
    .await;
    b.on_command(Command::Delete {
        chat_id: ALEX.into(),
        message_id: "theirs".into(),
    })
    .await;
    b.on_command(Command::Delete {
        chat_id: ALEX.into(),
        message_id: "missing".into(),
    })
    .await;
    let evs = drain(&mut erx);
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, Out::Notice { is_error: true, .. }))
            .count(),
        3
    );
    assert!(fake.calls().is_empty());
    // An empty edit is refused before any call.
    own_message(&b, ALEX, "mine", "text");
    b.on_command(Command::Edit {
        chat_id: ALEX.into(),
        message_id: "mine".into(),
        text: "   ".into(),
    })
    .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Notice { text, .. } if text.contains("cannot be empty"))
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn an_edit_keeps_the_reply_and_the_mentions_of_the_message() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.store
        .upsert_messages(&[ChatMessage {
            from_me: true,
            sender_id: OWN_JID.into(),
            text: "@Alex see this".into(),
            reply_to: Some(quote("g1", "Sam Sample", SAM, "the plan")),
            mentions: vec![person(ALEX, "Alex")],
            ..stored(TEAM, "mine", 1_700_000_100_000, true, "You")
        }])
        .unwrap();
    b.on_command(Command::Edit {
        chat_id: TEAM.into(),
        message_id: "mine".into(),
        text: "@Alex look at this".into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    let Call::Edit(_, _, msg) = &calls[0] else {
        panic!("{calls:?}")
    };
    let ext = msg.extended_text_message.as_option().unwrap();
    assert_eq!(ext.text.as_deref(), Some("@15550100001 look at this"));
    let ctx = ext.context_info.as_option().unwrap();
    assert_eq!(ctx.stanza_id.as_deref(), Some("g1"));
    assert_eq!(ctx.mentioned_jid, vec![ALEX.to_string()]);
    let m = b.store.message(NET, TEAM, "mine").unwrap().unwrap();
    assert_eq!(
        m.reply_to.unwrap().id,
        "g1",
        "the update never wipes the quote"
    );
    assert_eq!(m.mentions.len(), 1);
    drain(&mut erx);
}

#[tokio::test]
async fn deleting_our_own_message_empties_it_and_keeps_its_place() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    seed_chat(&b.store, 1_700_000_100_000, "You: secret", 0);
    own_message(&b, ALEX, "mine", "secret");
    b.store
        .upsert_messages(&[ChatMessage {
            attachments: vec![Attachment {
                kind: MediaKind::Image,
                name: None,
                mime: None,
                size: None,
                fetch_ref: Default::default(),
                open_url: None,
            }],
            ..b.store.message(NET, ALEX, "mine").unwrap().unwrap()
        }])
        .unwrap();
    b.store
        .set_reaction(
            NET,
            ALEX,
            "mine",
            "15550100001",
            false,
            Some("\u{1F44D}"),
            1,
        )
        .unwrap();
    b.on_command(Command::Delete {
        chat_id: ALEX.into(),
        message_id: "mine".into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    assert!(matches!(&fake.calls()[0], Call::Revoke(to, id) if to == &jid(ALEX) && id == "mine"));
    let m = b.store.message(NET, ALEX, "mine").unwrap().unwrap();
    assert_eq!(m.state, MessageState::Deleted);
    assert!(m.text.is_empty() && m.attachments.is_empty() && m.reactions.is_empty());
    assert_eq!(chat(&b, ALEX).last_preview, "You: [message deleted]");
    assert!(drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(x) if x.state == MessageState::Deleted)));

    // A refused delete leaves the message as it was.
    own_message(&b, ALEX, "mine2", "keep me");
    fake.fail_with("too late");
    b.on_command(Command::Delete {
        chat_id: ALEX.into(),
        message_id: "mine2".into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let m = b.store.message(NET, ALEX, "mine2").unwrap().unwrap();
    assert_eq!((m.state, m.text.as_str()), (MessageState::Sent, "keep me"));
    assert!(drain(&mut erx).iter().any(
        |e| matches!(e, Out::Notice { is_error: true, text, .. } if text.contains("too late"))
    ));
}

// ---------------------------------------------------------------------------------------------
// CodeRabbit fixes: preview by id, history never undoes an edit, redelivery, unread
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn editing_an_older_message_of_the_same_second_leaves_the_preview_on_the_newest() {
    use wa::message::protocol_message::Type;
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "A",
        false,
        1000,
        text("first"),
        "Alex Example",
    ))
    .await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "B",
        false,
        1000,
        text("second"),
        "Alex Example",
    ))
    .await;
    assert_eq!(chat(&b, ALEX).last_preview, "second");
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "e1",
        false,
        1001,
        protocol(Type::MessageEdit, "A", Some("first, fixed")),
        "",
    ))
    .await;
    assert_eq!(
        chat(&b, ALEX).last_preview,
        "second",
        "A is not the newest message, B is"
    );
    // Editing the newest one does move it.
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "e2",
        false,
        1002,
        protocol(Type::MessageEdit, "B", Some("second, fixed")),
        "",
    ))
    .await;
    assert_eq!(chat(&b, ALEX).last_preview, "second, fixed");
    drain(&mut erx);
}

fn history_of(id: &str, body: &str, ts: u64) -> wa::HistorySync {
    wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: ALEX.into(),
            messages: vec![wa::HistorySyncMsg {
                message: MessageField::some(wa::WebMessageInfo {
                    key: MessageField::some(wa::MessageKey {
                        id: Some(id.into()),
                        from_me: Some(false),
                        ..Default::default()
                    }),
                    message: MessageField::some(text(body)),
                    message_timestamp: Some(ts),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            unread_count: Some(0),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_history_chunk_never_puts_back_an_edited_or_deleted_message() {
    use wa::message::protocol_message::Type;
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "m1",
        false,
        1000,
        text("teh build"),
        "Alex Example",
    ))
    .await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "m2",
        false,
        1001,
        text("oops"),
        "Alex Example",
    ))
    .await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "e1",
        false,
        1002,
        protocol(Type::MessageEdit, "m1", Some("the build")),
        "",
    ))
    .await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "d1",
        false,
        1003,
        protocol(Type::Revoke, "m2", None),
        "",
    ))
    .await;
    drain(&mut erx);
    // The history snapshot still has the first versions of both.
    let mut hs = history_of("m1", "teh build", 1);
    hs.conversations[0].messages.extend(
        history_of("m2", "oops", 2).conversations[0]
            .messages
            .clone(),
    );
    hs.conversations[0].messages.extend(
        history_of("m3", "brand new", 3).conversations[0]
            .messages
            .clone(),
    );
    let o = apply_history(&b.store, &b.names, &hs).unwrap();
    assert_eq!(o.touched, vec![ALEX.to_string()]);
    let m1 = b.store.message(NET, ALEX, "m1").unwrap().unwrap();
    assert_eq!((m1.text.as_str(), m1.edited), ("the build", true));
    let m2 = b.store.message(NET, ALEX, "m2").unwrap().unwrap();
    assert_eq!((m2.state, m2.text.as_str()), (MessageState::Deleted, ""));
    // Messages the user never saw changed are still imported.
    assert!(b.store.message(NET, ALEX, "m3").unwrap().is_some());
}

#[tokio::test]
async fn a_redelivered_message_older_than_the_newest_page_is_not_unread_again() {
    let (mut b, mut erx, _rx) = backend();
    seed_chat(&b.store, 100_000, "x", 0);
    let rows: Vec<ChatMessage> = (0..80)
        .map(|i| stored(ALEX, &format!("m{i:02}"), 1_000 + i, false, "Alex Example"))
        .collect();
    b.store.upsert_messages(&rows).unwrap();
    // m00 is far outside the newest 50.
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "m00",
        false,
        1,
        text("text m00"),
        "Alex Example",
    ))
    .await;
    assert_eq!(chat(&b, ALEX).unread, 0);
    let evs = drain(&mut erx);
    assert!(
        !evs.iter().any(|e| matches!(e, Out::Message(_))),
        "a redelivery is not a new message"
    );
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(m) if m.id == "m00")));
    // A really new one still counts.
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "fresh",
        false,
        5_000_000,
        text("hi"),
        "Alex Example",
    ))
    .await;
    assert_eq!(chat(&b, ALEX).unread, 1);
}

#[tokio::test]
async fn a_redelivery_never_puts_back_an_edited_message() {
    use wa::message::protocol_message::Type;
    let (mut b, mut erx, _rx) = backend();
    let original = inbound(
        ALEX,
        ALEX,
        "m1",
        false,
        1000,
        text("teh build"),
        "Alex Example",
    );
    b.on_message(&original).await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "e1",
        false,
        1001,
        protocol(Type::MessageEdit, "m1", Some("the build")),
        "",
    ))
    .await;
    b.on_message(&original).await;
    let m = b.store.message(NET, ALEX, "m1").unwrap().unwrap();
    assert_eq!((m.text.as_str(), m.edited), ("the build", true));
    drain(&mut erx);
}

#[tokio::test]
async fn history_arriving_for_the_open_chat_is_sent_to_the_tui() {
    let (mut b, mut erx, _rx) = backend();
    b.on_command(Command::OpenChat {
        chat_id: ALEX.into(),
    })
    .await;
    drain(&mut erx);
    let o = apply_history(&b.store, &b.names, &history_of("h1", "older", 5)).unwrap();
    b.after_history(&o, false, None);
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::Messages { chat_id, messages, .. }
        if chat_id == ALEX && messages.len() == 1));

    // A chunk for some other chat does not reload the open one; an on-demand answer is not a reload.
    let other = HistoryOutcome {
        touched: vec![SAM.to_string()],
        ..o_clone(&o)
    };
    b.after_history(&other, false, None);
    b.after_history(&o, true, None);
    assert!(drain(&mut erx).is_empty());
}

fn o_clone(o: &HistoryOutcome) -> HistoryOutcome {
    HistoryOutcome {
        chats: o.chats,
        touched: o.touched.clone(),
        messages: o.messages,
        skipped: o.skipped,
        push_names: Vec::new(),
        book_names: Vec::new(),
        links: Vec::new(),
        future_fixed: 0,
    }
}

#[tokio::test]
async fn a_pairing_gets_a_connect_deadline_so_connecting_cannot_hang() {
    let (mut b, mut erx, _rx) = backend();
    b.set_status(Status::LoggingIn);
    assert!(b.connect_by.is_none());
    b.on_event(&Event::PairSuccess(
        wev::PairSuccess::builder()
            .id(ALEX.parse().unwrap())
            .lid(LID.parse().unwrap())
            .business_name(String::new())
            .platform(String::new())
            .build(),
    ))
    .await;
    assert!(b.connect_by.is_some(), "the watchdog starts at PairSuccess");
    assert!(matches!(b.last_status, Status::Connecting));
    drain(&mut erx);
    b.connect_timeout();
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::Status(_, Status::Online { live: false, .. })
    ));
}

#[tokio::test]
async fn a_lid_chat_moves_to_the_phone_number_when_a_later_message_names_it() {
    let (mut b, mut erx, _rx) = backend();
    // The first message arrives with only the hidden address.
    b.on_message(&inbound_with_alt(
        LID,
        LID,
        None,
        "l1",
        1_700_000_000,
        text("hello"),
    ))
    .await;
    assert_eq!(b.store.chat(NET, LID).unwrap().unwrap().unread, 1);
    // A later one names the phone number.
    b.on_message(&inbound_with_alt(
        LID,
        LID,
        Some(ALEX),
        "l2",
        1_700_000_100,
        text("again"),
    ))
    .await;
    let chats = b.store.chats(NET).unwrap();
    assert_eq!(chats.len(), 1, "one person, one row: {chats:?}");
    assert_eq!((chats[0].id.as_str(), chats[0].unread), (ALEX, 2));
    let ids: Vec<String> = b
        .store
        .messages(NET, ALEX, 10)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["l1", "l2"]);
    assert!(b.store.chat(NET, LID).unwrap().is_none());
    drain(&mut erx);
}

// ---------------------------------------------------------------------------------------------
// #16 / #17 media: metadata and download
// ---------------------------------------------------------------------------------------------

fn image_with(path: &str, sha: &[u8], len: u64) -> wa::Message {
    wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            mimetype: Some("image/jpeg".into()),
            caption: Some("sunset".into()),
            file_length: Some(len),
            direct_path: Some(path.into()),
            media_key: Some(vec![9; 32]),
            file_enc_sha256: Some(vec![1; 32]),
            file_sha256: Some(sha.to_vec()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn sha(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

#[tokio::test]
async fn an_incoming_image_keeps_its_marker_and_gets_one_attachment() {
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "i1",
        false,
        1_700_000_000,
        image_with("/v/a", &[2; 32], 2048),
        "Alex Example",
    ))
    .await;
    let evs = drain(&mut erx);
    let Some(Out::Message(m)) = evs.iter().find(|e| matches!(e, Out::Message(_))) else {
        panic!("{evs:?}")
    };
    assert_eq!(m.text, "[image] sunset", "the marker stays for older code");
    assert_eq!(m.attachments.len(), 1);
    assert_eq!(
        (m.attachments[0].kind, m.attachments[0].size),
        (MediaKind::Image, Some(2048))
    );
    // It was stored, and no bytes were fetched or stored.
    let kept = b.store.message(NET, ALEX, "i1").unwrap().unwrap();
    assert_eq!(kept.attachments, m.attachments);
    assert!(!format!("{:?}", kept.attachments).contains("direct_path"));
}

#[test]
fn a_history_message_with_a_document_gets_an_attachment_and_a_sender_id() {
    let hs = wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: TEAM.into(),
            messages: vec![wa::HistorySyncMsg {
                message: MessageField::some(wa::WebMessageInfo {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("d1".into()),
                        from_me: Some(false),
                        participant: Some(SAM.into()),
                        ..Default::default()
                    }),
                    message: MessageField::some(wa::Message {
                        document_message: MessageField::some(wa::message::DocumentMessage {
                            file_name: Some("plan.pdf".into()),
                            mimetype: Some("application/pdf".into()),
                            file_length: Some(10),
                            direct_path: Some("/v/d".into()),
                            media_key: Some(vec![3; 32]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    message_timestamp: Some(100),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let batch = map::map_history(&hs, &Names::default(), 1_000_000_000_000);
    let m = &batch.messages[0];
    assert_eq!(m.text, "[document: plan.pdf]");
    assert_eq!(m.attachments.len(), 1);
    assert_eq!(m.attachments[0].name.as_deref(), Some("plan.pdf"));
    assert_eq!(m.sender_id, SAM);
}

/// A stored image message whose attachment downloads to `bytes` (hash matching).
fn image_in_store(b: &Backend, bytes: &[u8], declared: u64) {
    let fetch = rich::Fetch {
        kind: MediaKind::Image,
        direct_path: "/v/img".into(),
        media_key: vec![9; 32],
        enc_sha256: vec![1; 32],
        sha256: sha(bytes),
        length: declared,
    };
    b.store
        .upsert_messages(&[ChatMessage {
            text: "[image]".into(),
            attachments: vec![Attachment {
                kind: MediaKind::Image,
                name: None,
                mime: Some("image/jpeg".into()),
                size: Some(declared),
                fetch_ref: rich::fetch_ref(&fetch),
                open_url: None,
            }],
            ..stored(ALEX, "i1", 1000, false, "Alex Example")
        }])
        .unwrap();
}

fn fetch_cmd() -> Command {
    Command::FetchMedia {
        chat_id: ALEX.into(),
        message_id: "i1".into(),
        index: 0,
    }
}

#[tokio::test]
async fn nothing_downloads_until_the_user_asks_and_then_the_cache_answers() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let bytes = b"decrypted image bytes".to_vec();
    *fake.download.lock().unwrap() = Plan::Bytes(bytes.clone());
    image_in_store(&b, &bytes, bytes.len() as u64);
    b.on_command(Command::OpenChat {
        chat_id: ALEX.into(),
    })
    .await;
    assert!(fake.calls().is_empty(), "opening a chat fetches no media");

    b.on_command(fetch_cmd()).await;
    pump(&mut b, &mut rx).await;
    assert_eq!(fake.calls().len(), 1);
    let Call::Download(asked) = &fake.calls()[0] else {
        panic!()
    };
    assert_eq!(
        (asked.direct_path.as_str(), asked.kind),
        ("/v/img", MediaKind::Image)
    );
    let evs = drain(&mut erx);
    let Some(Out::MediaReady { path, index, .. }) =
        evs.iter().find(|e| matches!(e, Out::MediaReady { .. }))
    else {
        panic!("{evs:?}")
    };
    assert_eq!(*index, 0);
    assert!(
        path.starts_with(b.media.root()),
        "the file lives in the cache"
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.ends_with(".jpg") && !name.contains("sunset"),
        "named by hash: {name}"
    );

    // The second request is the cache's answer: no second download.
    b.on_command(fetch_cmd()).await;
    assert_eq!(fake.calls().len(), 1);
    assert!(matches!(&drain(&mut erx)[0], Out::MediaReady { .. }));
}

#[tokio::test]
async fn a_download_fails_with_a_reason_and_leaves_no_file() {
    let (mut b, mut erx, mut rx) = backend();
    // Not connected.
    image_in_store(&b, b"x", 1);
    b.on_command(fetch_cmd()).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MediaFailed { reason, .. } if reason.contains("not connected"))
    );
    let fake = linked(&mut b);

    // The CDN says the media is gone.
    *fake.download.lock().unwrap() = Plan::Expired;
    b.on_command(fetch_cmd()).await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MediaFailed { reason, fallback_url: None, .. }
        if reason.starts_with("MEDIA EXPIRED"))
    );

    // The bytes do not match the sender's hash: discarded, never cached.
    *fake.download.lock().unwrap() = Plan::Bytes(b"tampered".to_vec());
    b.on_command(fetch_cmd()).await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MediaFailed { reason, .. } if reason.contains("integrity"))
    );

    // A network error; its URL never reaches the reason.
    *fake.download.lock().unwrap() =
        Plan::Fail("failed for https://cdn.example.invalid/x?token=SECRET now".into());
    b.on_command(fetch_cmd()).await;
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    let Out::MediaFailed { reason, .. } = &evs[0] else {
        panic!()
    };
    assert!(
        !reason.contains("SECRET") && !reason.contains("https"),
        "{reason}"
    );

    let cached: Vec<_> = std::fs::read_dir(b.media.root().join("whatsapp"))
        .unwrap()
        .collect();
    assert!(cached.is_empty(), "no failed download leaves a file");

    // A bad index.
    b.on_command(Command::FetchMedia {
        chat_id: ALEX.into(),
        message_id: "i1".into(),
        index: 5,
    })
    .await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::MediaFailed { index: 5, .. }
    ));
}

#[tokio::test]
async fn a_file_over_the_limit_is_refused_before_any_download() {
    let settings = Settings {
        media: MediaSettings {
            max_file_mb: 1,
            ..MediaSettings::default()
        },
        ..Settings::default()
    };
    let (mut b, mut erx, _rx) = backend_with(settings);
    let fake = linked(&mut b);
    image_in_store(&b, b"x", 2 * 1024 * 1024);
    b.on_command(fetch_cmd()).await;
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::MediaFailed { reason, .. } if reason.starts_with("FILE OVER 1 MB"))
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn an_attachment_without_download_details_cannot_be_fetched() {
    let (mut b, mut erx, _rx) = backend();
    let fake = linked(&mut b);
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "i1",
        false,
        1000,
        image_with("", &[], 0),
        "Alex Example",
    ))
    .await;
    drain(&mut erx);
    b.on_command(fetch_cmd()).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MediaFailed { reason, .. } if reason.starts_with("MEDIA EXPIRED"))
    );
    assert!(fake.calls().is_empty());
}

// ---------------------------------------------------------------------------------------------
// #21 / #35 sending media
// ---------------------------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chatcli-wa-send-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn media_cmd(path: &std::path::Path, kind: MediaKind, caption: &str) -> Command {
    Command::SendMedia {
        chat_id: ALEX.into(),
        client_id: "m1".into(),
        path: path.to_path_buf(),
        kind,
        caption: caption.into(),
    }
}

#[tokio::test]
async fn an_image_is_re_encoded_uploaded_and_sent_with_its_caption() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let dir = temp_dir("png");
    let path = dir.join("pic.png");
    image::RgbaImage::new(4, 3).save(&path).unwrap();
    b.on_command(media_cmd(&path, MediaKind::Image, "sunset"))
        .await;
    pump(&mut b, &mut rx).await;

    let calls = fake.calls();
    assert!(
        matches!(&calls[0], Call::Upload { kind: MediaKind::Image, bytes: Some(n), path: None } if *n > 0),
        "an image goes up from memory, after re-encoding: {calls:?}"
    );
    let Call::Send(to, msg) = &calls[1] else {
        panic!("{calls:?}")
    };
    assert_eq!(to, &jid(ALEX));
    let img = msg.image_message.as_option().unwrap();
    assert_eq!(img.mimetype.as_deref(), Some("image/png"));
    assert_eq!((img.width, img.height), (Some(4), Some(3)));
    assert_eq!(img.caption.as_deref(), Some("sunset"));
    assert_eq!(img.direct_path.as_deref(), Some("/v/up"));

    let evs = drain(&mut erx);
    assert!(evs.iter().any(|e| matches!(e, Out::SendProgress { client_id, done: 100, total: 100, .. } if client_id == "m1")));
    let Some(Out::MessageConfirmed { client_id, message }) = evs
        .iter()
        .find(|e| matches!(e, Out::MessageConfirmed { .. }))
    else {
        panic!("{evs:?}")
    };
    assert_eq!(client_id, "m1");
    assert_eq!(message.text, "[image] sunset");
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].kind, MediaKind::Image);
    assert!(
        !message.attachments[0].fetch_ref.is_empty(),
        "our own picture can be opened again"
    );
    assert_eq!(chat(&b, ALEX).last_preview, "You: [image] sunset");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_document_streams_from_disk_and_goes_out_as_a_document() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let dir = temp_dir("pdf");
    let path = dir.join("plan.pdf");
    std::fs::write(&path, b"%PDF-1.4 not really").unwrap();
    b.on_command(media_cmd(&path, MediaKind::Document, ""))
        .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    assert!(
        matches!(&calls[0], Call::Upload { kind: MediaKind::Document, bytes: None, path: Some(p) } if p == &path)
    );
    let Call::Send(_, msg) = &calls[1] else {
        panic!("{calls:?}")
    };
    let doc = msg.document_message.as_option().unwrap();
    assert_eq!(
        doc.file_name.as_deref(),
        Some("plan.pdf"),
        "the name only, never our folder"
    );
    assert_eq!(doc.mimetype.as_deref(), Some("application/pdf"));
    assert_eq!(doc.file_length, Some(1234));
    let evs = drain(&mut erx);
    assert!(evs.iter().any(|e| matches!(e, Out::MessageConfirmed { message, .. }
        if message.text == "[document: plan.pdf]" && message.attachments[0].name.as_deref() == Some("plan.pdf"))));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn the_backend_decides_the_kind_by_content_and_sends_what_the_tui_confirmed() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let dir = temp_dir("kind");
    // Called .png but not a picture: a document, whatever the TUI guessed.
    let fake_png = dir.join("notes.png");
    std::fs::write(&fake_png, b"just text").unwrap();
    b.on_command(media_cmd(&fake_png, MediaKind::Image, ""))
        .await;
    pump(&mut b, &mut rx).await;
    assert!(matches!(
        &fake.calls()[0],
        Call::Upload {
            kind: MediaKind::Document,
            ..
        }
    ));
    // An executable type is sent: the confirmation happens in the TUI before the command.
    let exe = dir.join("tool.exe");
    std::fs::write(&exe, b"MZ").unwrap();
    b.on_command(media_cmd(&exe, MediaKind::Document, "")).await;
    pump(&mut b, &mut rx).await;
    assert!(fake.calls().iter().any(
        |c| matches!(c, Call::Send(_, m) if m.document_message.as_option()
        .is_some_and(|d| d.file_name.as_deref() == Some("tool.exe")))
    ));
    drain(&mut erx);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_bad_file_is_refused_before_any_network_call_and_stays_as_not_sent() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let dir = temp_dir("bad");
    // Missing.
    b.on_command(media_cmd(&dir.join("nope.pdf"), MediaKind::Document, ""))
        .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, client_id, .. }
        if error.starts_with("FILE NOT FOUND") && client_id == "m1")
    );
    // A folder.
    b.on_command(media_cmd(&dir, MediaKind::Document, "")).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, .. } if error.contains("folder"))
    );
    // Empty.
    let empty = dir.join("empty.txt");
    std::fs::write(&empty, b"").unwrap();
    b.on_command(media_cmd(&empty, MediaKind::Document, ""))
        .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, .. } if error.starts_with("FILE IS EMPTY"))
    );
    assert!(fake.calls().is_empty());
    // The failed attempt is stored, so it is still there after the chat is reopened.
    let kept = b.store.message(NET, ALEX, "m1").unwrap().unwrap();
    assert!(matches!(kept.state, MessageState::Failed(_)));

    // The upload itself fails: NOT SENT with the reason.
    let ok = dir.join("ok.txt");
    std::fs::write(&ok, b"hello").unwrap();
    fake.fail_with("upload refused");
    b.on_command(media_cmd(&ok, MediaKind::Document, "")).await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, .. } if error.contains("upload refused"))
    );
    assert!(
        !fake.calls().iter().any(|c| matches!(c, Call::Send(..))),
        "nothing was sent after a failed upload"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sending_media_while_not_connected_fails_without_touching_the_file() {
    let (mut b, mut erx, _rx) = backend();
    b.on_command(media_cmd(
        std::path::Path::new("whatever.pdf"),
        MediaKind::Document,
        "",
    ))
    .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, .. } if error.contains("not connected"))
    );
}

#[test]
fn file_checks_use_the_limit_for_the_kind() {
    let dir = temp_dir("limit");
    let big = dir.join("big.bin");
    std::fs::write(&big, vec![0u8; 3 * 1024 * 1024]).unwrap();
    let e = ops::check_file(&big, MediaKind::Document, 2 * 1024 * 1024).unwrap_err();
    assert_eq!(e, "FILE OVER 2 MB: WhatsApp takes files up to that size.");
    assert!(ops::check_file(&big, MediaKind::Document, 4 * 1024 * 1024).is_ok());
    // A picture's limit applies to the re-encoded result, not the raw file.
    assert!(ops::check_file(&big, MediaKind::Image, 2 * 1024 * 1024).is_ok());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn upload_progress_reaches_the_tui_at_most_four_times_a_second() {
    let (etx, mut erx) = mpsc::unbounded_channel();
    let progress = ops::progress_to(etx, ALEX.into(), "m1".into());
    for done in 0..1000u64 {
        progress(done, 1000);
    }
    progress(1000, 1000);
    let evs = drain(&mut erx);
    assert!(
        evs.len() <= 3,
        "a tight loop is one line plus the last: got {}",
        evs.len()
    );
    assert!(matches!(
        evs.last(),
        Some(Out::SendProgress {
            done: 1000,
            total: 1000,
            ..
        })
    ));
}

#[test]
fn a_verified_write_checks_the_hash_and_the_cache_limit() {
    let dir = temp_dir("cache");
    let settings = MediaSettings {
        max_file_mb: 1,
        ..MediaSettings::default()
    };
    let cache = MediaCache::open(&dir, &settings).unwrap();
    let r = chatcli_core::model::FetchRef::new("ref-a");
    let path = ops::write_verified(&cache, &r, "bin", b"hello", &sha(b"hello")).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
    let e = ops::write_verified(
        &cache,
        &chatcli_core::model::FetchRef::new("ref-b"),
        "bin",
        b"hello",
        &sha(b"other"),
    )
    .unwrap_err();
    assert!(e.contains("integrity"));
    // No hash from the sender: nothing to compare.
    assert!(ops::write_verified(
        &cache,
        &chatcli_core::model::FetchRef::new("ref-c"),
        "bin",
        b"x",
        &[]
    )
    .is_ok());
    // A body over the per-file limit is refused with the sentence from the cache.
    let big = vec![0u8; 2 * 1024 * 1024];
    let e = ops::write_verified(
        &cache,
        &chatcli_core::model::FetchRef::new("ref-d"),
        "bin",
        &big,
        &[],
    )
    .unwrap_err();
    assert!(e.starts_with("FILE OVER 1 MB"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------------------------
// #30 older messages from the phone
// ---------------------------------------------------------------------------------------------

fn older_cmd(chat: &str, ts: i64, id: &str) -> Command {
    Command::LoadOlder {
        chat_id: chat.into(),
        before_ts: ts,
        before_id: id.into(),
    }
}

fn outcome(touched: &[&str]) -> HistoryOutcome {
    HistoryOutcome {
        chats: touched.len(),
        touched: touched.iter().map(|s| s.to_string()).collect(),
        messages: 0,
        skipped: 0,
        push_names: Vec::new(),
        book_names: Vec::new(),
        links: Vec::new(),
        future_fixed: 0,
    }
}

#[tokio::test]
async fn older_messages_come_from_the_store_first_with_no_network_call() {
    let (mut b, mut erx, _rx) = backend();
    let fake = linked(&mut b);
    let rows: Vec<ChatMessage> = (1..=60)
        .map(|i| stored(ALEX, &format!("m{i:02}"), i * 1000, false, "Alex Example"))
        .collect();
    b.store.upsert_messages(&rows).unwrap();
    b.on_command(older_cmd(ALEX, 60_000, "m60")).await;
    let evs = drain(&mut erx);
    let Out::OlderMessages {
        messages, at_start, ..
    } = &evs[0]
    else {
        panic!("{evs:?}")
    };
    assert_eq!((messages.len(), *at_start), (50, false));
    assert_eq!(messages[0].id, "m10", "oldest first");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn with_nothing_older_stored_the_phone_is_asked_once_and_its_answer_is_merged() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.store
        .upsert_messages(&[stored(ALEX, "m10", 10_000, true, "You")])
        .unwrap();
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    pump(&mut b, &mut rx).await; // the request left
                                 // Scrolling again while it waits is the same request.
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "one request per user scroll window");
    let Call::History {
        chat,
        id,
        from_me,
        ts,
    } = &calls[0]
    else {
        panic!()
    };
    assert_eq!(
        (chat, id.as_str(), *from_me, *ts),
        (&jid(ALEX), "m10", true, 10_000)
    );
    assert!(drain(&mut erx).is_empty(), "nothing to show yet");

    // The phone answers through history sync: its messages are stored, then the page is sent.
    let older: Vec<ChatMessage> = (1..=3)
        .map(|i| stored(ALEX, &format!("o{i}"), i * 1000, false, "Alex Example"))
        .collect();
    b.store.upsert_messages(&older).unwrap();
    b.after_history(&outcome(&[ALEX]), true, Some("REQ1"));
    let evs = drain(&mut erx);
    let Out::OlderMessages {
        messages, at_start, ..
    } = &evs[0]
    else {
        panic!("{evs:?}")
    };
    assert_eq!((messages.len(), *at_start), (3, false));
    assert_eq!(messages[0].id, "o1");
    assert!(b.older.is_empty());
    // No duplicate rows: the same answer again changes nothing in the store.
    b.store.upsert_messages(&older).unwrap();
    assert_eq!(b.store.messages(NET, ALEX, 100).unwrap().len(), 4);
}

#[tokio::test]
async fn an_answer_with_nothing_older_marks_the_start_and_is_remembered() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.store
        .upsert_messages(&[stored(ALEX, "m10", 10_000, true, "You")])
        .unwrap();
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    pump(&mut b, &mut rx).await;
    // The answer names the request and carries nothing for this chat.
    b.after_history(&outcome(&[]), true, Some("REQ1"));
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::OlderMessages { messages, at_start: true, .. } if messages.is_empty())
    );
    // Asked again: answered from what is remembered, with no new request.
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::OlderMessages { at_start: true, .. }
    ));
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_silent_phone_gives_a_timeout_notice_and_allows_a_retry() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.store
        .upsert_messages(&[stored(ALEX, "m10", 10_000, true, "You")])
        .unwrap();
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    pump(&mut b, &mut rx).await; // the request left
    assert!(drain(&mut erx).is_empty());
    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err(), "29 s is not yet the deadline");
    tokio::time::advance(Duration::from_secs(2)).await;
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::OlderFailed { error, .. } if error.starts_with("PHONE DID NOT ANSWER"))
    );
    // A retry is a new request.
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    pump(&mut b, &mut rx).await;
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn an_older_request_that_cannot_be_sent_is_an_error_not_a_hang() {
    let (mut b, mut erx, mut rx) = backend();
    b.store
        .upsert_messages(&[stored(ALEX, "m10", 10_000, true, "You")])
        .unwrap();
    // Not connected.
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::OlderFailed { error, .. } if error.contains("not connected"))
    );
    let fake = linked(&mut b);
    // Nothing stored at all for another chat.
    b.on_command(older_cmd(SAM, 10_000, "x")).await;
    assert!(matches!(&drain(&mut erx)[0], Out::OlderFailed { .. }));
    // The request itself fails to leave.
    fake.fail_with("socket closed");
    b.on_command(older_cmd(ALEX, 10_000, "m10")).await;
    pump(&mut b, &mut rx).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::OlderFailed { error, .. } if error.contains("socket closed"))
    );
    assert!(b.older.is_empty(), "a retry is allowed");
}

// ---------------------------------------------------------------------------------------------
// #52 typing
// ---------------------------------------------------------------------------------------------

fn typing_on() -> Settings {
    let mut s = Settings::default();
    s.whatsapp.send_typing = true;
    s
}

/// Own typing and read receipts both off (they default to on).
fn quiet() -> Settings {
    let mut s = Settings::default();
    s.whatsapp.send_typing = false;
    s.whatsapp.send_read_receipts = false;
    s
}

fn typing_cmd(chat: &str, active: bool) -> Command {
    Command::Typing {
        chat_id: chat.into(),
        active,
    }
}

#[tokio::test]
async fn with_send_typing_off_no_chat_state_stanza_is_ever_sent() {
    let (mut b, _erx, _rx) = backend_with(quiet());
    let fake = linked(&mut b);
    for active in [true, false, true] {
        b.on_command(typing_cmd(ALEX, active)).await;
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn with_send_typing_on_composing_goes_out_once_per_five_seconds_per_chat() {
    let (mut b, _erx, _rx) = backend_with(typing_on());
    let fake = linked(&mut b);
    for _ in 0..5 {
        b.on_command(typing_cmd(ALEX, true)).await;
    }
    assert_eq!(
        fake.calls().len(),
        1,
        "keystrokes inside 5 s send nothing more"
    );
    // Another chat has its own clock.
    b.on_command(typing_cmd(SAM, true)).await;
    assert_eq!(fake.calls().len(), 2);
    tokio::time::advance(Duration::from_secs(4)).await;
    b.on_command(typing_cmd(ALEX, true)).await;
    assert_eq!(fake.calls().len(), 2);
    tokio::time::advance(Duration::from_secs(2)).await;
    b.on_command(typing_cmd(ALEX, true)).await;
    assert_eq!(fake.calls().len(), 3);
    assert!(matches!(&fake.calls()[2], Call::Typing(j, true) if j == &jid(ALEX)));
}

#[tokio::test(start_paused = true)]
async fn a_changed_setting_applies_without_a_restart() {
    let (mut b, _erx, _rx) = backend_with(quiet());
    let fake = linked(&mut b);
    b.on_command(typing_cmd(ALEX, true)).await;
    assert!(fake.calls().is_empty(), "send_typing is off here");
    b.on_command(Command::Settings(Arc::new(typing_on()))).await;
    b.on_command(typing_cmd(ALEX, true)).await;
    assert!(matches!(&fake.calls()[..], [Call::Typing(j, true)] if j == &jid(ALEX)));
    // And back off: the next chat's composing is dropped again.
    b.on_command(Command::Settings(Arc::new(quiet()))).await;
    b.on_command(typing_cmd(SAM, true)).await;
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn paused_follows_composing_on_send_and_is_never_sent_alone() {
    let (mut b, _erx, _rx) = backend_with(typing_on());
    let fake = linked(&mut b);
    b.on_command(typing_cmd(ALEX, false)).await;
    assert!(fake.calls().is_empty(), "nothing to pause");
    b.on_command(typing_cmd(ALEX, true)).await;
    b.on_command(typing_cmd(ALEX, false)).await;
    b.on_command(typing_cmd(ALEX, false)).await;
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert!(matches!(&calls[1], Call::Typing(_, false)));
}

#[tokio::test]
async fn typing_while_offline_sends_nothing_and_says_nothing() {
    let (mut b, mut erx, _rx) = backend_with(typing_on());
    b.on_command(typing_cmd(ALEX, true)).await;
    assert!(drain(&mut erx).is_empty());
}

fn presence(chat: &str, sender: &str, state: ChatPresence, from_me: bool) -> Event {
    let source = MessageSource {
        chat: chat.parse().unwrap(),
        sender: sender.parse().unwrap(),
        is_from_me: from_me,
        is_group: chat.ends_with("@g.us"),
        ..Default::default()
    };
    Event::ChatPresence(
        wev::ChatPresenceUpdate::builder()
            .source(source)
            .state(state)
            .media(ChatPresenceMedia::Text)
            .build(),
    )
}

#[tokio::test]
async fn incoming_typing_is_shown_named_and_cleared_by_paused_or_their_message() {
    let (mut b, mut erx, _rx) = backend();
    b.names.set_book("15550100002", "Sam Sample");
    b.on_event(&presence(TEAM, SAM, ChatPresence::Composing, false))
        .await;
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::Typing { chat_id, sender, active: true, .. }
        if chat_id == TEAM && sender == "Sam Sample")
    );
    b.on_event(&presence(TEAM, SAM, ChatPresence::Paused, false))
        .await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::Typing { active: false, .. }
    ));

    // Their message ends it too.
    b.on_event(&presence(TEAM, SAM, ChatPresence::Composing, false))
        .await;
    drain(&mut erx);
    b.on_message(&inbound(
        TEAM,
        SAM,
        "g1",
        false,
        1_700_000_000,
        text("done typing"),
        "Sam Sample",
    ))
    .await;
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::Typing { active: false, sender, .. } if sender == "Sam Sample"));
    assert!(evs.iter().any(|e| matches!(e, Out::Message(_))));

    // Our own typing from another device is not shown, and an unsupported chat is ignored.
    b.on_event(&presence(TEAM, OWN_JID, ChatPresence::Composing, true))
        .await;
    b.on_event(&presence(
        "status@broadcast",
        SAM,
        ChatPresence::Composing,
        false,
    ))
    .await;
    assert!(drain(&mut erx).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Logout, account, capabilities
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn logout_deletes_the_saved_contact_names_and_links() {
    let (mut b, mut erx, _rx) = backend();
    b.names.set_book("15550100001", "Alex Example");
    b.names.link("15550100001", "100200300");
    assert_eq!(b.flush_names(), 2);
    assert_eq!(b.store.names(NET).unwrap().len(), 2);
    b.on_command(Command::Logout).await;
    assert!(
        b.store.names(NET).unwrap().is_empty(),
        "no address-book name outlives the link"
    );
    assert_eq!(
        b.names.label_with(&jid(ALEX), None),
        "+15550100001",
        "and none in memory"
    );
    // The next run starts with none either.
    assert_eq!(b.flush_names(), 0);
    drain(&mut erx);
}

#[tokio::test]
async fn a_different_linked_account_clears_the_cached_chats() {
    let (mut b, mut erx, _rx) = backend();
    let _fake = linked(&mut b);
    b.bind_account();
    seed_chat(&b.store, 1000, "old account chat", 1);
    // Same account again: nothing happens.
    b.bind_account();
    assert!(b.store.chat(NET, ALEX).unwrap().is_some());
    // A new link with another number.
    b.store
        .set_meta("whatsapp.account", Some("15550100077"))
        .unwrap();
    b.bind_account();
    assert!(b.store.chats(NET).unwrap().is_empty());
    assert!(matches!(drain(&mut erx).last(), Some(Out::Chats(_, c)) if c.is_empty()));
}

#[tokio::test]
async fn online_announces_what_this_backend_can_do() {
    let (b, mut erx, _rx) = backend();
    b.emit_caps();
    let Out::Capabilities { caps, network } = &drain(&mut erx)[0] else {
        panic!()
    };
    assert_eq!(*network, NET);
    assert!(caps.reply && caps.react && caps.edit && caps.delete && caps.mentions);
    assert!(
        caps.send_image && caps.send_file && caps.fetch_media && caps.load_older && caps.typing
    );
    assert!(caps.pin && caps.mute && caps.hide);
    assert!(!caps.fetch_files, "Teams files only");
}

// ---------------------------------------------------------------------------------------------
// Read state and chat flags
// ---------------------------------------------------------------------------------------------

fn unread_chat(b: &Backend) {
    b.store
        .upsert_chats(&[ChatSummary {
            network: NET,
            id: TEAM.into(),
            title: "Team".into(),
            kind: ChatKind::Group,
            last_ts: Some(2000),
            last_preview: "x".into(),
            unread: 2,
            ..ChatSummary::blank(NET)
        }])
        .unwrap();
    b.store.bump_mentions(NET, TEAM).unwrap();
    for (id, ts) in [("u1", 1000), ("u2", 2000)] {
        b.store
            .upsert_messages(&[ChatMessage {
                sender_id: SAM.into(),
                ..stored(TEAM, id, ts, false, "Sam Sample")
            }])
            .unwrap();
    }
}

#[tokio::test]
async fn with_receipts_off_a_read_chat_is_marked_read_on_the_phone_without_receipts() {
    let (mut b, mut erx, mut rx) = backend_with(quiet());
    let fake = linked(&mut b);
    unread_chat(&b);
    b.on_command(Command::MarkRead {
        chat_id: TEAM.into(),
    })
    .await;
    assert_eq!(
        (chat(&b, TEAM).unread, chat(&b, TEAM).mentions),
        (0, 0),
        "cleared at once"
    );
    pump(&mut b, &mut rx).await;
    assert_eq!(fake.calls().len(), 1);
    assert!(matches!(&fake.calls()[0], Call::MarkRead(j) if j == &jid(TEAM)));
    assert!(
        !fake.calls().iter().any(|c| matches!(c, Call::Receipts(..))),
        "no receipts: the sender is not told"
    );
    drain(&mut erx);
}

#[tokio::test]
async fn read_receipts_go_out_by_default() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    unread_chat(&b);
    b.on_command(Command::MarkRead {
        chat_id: TEAM.into(),
    })
    .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    assert!(matches!(&calls[0], Call::MarkRead(_)));
    let Call::Receipts(chat, sender, ids) = &calls[1] else {
        panic!("{calls:?}")
    };
    assert_eq!((chat, sender.clone()), (&jid(TEAM), Some(jid(SAM))));
    assert_eq!(ids.len(), 2);
    drain(&mut erx);
}

#[tokio::test]
async fn sending_in_an_unread_chat_marks_it_read_on_the_phone() {
    let (mut b, mut erx, mut rx) = backend_with(quiet());
    let fake = linked(&mut b);
    seed_chat(&b.store, 1000, "x", 3);
    b.on_command(send_cmd(ALEX, "on it", "c1", None, vec![]))
        .await;
    assert_eq!(chat(&b, ALEX).unread, 0, "cleared at once");
    pump(&mut b, &mut rx).await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, Call::MarkRead(j) if j == &jid(ALEX))),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| matches!(c, Call::Send(..))),
        "{calls:?}"
    );
    drain(&mut erx);
}

#[tokio::test]
async fn sending_in_a_read_chat_costs_no_read_mark() {
    let (mut b, mut erx, mut rx) = backend_with(quiet());
    let fake = linked(&mut b);
    seed_chat(&b.store, 1000, "x", 0);
    b.on_command(send_cmd(ALEX, "on it", "c1", None, vec![]))
        .await;
    pump(&mut b, &mut rx).await;
    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| matches!(c, Call::MarkRead(_))),
        "{calls:?}"
    );
    drain(&mut erx);
}

#[tokio::test]
async fn a_refused_read_mark_restores_the_unread_count_and_says_so() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    unread_chat(&b);
    fake.fail_with("offline");
    b.on_command(Command::MarkRead {
        chat_id: TEAM.into(),
    })
    .await;
    drain(&mut erx);
    pump(&mut b, &mut rx).await;
    assert_eq!((chat(&b, TEAM).unread, chat(&b, TEAM).mentions), (2, 1));
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::ChatUpdated(c) if c.unread == 2)));
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Notice { is_error: true, .. })));
}

#[tokio::test]
async fn reading_offline_just_clears_the_local_count() {
    let (mut b, mut erx, _rx) = backend();
    unread_chat(&b);
    b.on_command(Command::MarkRead {
        chat_id: TEAM.into(),
    })
    .await;
    assert_eq!(chat(&b, TEAM).unread, 0);
    assert!(!drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::Notice { .. })));
}

#[tokio::test]
async fn pinning_updates_the_row_at_once_then_tells_the_phone_and_reverts_if_refused() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    seed_chat(&b.store, 1000, "x", 0);
    b.on_command(Command::SetChatFlag {
        chat_id: ALEX.into(),
        flag: ChatFlag::Pinned,
        on: true,
    })
    .await;
    assert!(matches!(&drain(&mut erx)[0], Out::ChatUpdated(c) if c.flags.pinned));
    pump(&mut b, &mut rx).await;
    assert!(matches!(&fake.calls()[0], Call::Flag(j, ChatFlag::Pinned, true) if j == &jid(ALEX)));
    assert!(b.store.chat_flags(NET, ALEX).unwrap().pinned);

    // Mute for good, then hide: both reach the phone.
    b.on_command(Command::SetChatFlag {
        chat_id: ALEX.into(),
        flag: ChatFlag::Muted,
        on: true,
    })
    .await;
    assert_eq!(
        b.store.chat_flags(NET, ALEX).unwrap().muted_until,
        Some(MUTE_FOREVER)
    );
    pump(&mut b, &mut rx).await;
    drain(&mut erx);

    // The phone refuses: back as it was, with a notice and the row sent again.
    fake.fail_with("app state busy");
    b.on_command(Command::SetChatFlag {
        chat_id: ALEX.into(),
        flag: ChatFlag::Hidden,
        on: true,
    })
    .await;
    assert!(b.store.chat_flags(NET, ALEX).unwrap().hidden);
    pump(&mut b, &mut rx).await;
    assert!(!b.store.chat_flags(NET, ALEX).unwrap().hidden);
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::ChatUpdated(c) if !c.flags.hidden)));
    assert!(evs.iter().any(
        |e| matches!(e, Out::Notice { is_error: true, text, .. } if text.contains("app state busy"))
    ));
    // The earlier pin survived the revert.
    assert!(b.store.chat_flags(NET, ALEX).unwrap().pinned);
}

#[tokio::test]
async fn a_flag_change_while_offline_is_refused_and_the_row_is_shown_unchanged() {
    let (mut b, mut erx, _rx) = backend();
    seed_chat(&b.store, 1000, "x", 0);
    b.on_command(Command::SetChatFlag {
        chat_id: ALEX.into(),
        flag: ChatFlag::Pinned,
        on: true,
    })
    .await;
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Notice { is_error: true, .. })));
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::ChatUpdated(c) if !c.flags.pinned)));
    assert!(!b.store.chat_flags(NET, ALEX).unwrap().pinned);
}

#[tokio::test]
async fn flags_from_the_phone_win_over_what_is_stored() {
    let (mut b, mut erx, _rx) = backend();
    seed_chat(&b.store, 1000, "x", 0);
    b.store
        .set_chat_flags(
            NET,
            ALEX,
            ChatFlags {
                pinned: true,
                ..ChatFlags::default()
            },
        )
        .unwrap();
    b.on_pin_update(&jid(ALEX), false);
    assert!(!b.store.chat_flags(NET, ALEX).unwrap().pinned);
    assert!(matches!(&drain(&mut erx)[0], Out::ChatUpdated(c) if !c.flags.pinned));
    b.on_mute_update(&jid(ALEX), Some(MUTE_FOREVER));
    b.on_archive_update(&jid(ALEX), true);
    let f = b.store.chat_flags(NET, ALEX).unwrap();
    assert_eq!((f.muted_until, f.hidden), (Some(MUTE_FOREVER), true));
    // Nothing changed: no event.
    drain(&mut erx);
    b.on_archive_update(&jid(ALEX), true);
    assert!(drain(&mut erx).is_empty());
    // A hidden-number address lands on the phone-number chat once linked.
    b.names.link("15550100001", "100200300");
    b.on_pin_update(&jid(LID), true);
    assert!(b.store.chat_flags(NET, ALEX).unwrap().pinned);
}

#[test]
fn mutes_from_the_phone_map_to_the_model() {
    let now = 1_000_000;
    assert_eq!(map::mute_until(false, Some(5_000_000), now), None);
    assert_eq!(map::mute_until(true, Some(-1), now), Some(MUTE_FOREVER));
    assert_eq!(map::mute_until(true, None, now), Some(MUTE_FOREVER));
    assert_eq!(map::mute_until(true, Some(2_000_000), now), Some(2_000_000));
    assert_eq!(
        map::mute_until(true, Some(500_000), now),
        None,
        "an end in the past is not a mute"
    );
}

// ---------------------------------------------------------------------------------------------
// Shutdown and plumbing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_aborts_running_transfers_within_five_seconds() {
    let (mut b, _erx, _rx) = backend();
    b.tasks.spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    let started = std::time::Instant::now();
    b.stop_tasks().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(b.tasks.is_empty());
}

#[tokio::test]
async fn the_command_loop_is_not_blocked_by_a_slow_operation() {
    let (mut b, mut erx, _rx) = backend();
    let fake = linked(&mut b);
    *fake.hang.lock().unwrap() = true;
    own_message(&b, ALEX, "mine", "x");
    // A reaction whose call never returns: the command returns at once and the next one runs.
    tokio::time::timeout(
        Duration::from_secs(2),
        b.on_command(reaction_cmd(ALEX, "mine", "\u{1F44D}", false)),
    )
    .await
    .expect("the command loop does not wait for the network");
    b.on_command(Command::Refresh).await;
    assert!(drain(&mut erx).iter().any(|e| matches!(e, Out::Chats(..))));
    b.stop_tasks().await;
}

/// Not a regression gate: prints how long one big history chunk takes to import, since each
/// message costs one indexed lookup (so an edited or deleted one is never put back).
/// Run: cargo test -p chatcli-whatsapp --lib history_import_speed -- --ignored --nocapture
#[test]
#[ignore = "timing only"]
fn history_import_speed() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    let msgs: Vec<wa::HistorySyncMsg> = (0..20_000u64)
        .map(|i| wa::HistorySyncMsg {
            message: MessageField::some(wa::WebMessageInfo {
                key: MessageField::some(wa::MessageKey {
                    id: Some(format!("h{i}")),
                    from_me: Some(false),
                    ..Default::default()
                }),
                message: MessageField::some(text("hello")),
                message_timestamp: Some(1_700_000_000 + i),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    let hs = wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: ALEX.into(),
            messages: msgs,
            ..Default::default()
        }],
        ..Default::default()
    };
    for round in ["first import", "same chunk again"] {
        let t = std::time::Instant::now();
        apply_history(&store, &Names::default(), &hs).unwrap();
        println!("{round}: 20000 messages in {:?}", t.elapsed());
    }
    let t = std::time::Instant::now();
    for i in 0..20_000 {
        store.message(NET, ALEX, &format!("h{i}")).unwrap();
    }
    println!("20000 lookups alone: {:?}", t.elapsed());
}

#[tokio::test]
async fn a_revoke_or_edit_from_another_group_member_leaves_the_message_alone() {
    use wa::message::protocol_message::Type;
    let (mut b, mut erx, _rx) = backend();
    group_with_sam(&mut b, &mut erx).await;
    // Alex forges a revoke and an edit that name Sam's message.
    for body in [
        protocol(Type::Revoke, "g1", None),
        protocol(Type::MessageEdit, "g1", Some("forged")),
    ] {
        b.on_message(&inbound(
            TEAM,
            ALEX,
            "f1",
            false,
            1_700_000_050,
            body,
            "Alex Example",
        ))
        .await;
    }
    // An own-device flag does not help a forger either: the stored message is not ours.
    b.on_message(&inbound(
        TEAM,
        ALEX,
        "f2",
        true,
        1_700_000_051,
        protocol(Type::Revoke, "g1", None),
        "",
    ))
    .await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!(
        (m.state, m.text.as_str(), m.edited),
        (MessageState::Sent, "the plan", false)
    );
    // The real author still can.
    b.on_message(&inbound(
        TEAM,
        SAM,
        "e2",
        false,
        1_700_000_052,
        protocol(Type::MessageEdit, "g1", Some("the new plan")),
        "Sam Sample",
    ))
    .await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!((m.text.as_str(), m.edited), ("the new plan", true));
    b.on_message(&inbound(
        TEAM,
        SAM,
        "d2",
        false,
        1_700_000_053,
        protocol(Type::Revoke, "g1", None),
        "Sam Sample",
    ))
    .await;
    let m = b.store.message(NET, TEAM, "g1").unwrap().unwrap();
    assert_eq!(m.state, MessageState::Deleted);
}

#[tokio::test]
async fn a_peer_cannot_revoke_a_message_of_ours() {
    use wa::message::protocol_message::Type;
    let (mut b, mut erx, _rx) = backend();
    own_message(&b, ALEX, "mine", "my words");
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "d1",
        false,
        1_700_000_200,
        protocol(Type::Revoke, "mine", None),
        "Alex Example",
    ))
    .await;
    let m = b.store.message(NET, ALEX, "mine").unwrap().unwrap();
    assert_eq!((m.state, m.text.as_str()), (MessageState::Sent, "my words"));
    drain(&mut erx);
}

#[tokio::test]
async fn a_file_that_lies_about_its_size_is_stopped_at_the_limit_while_downloading() {
    let settings = Settings {
        media: MediaSettings {
            max_file_mb: 1,
            ..MediaSettings::default()
        },
        ..Settings::default()
    };
    let (mut b, mut erx, mut rx) = backend_with(settings);
    let fake = linked(&mut b);
    // Declared length 0 passes the up-front check; the real bytes are 2 MiB.
    *fake.download.lock().unwrap() = Plan::Bytes(vec![7u8; 2 * 1024 * 1024]);
    image_in_store(&b, b"x", 0);
    b.on_command(fetch_cmd()).await;
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    assert!(
        evs.iter().any(
            |e| matches!(e, Out::MediaFailed { reason, .. } if reason.starts_with("FILE OVER 1 MB"))
        ),
        "{evs:?}"
    );
    assert!(!evs.iter().any(|e| matches!(e, Out::MediaReady { .. })));
}

// ---------------------------------------------------------------------------------------------
// Profile photos
// ---------------------------------------------------------------------------------------------

fn avatar_cmd(ids: &[String]) -> Command {
    Command::FetchAvatars {
        people: ids.to_vec(),
    }
}

fn phone(i: u32) -> String {
    format!("1555010{i:04}@s.whatsapp.net")
}

fn avatar_events(erx: &mut EventRx) -> Vec<(String, Option<PathBuf>)> {
    drain(erx)
        .into_iter()
        .filter_map(|e| match e {
            Out::Avatar {
                person_id, path, ..
            } => Some((person_id, path)),
            _ => None,
        })
        .collect()
}

/// Runs the backend like the run loop does until `count` answers came: a finished lookup is
/// handled, then the wake-up the rate limit asked for. The clock is paused, so waits are free.
async fn run_avatars(
    b: &mut Backend,
    erx: &mut EventRx,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
    count: usize,
) -> Vec<(String, Option<PathBuf>)> {
    let mut got = avatar_events(erx);
    while got.len() < count {
        if let Some(t) = b.avatar_at {
            if let Ok(m) = rx.try_recv() {
                b.on_msg(m).await;
            } else {
                tokio::time::sleep_until(t).await;
                b.schedule_avatar();
            }
        } else {
            let m = rx.recv().await.expect("channel open");
            b.on_msg(m).await;
        }
        got.extend(avatar_events(erx));
    }
    got
}

#[tokio::test(start_paused = true)]
async fn avatar_lookups_obey_ten_a_minute_and_two_seconds_apart() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let ids: Vec<String> = (0..15).map(phone).collect();
    let start = tokio::time::Instant::now();
    b.on_command(avatar_cmd(&ids)).await;
    let got = run_avatars(&mut b, &mut erx, &mut rx, 15).await;
    assert_eq!(got.len(), 15);
    assert!(got.iter().all(|(_, p)| p.is_some()));
    let at = fake.photo_at.lock().unwrap().clone();
    assert_eq!(at.len(), 15);
    let in_first_minute = at
        .iter()
        .filter(|t| t.duration_since(start) < Duration::from_secs(60))
        .count();
    assert_eq!(in_first_minute, 10);
    for w in at.windows(2) {
        assert!(w[1].duration_since(w[0]) >= Duration::from_secs(2));
    }
    assert!(at[10].duration_since(at[0]) >= Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn a_stored_photo_is_a_square_png_in_the_avatars_folder() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    b.on_command(avatar_cmd(&[phone(1)])).await;
    let got = run_avatars(&mut b, &mut erx, &mut rx, 1).await;
    assert!(matches!(&fake.calls()[..], [Call::Photo(j)] if j.to_string() == phone(1)));
    let path = got[0].1.clone().expect("a photo");
    assert_eq!(path.parent().unwrap(), b.media.root().join("avatars"));
    assert_eq!(image::open(&path).unwrap().width(), 96);
}

#[tokio::test(start_paused = true)]
async fn the_same_id_is_never_looked_up_twice_in_a_session() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    fake.fail_with("socket closed");
    b.on_command(avatar_cmd(&[phone(1)])).await;
    let got = run_avatars(&mut b, &mut erx, &mut rx, 1).await;
    assert_eq!(
        got[0].1, None,
        "a failure answers None and writes no marker"
    );
    assert_eq!(fake.calls().len(), 1);
    b.on_command(avatar_cmd(&[phone(1)])).await;
    assert_eq!(avatar_events(&mut erx).len(), 1, "answered at once");
    assert_eq!(fake.calls().len(), 1, "no second lookup");
}

#[tokio::test(start_paused = true)]
async fn junk_ids_groups_and_a_missing_link_answer_none_without_a_lookup() {
    let (mut b, mut erx, _rx) = backend();
    // Not connected: answered None, and not remembered as asked.
    b.on_command(avatar_cmd(&[phone(1)])).await;
    assert_eq!(avatar_events(&mut erx), vec![(phone(1), None)]);
    assert!(b.avatar_seen.is_empty());
    let fake = linked(&mut b);
    let ids: Vec<String> = [TEAM, "not a jid", "", "status@broadcast"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    b.on_command(avatar_cmd(&ids)).await;
    let got = avatar_events(&mut erx);
    assert_eq!(got.len(), 4);
    assert!(got.iter().all(|(_, p)| p.is_none()));
    assert!(fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_fresh_cache_entry_makes_no_lookup() {
    let (mut b, mut erx, _rx) = backend();
    let fake = linked(&mut b);
    let path = b.media.store_avatar(NET, &phone(1), &photo_png()).unwrap();
    b.media.store_no_avatar(NET, &phone(2)).unwrap();
    b.on_command(avatar_cmd(&[phone(1), phone(2)])).await;
    assert_eq!(
        avatar_events(&mut erx),
        vec![(phone(1), Some(path)), (phone(2), None)]
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_oversized_or_missing_photo_writes_the_marker() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    *fake.photo.lock().unwrap() = Err(crate::wa::AVATAR_TOO_BIG.to_string());
    b.on_command(avatar_cmd(&[phone(1)])).await;
    let got = run_avatars(&mut b, &mut erx, &mut rx, 1).await;
    assert_eq!(got[0].1, None);
    assert_eq!(
        b.media.avatar_lookup(NET, &phone(1), now_ms()),
        AvatarLookup::NoPhoto
    );
    *fake.photo.lock().unwrap() = Ok(None);
    b.on_command(avatar_cmd(&[phone(2)])).await;
    run_avatars(&mut b, &mut erx, &mut rx, 1).await;
    assert_eq!(
        b.media.avatar_lookup(NET, &phone(2), now_ms()),
        AvatarLookup::NoPhoto
    );
    // Bytes that are not a picture: the same marker.
    *fake.photo.lock().unwrap() = Ok(Some(b"junk".to_vec()));
    b.on_command(avatar_cmd(&[phone(3)])).await;
    run_avatars(&mut b, &mut erx, &mut rx, 1).await;
    assert_eq!(
        b.media.avatar_lookup(NET, &phone(3), now_ms()),
        AvatarLookup::NoPhoto
    );
}

#[tokio::test(start_paused = true)]
async fn logout_answers_the_waiting_people_with_none() {
    let (mut b, mut erx, _rx) = backend();
    let _fake = linked(&mut b);
    let ids: Vec<String> = (0..4).map(phone).collect();
    b.on_command(avatar_cmd(&ids)).await;
    assert!(avatar_events(&mut erx).is_empty(), "all four are pending");
    b.on_command(Command::Logout).await;
    let got = avatar_events(&mut erx);
    assert_eq!(got.len(), 3, "one lookup is in flight, three were waiting");
    assert!(got.iter().all(|(_, p)| p.is_none()));
    assert!(b.avatar_queue.is_empty() && b.avatar_at.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_lookup_answered_after_logout_writes_and_shows_nothing() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    let gate = Arc::new(tokio::sync::Notify::new());
    *fake.photo_gate.lock().unwrap() = Some(gate.clone());
    b.on_command(avatar_cmd(&[phone(1)])).await;
    b.on_command(Command::Logout).await;
    gate.notify_one();
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
        b.on_msg(m).await;
    }
    assert!(
        avatar_events(&mut erx).is_empty(),
        "no photo event after logout"
    );
    assert_eq!(
        b.media.avatar_lookup(NET, &phone(1), now_ms()),
        AvatarLookup::Missing
    );
}

#[tokio::test(start_paused = true)]
async fn the_avatar_queue_is_capped() {
    let (mut b, mut erx, _rx) = backend();
    let _fake = linked(&mut b);
    let ids: Vec<String> = (0..70).map(phone).collect();
    b.on_command(avatar_cmd(&ids)).await;
    // 64 fit, one of them already started; the other 6 were answered None at once.
    let got = avatar_events(&mut erx);
    assert_eq!(b.avatar_queue.len(), 63);
    assert_eq!(got.len(), 6);
    assert!(got.iter().all(|(_, p)| p.is_none()));
}

// ---------------------------------------------------------------------------------------------
// #71 read ticks and undelivered crosses
// ---------------------------------------------------------------------------------------------

fn receipt(chat: &str, reader: &str, ids: &[&str], kind: ReceiptType) -> Event {
    let source = MessageSource {
        chat: chat.parse().unwrap(),
        sender: reader.parse().unwrap(),
        is_from_me: false,
        is_group: chat.ends_with("@g.us"),
        ..Default::default()
    };
    Event::Receipt(
        wev::Receipt::builder()
            .source(source)
            .message_ids(ids.iter().map(|s| s.to_string()).collect())
            .timestamp(whatsapp_rust::chrono::DateTime::from_timestamp(1, 0).unwrap())
            .r#type(kind)
            .offline(false)
            .build(),
    )
}

/// Our messages "m1" at 1000 and "m2" at 2000, and their "t1" at 1500, in `chat`.
fn ours_and_theirs(b: &Backend, chat: &str) {
    b.store
        .upsert_messages(&[
            stored(chat, "m1", 1000, true, "You"),
            stored(chat, "t1", 1500, false, "Alex Example"),
            stored(chat, "m2", 2000, true, "You"),
        ])
        .unwrap();
}

/// As `ours_and_theirs`, with delivery tracked from 500 on (both of ours are after it).
fn tracked(b: &Backend, chat: &str) {
    ours_and_theirs(b, chat);
    b.store.set_delivery_baseline(NET, chat, 500).unwrap();
}

/// (chat, read, delivered) of every `Receipts` event.
type Marks = (String, Option<i64>, Option<i64>);

fn receipts(evs: &[Out]) -> Vec<Marks> {
    evs.iter()
        .filter_map(|e| match e {
            Out::Receipts {
                chat_id,
                read,
                delivered,
                ..
            } => Some((chat_id.clone(), *read, *delivered)),
            _ => None,
        })
        .collect()
}

fn marks(chat: &str, read: Option<i64>, delivered: Option<i64>) -> Vec<Marks> {
    vec![(chat.to_string(), read, delivered)]
}

#[tokio::test]
async fn receipts_in_a_direct_chat_move_forward_only_and_read_counts_as_delivered() {
    let (mut b, mut erx, _rx) = backend();
    tracked(&b, ALEX);
    // Played (a voice note) is not read: it comes even with read receipts off. Their own
    // message and an unknown id say nothing.
    b.on_event(&receipt(ALEX, ALEX, &["m2"], ReceiptType::Played))
        .await;
    b.on_event(&receipt(ALEX, ALEX, &["t1", "gone"], ReceiptType::Read))
        .await;
    assert!(receipts(&drain(&mut erx)).is_empty());

    b.on_event(&receipt(ALEX, ALEX, &["m1"], ReceiptType::Delivered))
        .await;
    assert_eq!(receipts(&drain(&mut erx)), marks(ALEX, None, Some(1000)));
    b.on_event(&receipt(ALEX, ALEX, &["m1"], ReceiptType::Read))
        .await;
    assert_eq!(
        receipts(&drain(&mut erx)),
        marks(ALEX, Some(1000), Some(1000))
    );
    // The newest of the ids wins, and a read moves delivered along.
    b.on_event(&receipt(ALEX, ALEX, &["m1", "m2"], ReceiptType::Read))
        .await;
    assert_eq!(
        receipts(&drain(&mut erx)),
        marks(ALEX, Some(2000), Some(2000))
    );
    // A late receipt for an older message never moves either back.
    b.on_event(&receipt(ALEX, ALEX, &["m1"], ReceiptType::Read))
        .await;
    b.on_event(&receipt(ALEX, ALEX, &["m1"], ReceiptType::Delivered))
        .await;
    assert!(receipts(&drain(&mut erx)).is_empty());

    // Opening the chat sends the marks after the messages.
    b.on_command(Command::OpenChat {
        chat_id: ALEX.into(),
    })
    .await;
    let evs = drain(&mut erx);
    assert!(matches!(evs[0], Out::Messages { .. }), "{evs:?}");
    assert_eq!(receipts(&evs), marks(ALEX, Some(2000), Some(2000)));
}

#[tokio::test]
async fn a_receipt_from_our_own_device_is_not_a_read_tick() {
    let (mut b, mut erx, _rx) = backend();
    b.names.set_own(vec![OWN_PN.to_string()]);
    tracked(&b, ALEX);
    b.on_event(&receipt(ALEX, OWN_JID, &["m2"], ReceiptType::Read))
        .await;
    assert!(receipts(&drain(&mut erx)).is_empty());
    assert!(b.store.read_marks(NET, ALEX).unwrap().is_empty());
}

#[tokio::test]
async fn the_chat_with_ourselves_never_gets_receipt_marks() {
    let (mut b, mut erx, _rx) = backend();
    b.names.set_own(vec![OWN_PN.to_string()]);
    // A note to self from the phone would start delivery tracking anywhere else.
    b.on_message(&inbound(
        OWN_JID,
        OWN_JID,
        "n1",
        true,
        1_700_000_000,
        text("note"),
        "",
    ))
    .await;
    assert!(receipts(&drain(&mut erx)).is_empty());
    b.on_command(Command::OpenChat {
        chat_id: OWN_JID.into(),
    })
    .await;
    assert!(receipts(&drain(&mut erx)).is_empty());
}

#[tokio::test]
async fn our_first_message_in_a_chat_starts_delivery_tracking_once() {
    let (mut b, mut erx, _rx) = backend();
    // Sent from the phone: everything before it counts as delivered, it does not.
    b.on_message(&inbound(
        ALEX,
        OWN_JID,
        "p1",
        true,
        1_700_000_000,
        text("hi"),
        "",
    ))
    .await;
    let at = 1_700_000_000_000;
    let evs = drain(&mut erx);
    assert_eq!(receipts(&evs), marks(ALEX, None, Some(at - 1)));
    // The TUI is told tracking starts there, so older messages get no state.
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Receipts { since, .. } if *since == Some(at - 1))));
    // A second one is past the same baseline: nothing new to say.
    b.on_message(&inbound(
        ALEX,
        OWN_JID,
        "p2",
        true,
        1_700_000_010,
        text("there"),
        "",
    ))
    .await;
    assert!(receipts(&drain(&mut erx)).is_empty());
    assert_eq!(
        b.store.delivered_marks(NET, ALEX).unwrap(),
        [(String::new(), at - 1)]
    );
}

#[tokio::test]
async fn a_first_receipt_counts_everything_stored_before_it_as_delivered() {
    let (mut b, mut erx, _rx) = backend();
    // From before delivery was tracked: m2 has no receipt and must not look undelivered.
    ours_and_theirs(&b, ALEX);
    b.on_event(&receipt(ALEX, ALEX, &["m1"], ReceiptType::Read))
        .await;
    let got = receipts(&drain(&mut erx));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, Some(1000));
    assert!(got[0].2.is_some_and(|d| d >= 2000), "{got:?}");
}

#[tokio::test]
async fn a_group_is_read_and_delivered_once_every_member_is_by_either_address() {
    let (mut b, mut erx, mut rx) = backend();
    let fake = linked(&mut b);
    *fake.members.lock().unwrap() = vec![(LID.into(), Some(ALEX.into())), (SAM.into(), None)];
    // The hidden address of Alex is known to be Alex's phone number.
    b.names.link("15550100001", "100200300");
    tracked(&b, TEAM);

    // Alex reads by phone number. The members are fetched once, quietly (no Members event for
    // the TUI); then Sam has nothing past the baseline, so nothing is read and only what came
    // before tracking counts as delivered.
    b.on_event(&receipt(TEAM, ALEX, &["m2"], ReceiptType::Read))
        .await;
    assert!(receipts(&drain(&mut erx)).is_empty());
    pump(&mut b, &mut rx).await;
    let evs = drain(&mut erx);
    assert!(!evs.iter().any(|e| matches!(e, Out::Members { .. })));
    assert_eq!(receipts(&evs), marks(TEAM, None, Some(500)));
    assert_eq!(b.store.members(NET, TEAM, 10).unwrap().len(), 2);

    // Sam's phone gets the older one, then Sam reads it, then the newer one.
    b.on_event(&receipt(TEAM, SAM, &["m1"], ReceiptType::Delivered))
        .await;
    assert_eq!(receipts(&drain(&mut erx)), marks(TEAM, None, Some(1000)));
    b.on_event(&receipt(TEAM, SAM, &["m1"], ReceiptType::Read))
        .await;
    assert_eq!(
        receipts(&drain(&mut erx)),
        marks(TEAM, Some(1000), Some(1000))
    );
    b.on_event(&receipt(TEAM, SAM, &["m2"], ReceiptType::Read))
        .await;
    assert_eq!(
        receipts(&drain(&mut erx)),
        marks(TEAM, Some(2000), Some(2000))
    );
    let fetches = fake
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Members(_)))
        .count();
    assert_eq!(fetches, 1, "the member list is fetched once a run");
}
