//! Backend behaviour against an in-memory store, with library events built by hand (no network).

use super::*;
use chatcli_core::model::{ChatKind, EventRx};
use whatsapp_rust::wacore::types::events as wev;
use whatsapp_rust::wacore::types::message::MessageInfo;

pub(super) const ALEX: &str = "15550100001@s.whatsapp.net";
pub(super) const SAM: &str = "15550100002@s.whatsapp.net";
pub(super) const TEAM: &str = "120363000000000000@g.us";

/// A backend context over `store`: default settings and a media cache in its own temp folder.
pub(super) fn ctx_for(store: Store, settings: Settings) -> BackendCtx {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("chatcli-wa-media-{}-{n}", std::process::id()));
    let media = MediaCache::open(&dir, &settings.media).unwrap();
    BackendCtx {
        store,
        settings: Arc::new(settings),
        media,
    }
}

pub(super) fn backend() -> (Backend, EventRx, mpsc::UnboundedReceiver<Msg>) {
    backend_with(Settings::default())
}

pub(super) fn backend_with(settings: Settings) -> (Backend, EventRx, mpsc::UnboundedReceiver<Msg>) {
    crate::test_data_dir();
    let (etx, erx) = mpsc::unbounded_channel();
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Backend::new(ctx_for(Store::open_in_memory().unwrap(), settings), etx, tx),
        erx,
        rx,
    )
}

pub(super) fn drain(rx: &mut EventRx) -> Vec<Out> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

pub(super) fn inbound(
    chat: &str,
    sender: &str,
    id: &str,
    from_me: bool,
    ts: i64,
    body: wa::Message,
    push: &str,
) -> InboundMessage {
    let source = whatsapp_rust::wacore::types::message::MessageSource {
        chat: chat.parse().unwrap(),
        sender: sender.parse().unwrap(),
        is_from_me: from_me,
        is_group: chat.ends_with("@g.us"),
        ..Default::default()
    };
    let info = MessageInfo {
        id: id.to_string(),
        push_name: push.to_string(),
        timestamp: whatsapp_rust::chrono::DateTime::from_timestamp(ts, 0).unwrap(),
        source,
        ..Default::default()
    };
    InboundMessage::builder()
        .message(Arc::new(body))
        .info(Arc::new(info))
        .build()
}

pub(super) fn text(t: &str) -> wa::Message {
    wa::Message {
        conversation: Some(t.to_string()),
        ..Default::default()
    }
}

pub(super) fn chat(b: &Backend, id: &str) -> ChatSummary {
    b.store.chat(NET, id).unwrap().expect("chat stored")
}

pub(super) fn seed_chat(store: &Store, last_ts: i64, preview: &str, unread: u32) {
    store
        .upsert_chats(&[ChatSummary {
            network: NET,
            id: ALEX.into(),
            title: "Alex Example".into(),
            kind: ChatKind::Direct,
            last_ts: Some(last_ts),
            last_preview: preview.into(),
            unread,
            ..ChatSummary::blank(NET)
        }])
        .unwrap();
}

#[tokio::test]
async fn incoming_direct_message_creates_chat_and_counts_unread_once() {
    let (mut b, mut erx, _rx) = backend();
    let m = inbound(
        ALEX,
        ALEX,
        "m1",
        false,
        1_700_000_000,
        text("is the build done?"),
        "Alex Example",
    );
    b.on_message(&m).await;
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::Message(x) if x.text == "is the build done?" && x.sender == "Alex Example")
    );
    assert!(matches!(&evs[1], Out::ChatUpdated(c) if c.unread == 1 && c.title == "Alex Example"));
    let c = chat(&b, ALEX);
    assert_eq!(
        (c.unread, c.last_ts, c.kind),
        (1, Some(1_700_000_000_000), ChatKind::Direct)
    );
    assert_eq!(c.last_preview, "is the build done?");

    // The same message delivered again (offline queue) is not unread twice.
    b.on_message(&m).await;
    assert_eq!(chat(&b, ALEX).unread, 1);

    // Our own message from another device: preview names us, unread unchanged.
    let own = inbound(
        ALEX,
        SAM,
        "m2",
        true,
        1_700_000_100,
        text("yes, shipped"),
        "",
    );
    b.on_message(&own).await;
    let c = chat(&b, ALEX);
    assert_eq!(
        (c.unread, c.last_preview.as_str()),
        (1, "You: yes, shipped")
    );
    assert_eq!(b.store.messages(NET, ALEX, 10).unwrap().len(), 2);
}

#[tokio::test]
async fn group_message_gets_sender_prefix_and_old_messages_do_not_rewind() {
    let (mut b, _erx, _rx) = backend();
    b.on_message(&inbound(
        TEAM,
        SAM,
        "g2",
        false,
        2000,
        text("newer"),
        "Sam Sample",
    ))
    .await;
    b.on_message(&inbound(
        TEAM,
        SAM,
        "g1",
        false,
        1000,
        text("older"),
        "Sam Sample",
    ))
    .await;
    let c = chat(&b, TEAM);
    assert_eq!(c.kind, ChatKind::Group);
    assert_eq!(c.title, "Group", "no session to ask for the subject");
    assert_eq!(
        (c.last_ts, c.last_preview.as_str(), c.unread),
        (Some(2_000_000), "Sam Sample: newer", 2)
    );
    assert!(b.sender_of(TEAM, "g1").is_some());
}

#[tokio::test]
async fn out_of_scope_chats_and_plumbing_are_ignored() {
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(
        "status@broadcast",
        ALEX,
        "s1",
        false,
        1,
        text("status"),
        "",
    ))
    .await;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "r1",
        false,
        1,
        wa::Message::default(),
        "",
    ))
    .await;
    assert!(drain(&mut erx).is_empty());
    assert!(b.store.chats(NET).unwrap().is_empty());
}

#[tokio::test]
async fn edit_and_delete_update_the_stored_message() {
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
    drain(&mut erx);
    let proto = |t: Type, edited: Option<wa::Message>| wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("m1".into()),
                ..Default::default()
            }),
            r#type: Some(t),
            edited_message: edited.map(MessageField::some).unwrap_or_default(),
            ..Default::default()
        }),
        ..Default::default()
    };
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "e1",
        false,
        1001,
        proto(Type::MessageEdit, Some(text("the build"))),
        "",
    ))
    .await;
    let m = &b.store.messages(NET, ALEX, 10).unwrap()[0];
    assert_eq!((m.text.as_str(), m.edited), ("the build", true));
    assert_eq!(chat(&b, ALEX).last_preview, "the build");
    // An edit changes a message that is already there: it never raises a notification.
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(x) if x.id == "m1" && x.edited)));
    assert!(!evs.iter().any(|e| matches!(e, Out::Message(_))));

    b.on_message(&inbound(
        ALEX,
        ALEX,
        "d1",
        false,
        1002,
        proto(Type::Revoke, None),
        "",
    ))
    .await;
    let m = &b.store.messages(NET, ALEX, 10).unwrap()[0];
    assert_eq!(
        (m.state.clone(), m.text.as_str()),
        (MessageState::Deleted, ""),
        "a deleted message has no text: the pane draws [message deleted] itself"
    );
    assert_eq!(chat(&b, ALEX).last_preview, "[message deleted]");
    assert!(drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::MessageChanged(x) if x.state == MessageState::Deleted)));
    // The edit and revoke stanzas themselves never become messages.
    assert_eq!(b.store.messages(NET, ALEX, 10).unwrap().len(), 1);

    // A target outside the store is ignored, not an error.
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "d2",
        false,
        1003,
        proto(Type::Revoke, None),
        "",
    ))
    .await;
}

#[tokio::test]
async fn send_outcomes() {
    let (mut b, mut erx, _rx) = backend();
    b.on_command(Command::Send {
        chat_id: ALEX.into(),
        text: "hi".into(),
        client_id: "c1".into(),
        reply_to: None,
        mentions: vec![],
    })
    .await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { client_id, error, .. }
        if client_id == "c1" && error.contains("not connected"))
    );
    let kept = b.store.messages(NET, ALEX, 10).unwrap();
    assert_eq!((kept[0].id.as_str(), kept[0].text.as_str()), ("c1", "hi"));
    assert!(
        matches!(&kept[0].state, MessageState::Failed(_)),
        "NOT SENT survives a reopen"
    );

    let ok = Sent {
        chat_id: ALEX.into(),
        client_id: "c2".into(),
        draft: b.draft(ALEX, "c2", "yes"),
        result: Ok("REALID".into()),
    };
    b.on_msg(Msg::Sent(Box::new(ok))).await;
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[0], Out::MessageConfirmed { client_id, message } if client_id == "c2" && message.id == "REALID")
    );
    assert_eq!(chat(&b, ALEX).last_preview, "You: yes");
    assert!(b
        .store
        .messages(NET, ALEX, 10)
        .unwrap()
        .iter()
        .any(|m| m.id == "REALID"));

    let bad = Sent {
        chat_id: ALEX.into(),
        client_id: "c3".into(),
        draft: b.draft(ALEX, "c3", "no"),
        result: Err("not logged in".into()),
    };
    b.on_msg(Msg::Sent(Box::new(bad))).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::MessageFailed { error, .. } if error == "not logged in")
    );
}

#[tokio::test]
async fn mark_read_and_open_chat() {
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "m1",
        false,
        1000,
        text("hello"),
        "Alex Example",
    ))
    .await;
    drain(&mut erx);
    b.on_command(Command::MarkRead {
        chat_id: ALEX.into(),
    })
    .await;
    assert!(matches!(&drain(&mut erx)[0], Out::ChatUpdated(c) if c.unread == 0));
    assert_eq!(chat(&b, ALEX).unread, 0);
    b.on_command(Command::OpenChat {
        chat_id: ALEX.into(),
    })
    .await;
    assert!(matches!(&drain(&mut erx)[0], Out::Messages { messages, .. } if messages.len() == 1));
}

#[tokio::test]
async fn link_state_events() {
    let (mut b, mut erx, _rx) = backend();
    let qr = wev::PairingQrCode::builder()
        .code("not-a-real-code".to_string())
        .timeout(Duration::from_secs(20))
        .build();
    b.on_event(&Event::PairingQrCode(qr)).await;
    assert!(
        matches!(&drain(&mut erx)[0], Out::Status(_, Status::Qr { code }) if code == "not-a-real-code")
    );

    b.on_event(&Event::Connected(wev::Connected::builder().build()))
        .await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::Status(
            _,
            Status::Online {
                live: true,
                detail: None
            }
        )
    ));

    // Unlinked from the phone: sign-in needed with a reason, a notice, no reconnect.
    let l = wev::LoggedOut::builder()
        .on_connect(false)
        .reason(ConnectFailureReason::LoggedOut)
        .build();
    b.on_event(&Event::LoggedOut(l)).await;
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::Notice { is_error: true, .. }));
    assert!(
        matches!(&evs[1], Out::Status(_, Status::NeedsLogin { reason: Some(r) }) if r.contains("unlinked"))
    );
    assert!(b.session.is_none());

    b.on_event(&Event::StreamReplaced(
        wev::StreamReplaced::builder().build(),
    ))
    .await;
    assert!(
        matches!(&drain(&mut erx)[1], Out::Status(_, Status::NeedsLogin { reason: Some(r) }) if r.contains("replaced"))
    );
}

#[tokio::test]
async fn stuck_connect_is_shown_and_disconnect_only_flips_a_live_link() {
    let (mut b, mut erx, _rx) = backend();
    b.set_status(Status::Connecting);
    b.connect_timeout();
    let evs = drain(&mut erx);
    assert!(
        matches!(&evs[1], Out::Status(_, Status::Online { live: false, detail: Some(d) }) if d.contains("cannot reach"))
    );
    // Once online, the watchdog does nothing.
    b.set_status(Status::Online {
        live: true,
        detail: None,
    });
    drain(&mut erx);
    b.connect_timeout();
    assert!(drain(&mut erx).is_empty());

    // A disconnect during QR pairing must not turn the QR panel into NOT LIVE.
    b.set_status(Status::LoggingIn);
    drain(&mut erx);
    let d = wev::Disconnected::builder()
        .reason(whatsapp_rust::transport::DisconnectReason::Unknown)
        .build();
    b.on_event(&Event::Disconnected(d.clone())).await;
    assert!(drain(&mut erx).is_empty());
    b.set_status(Status::Online {
        live: true,
        detail: None,
    });
    drain(&mut erx);
    b.on_event(&Event::Disconnected(d)).await;
    assert!(matches!(
        &drain(&mut erx)[0],
        Out::Status(_, Status::Online { live: false, .. })
    ));
}

#[tokio::test]
async fn events_from_a_stopped_session_are_ignored() {
    let (mut b, mut erx, _rx) = backend();
    b.gen = 5;
    let old = Arc::new(Event::Connected(wev::Connected::builder().build()));
    b.on_msg(Msg::Wa(4, old.clone())).await;
    assert!(drain(&mut erx).is_empty());
    b.on_msg(Msg::Wa(5, old)).await;
    // Online, then what this backend can do.
    let evs = drain(&mut erx);
    assert_eq!(evs.len(), 2);
    assert!(matches!(
        &evs[0],
        Out::Status(_, Status::Online { live: true, .. })
    ));
    assert!(matches!(&evs[1], Out::Capabilities { caps, .. } if caps.react && caps.load_older));
}

#[tokio::test]
async fn names_retitle_chats_after_a_pause() {
    let (mut b, mut erx, _rx) = backend();
    b.on_message(&inbound(ALEX, ALEX, "m1", false, 1000, text("hi"), ""))
        .await;
    assert_eq!(chat(&b, ALEX).title, "+15550100001");
    drain(&mut erx);
    let upd = wev::PushNameUpdate::builder()
        .jid(ALEX.parse().unwrap())
        .message(Box::new(MessageInfo::default()))
        .old_push_name(String::new())
        .new_push_name("Alex Example".to_string())
        .build();
    b.on_event(&Event::PushNameUpdate(upd)).await;
    assert!(
        b.retitle_at.is_some(),
        "retitle is scheduled, not run per name"
    );
    b.retitle();
    assert_eq!(chat(&b, ALEX).title, "Alex Example");
    assert!(matches!(&drain(&mut erx)[0], Out::Chats(_, c) if c[0].title == "Alex Example"));
}

#[test]
fn history_chunk_lands_in_the_store_without_rewinding_chats() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    seed_chat(&store, 9_000_000, "newest", 3);
    // The chat's newest message is stored (live messages always are), and newer than history.
    store
        .upsert_messages(&[stored(ALEX, "live", 9_000_000, false, "Alex Example")])
        .unwrap();
    let msg = |id: &str, ts: u64| wa::HistorySyncMsg {
        message: MessageField::some(wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some(id.into()),
                from_me: Some(false),
                ..Default::default()
            }),
            message: MessageField::some(text("old")),
            message_timestamp: Some(ts),
            ..Default::default()
        }),
        ..Default::default()
    };
    let hs = wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: ALEX.into(),
            messages: vec![msg("a", 100), msg("b", 200)],
            unread_count: Some(0),
            ..Default::default()
        }],
        ..Default::default()
    };
    let o = apply_history(&store, &Names::default(), &hs).unwrap();
    assert_eq!((o.chats, o.messages), (1, 2));
    let c = store.chat(NET, ALEX).unwrap().unwrap();
    assert_eq!(
        (c.title.as_str(), c.last_ts, c.unread),
        ("Alex Example", Some(9_000_000), 3)
    );
    assert_eq!(store.messages(NET, ALEX, 10).unwrap().len(), 3);
}

#[tokio::test]
async fn session_files_are_removed_and_missing_ones_are_fine() {
    crate::test_data_dir();
    let dir = std::env::temp_dir().join(format!("chatcli-whatsapp-del-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("whatsapp.db");
    for suffix in ["", "-wal", "-shm"] {
        std::fs::write(dir.join(format!("whatsapp.db{suffix}")), b"x").unwrap();
    }
    assert!(!remove_db_files(&db).await, "nothing left");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    assert!(!remove_db_files(&db).await, "already gone is not an error");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unlinked_reason_text() {
    assert!(unlinked_reason(&ConnectFailureReason::LoggedOut).contains("unlinked"));
    assert!(unlinked_reason(&ConnectFailureReason::AccountLocked).contains("locked"));
}

#[tokio::test]
async fn run_without_a_session_waits_for_login_and_shuts_down() {
    crate::test_data_dir();
    let (etx, mut erx) = mpsc::unbounded_channel();
    let (ctx, crx) = mpsc::unbounded_channel();
    let store = Store::open_in_memory().unwrap();
    seed_chat(&store, 1, "cached", 0);
    ctx.send(Command::Shutdown).unwrap();
    run(ctx_for(store, Settings::default()), crx, etx).await;
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::Status(_, Status::Starting)));
    assert!(matches!(&evs[1], Out::Chats(_, c) if c.len() == 1 && c[0].last_preview == "cached"));
    assert!(matches!(
        &evs[2],
        Out::Status(_, Status::NeedsLogin { reason: None })
    ));
}

// ---- bug 1: future-dated messages ----------------------------------------------------------

pub(super) const LID: &str = "100200300@lid";

pub(super) fn stored(chat_id: &str, id: &str, ts: i64, from_me: bool, sender: &str) -> ChatMessage {
    ChatMessage {
        network: NET,
        chat_id: chat_id.into(),
        id: id.into(),
        ts,
        sender: sender.into(),
        from_me,
        text: format!("text {id}"),
        state: MessageState::Sent,
        ..ChatMessage::blank(NET)
    }
}

pub(super) fn seed_chat_as(store: &Store, id: &str, title: &str, last_ts: Option<i64>) {
    store
        .upsert_chats(&[ChatSummary {
            network: NET,
            id: id.into(),
            title: title.into(),
            kind: if id.ends_with("@g.us") {
                ChatKind::Group
            } else {
                ChatKind::Direct
            },
            last_ts,
            last_preview: "stale".into(),
            unread: 0,
            ..ChatSummary::blank(NET)
        }])
        .unwrap();
}

/// The shape seen in real use: own messages years ahead sit at the top of the chat list. After the
/// start-up repair the chats sort by real activity again, nothing is deleted, and a second run
/// changes nothing.
#[test]
fn start_up_repair_unfloats_chats_and_is_idempotent() {
    let (mut b, _erx, _rx) = backend();
    let now = now_ms();
    let real = now - 3_600_000;
    let future = 2_057_119_831_000;
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(future));
    seed_chat_as(&b.store, TEAM, "Project team", Some(2_459_086_227_000));
    seed_chat_as(&b.store, SAM, "Sam Sample", Some(now - 10_000));
    b.store
        .upsert_messages(&[
            stored(ALEX, "a1", real, false, "Alex Example"),
            stored(ALEX, "a2", future, true, "You"),
            stored(TEAM, "t1", real - 5_000, false, "Sam Sample"),
            stored(TEAM, "t2", 2_459_086_174_000, true, "You"),
            stored(TEAM, "t3", 2_459_086_227_000, true, "You"),
            stored(SAM, "s1", now - 10_000, false, "Sam Sample"),
        ])
        .unwrap();

    b.repair_future_dates();

    let chats = b.store.chats(NET).unwrap();
    assert_eq!(chats.len(), 3, "no chat lost");
    assert_eq!(
        chats[0].id, SAM,
        "the genuinely newest chat is on top again"
    );
    assert!(chats
        .iter()
        .all(|c| !map::is_future(c.last_ts.unwrap(), now_ms())));
    let alex = chat(&b, ALEX);
    assert_eq!(alex.last_ts, Some(real + 1));
    assert_eq!(
        alex.last_preview, "You: text a2",
        "preview follows the newest message"
    );
    let team = b.store.messages(NET, TEAM, 10).unwrap();
    assert_eq!(team.len(), 3, "no message deleted");
    assert_eq!(
        team.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["t1", "t2", "t3"],
        "order of the moved messages kept"
    );

    let before = b.store.chats(NET).unwrap();
    b.repair_future_dates();
    assert_eq!(b.store.chats(NET).unwrap(), before);
}

/// A chat that only has a future time and no messages (history sent a conversation time) would
/// sink to the bottom rather than float to the top.
#[test]
fn start_up_repair_clears_a_future_chat_time_with_no_messages() {
    let (mut b, _erx, _rx) = backend();
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(2_352_421_960_000));
    b.repair_future_dates();
    let c = chat(&b, ALEX);
    assert_eq!((c.last_ts, c.last_preview.as_str()), (None, ""));
}

#[tokio::test]
async fn live_message_with_a_future_time_does_not_float_the_chat() {
    let (mut b, _erx, _rx) = backend();
    let before = now_ms();
    b.on_message(&inbound(
        ALEX,
        SAM,
        "own1",
        true,
        2_057_119_831,
        text("sent from another device"),
        "",
    ))
    .await;
    let c = chat(&b, ALEX);
    let ts = c.last_ts.unwrap();
    assert!(
        ts >= before && !map::is_future(ts, now_ms()),
        "now, not 2035"
    );
    assert_eq!(b.store.messages(NET, ALEX, 10).unwrap()[0].ts, ts);
}

/// A chat row that is already ahead (written before the fix) must not block newer messages
/// from becoming its last one.
#[tokio::test]
async fn a_stored_future_chat_time_does_not_block_newer_messages() {
    let (mut b, _erx, _rx) = backend();
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(2_057_119_831_000));
    let now = now_ms() / 1000;
    b.on_message(&inbound(
        ALEX,
        ALEX,
        "m1",
        false,
        now,
        text("hi"),
        "Alex Example",
    ))
    .await;
    let c = chat(&b, ALEX);
    assert_eq!(
        (c.last_ts, c.last_preview.as_str()),
        (Some(now * 1000), "hi")
    );
}

/// A history chunk delivering the same messages again overwrites the approximated rows with the
/// faithful ones (receipt time), so a re-sync is the better repair when one happens.
#[test]
fn history_sync_replaces_stored_future_rows_with_receipt_times() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    let real = 1_794_000_000_i64;
    seed_chat_as(&store, ALEX, "Alex Example", Some(real * 1000 + 1));
    store
        .upsert_messages(&[stored(ALEX, "own-bad", real * 1000 + 1, true, "You")])
        .unwrap();
    let mut bad = wa::HistorySyncMsg {
        message: MessageField::some(wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("own-bad".into()),
                from_me: Some(true),
                ..Default::default()
            }),
            message: MessageField::some(text("hello")),
            message_timestamp: Some(2_173_107_337),
            user_receipt: vec![wa::UserReceipt {
                user_jid: ALEX.into(),
                receipt_timestamp: Some(real - 600),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    bad.msg_order_id = Some(1);
    let hs = wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: ALEX.into(),
            name: Some("Alex Example".into()),
            messages: vec![bad],
            ..Default::default()
        }],
        ..Default::default()
    };
    let o = apply_history(&store, &Names::default(), &hs).unwrap();
    assert_eq!(o.future_fixed, 1);
    let m = &store.messages(NET, ALEX, 10).unwrap()[0];
    assert_eq!(m.ts, (real - 600) * 1000);
    assert!(!map::is_future(m.ts, now_ms()));
    // The sidebar row follows the faithful message time, it does not keep the approximation.
    let c = store.chat(NET, ALEX).unwrap().unwrap();
    assert_eq!(c.last_ts, Some((real - 600) * 1000));
    assert_eq!(c.last_preview, "You: hello");
}

/// Only future-dated messages, nothing believable anywhere in the chat: the chat must sink below
/// a chat with real activity from an hour ago, not take the start-up time and float to the top.
#[test]
fn a_chat_of_only_future_messages_does_not_outrank_real_activity() {
    let (mut b, _erx, _rx) = backend();
    let now = now_ms();
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(2_057_119_831_000));
    seed_chat_as(&b.store, SAM, "Sam Sample", Some(now - 3_600_000));
    b.store
        .upsert_messages(&[
            stored(ALEX, "a1", 2_057_119_831_000, true, "You"),
            stored(ALEX, "a2", 2_057_119_890_000, true, "You"),
            stored(SAM, "s1", now - 3_600_000, false, "Sam Sample"),
        ])
        .unwrap();
    b.repair_future_dates();
    let chats = b.store.chats(NET).unwrap();
    assert_eq!(
        chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        [SAM, ALEX]
    );
    let m = b.store.messages(NET, ALEX, 10).unwrap();
    assert!(m[0].ts < m[1].ts, "order kept");
}

/// The start-up repair rewrites times for good, so a PC clock that starts 30 minutes slow must
/// not turn the last half hour of real messages into "future" rows.
#[test]
fn start_up_repair_leaves_a_message_30_minutes_ahead_alone() {
    let (mut b, _erx, _rx) = backend();
    let ahead = now_ms() + 30 * 60 * 1000;
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(ahead));
    b.store
        .upsert_messages(&[stored(ALEX, "a1", ahead, false, "Alex Example")])
        .unwrap();
    b.repair_future_dates();
    assert_eq!(b.store.messages(NET, ALEX, 10).unwrap()[0].ts, ahead);
}

// ---- bug 2: names ----------------------------------------------------------------------------

fn contact_update(jid: &str, name: &str, lid: Option<&str>, pn: Option<&str>) -> Event {
    let action = wa::sync_action_value::ContactAction {
        full_name: (!name.is_empty()).then(|| name.to_string()),
        lid_jid: lid.map(str::to_string),
        pn_jid: pn.map(str::to_string),
        ..Default::default()
    };
    Event::ContactUpdate(
        wev::ContactUpdate::builder()
            .jid(jid.parse().unwrap())
            .timestamp(whatsapp_rust::chrono::DateTime::from_timestamp(1, 0).unwrap())
            .action(Box::new(action))
            .from_full_sync(true)
            .build(),
    )
}

/// The "Unknown contact" chats are LID ids. An address-book contact that carries both
/// addresses names them, and the name is saved so a restart keeps it.
#[tokio::test]
async fn contact_record_names_a_lid_chat_and_survives_a_restart() {
    let (mut b, _erx, _rx) = backend();
    seed_chat_as(&b.store, LID, "Unknown contact", Some(1_000));
    b.on_event(&contact_update(ALEX, "Alex Example", Some(LID), Some(ALEX)))
        .await;
    assert!(b.retitle_at.is_some());
    b.retitle();
    assert_eq!(chat(&b, LID).title, "Alex Example");

    // "Restart": a new backend on the same store knows the name before any event arrives.
    let store = b.store.clone();
    let (etx, _erx2) = mpsc::unbounded_channel();
    let (tx, _rx2) = mpsc::unbounded_channel();
    let mut b2 = Backend::new(ctx_for(store, Settings::default()), etx, tx);
    b2.load_names();
    assert_eq!(
        b2.names.label_with(&ALEX.parse().unwrap(), None),
        "Alex Example"
    );
    // A brand new chat for that number no longer starts as a bare phone number.
    b2.on_message(&inbound(
        ALEX,
        ALEX,
        "m9",
        false,
        1_794_000_000,
        text("hi"),
        "",
    ))
    .await;
    assert_eq!(chat(&b2, ALEX).title, "Alex Example");
}

/// A push name learned under the phone number titles the LID chat of the same person, but only
/// once the two addresses are known to be one person.
#[tokio::test]
async fn push_name_reaches_the_linked_lid_chat() {
    let (mut b, _erx, _rx) = backend();
    seed_chat_as(&b.store, LID, "Unknown contact", Some(1_000));
    b.on_event(&Event::PushNameUpdate(
        wev::PushNameUpdate::builder()
            .jid(ALEX.parse().unwrap())
            .message(Box::new(MessageInfo::default()))
            .old_push_name(String::new())
            .new_push_name("Alex P".to_string())
            .build(),
    ))
    .await;
    b.retitle();
    assert_eq!(
        chat(&b, LID).title,
        "Unknown contact",
        "no link yet: honest, not guessed"
    );
    b.on_event(&contact_update(ALEX, "", Some(LID), Some(ALEX)))
        .await;
    b.retitle();
    assert_eq!(chat(&b, LID).title, "Alex P");
}

/// Existing titles improve on start from data already stored: the label the contact's own
/// messages carry, and the title of the same person's chat under the other address.
#[test]
fn start_up_retitle_uses_stored_sender_labels_and_sibling_chats() {
    let (mut b, mut erx, _rx) = backend();
    // Chat 1: LID chat, title unknown, but its messages carry the push name.
    seed_chat_as(&b.store, LID, "Unknown contact", Some(5));
    b.store
        .upsert_messages(&[
            stored(LID, "1", 1, false, "Alex P"),
            stored(LID, "2", 2, false, "Alex P"),
            stored(LID, "3", 3, false, "+15550100001"),
            stored(LID, "4", 4, true, "You"),
        ])
        .unwrap();
    // Chat 2: LID chat whose phone-number twin has a real title (link saved by an earlier run).
    let lid2 = "400500600@lid";
    seed_chat_as(&b.store, lid2, "Unknown contact", Some(4));
    seed_chat_as(&b.store, SAM, "Sam Sample", Some(3));
    b.store
        .upsert_names(
            NET,
            &[("link".into(), "15550100002".into(), "400500600".into())],
        )
        .unwrap();
    // Chat 3: nothing known anywhere: stays honestly unnamed. Chat 4: a real name stays.
    seed_chat_as(&b.store, "700800900@lid", "Unknown contact", Some(2));
    seed_chat_as(
        &b.store,
        "15550100003@s.whatsapp.net",
        "Pat Sample",
        Some(1),
    );
    b.store
        .upsert_messages(&[stored(
            "15550100003@s.whatsapp.net",
            "p1",
            1,
            false,
            "pat the push name",
        )])
        .unwrap();

    b.load_names();
    drain(&mut erx);
    b.retitle();

    assert_eq!(chat(&b, LID).title, "Alex P");
    assert_eq!(chat(&b, lid2).title, "Sam Sample");
    assert_eq!(chat(&b, "700800900@lid").title, "Unknown contact");
    assert_eq!(chat(&b, "15550100003@s.whatsapp.net").title, "Pat Sample");
    assert!(
        matches!(&drain(&mut erx)[0], Out::Chats(..)),
        "sidebar refreshed"
    );
    // Idempotent: a second run renames nothing and emits nothing.
    b.retitle();
    assert!(drain(&mut erx).is_empty());
}

/// History carries the address book: a chunk names LID chats and reports what to save.
#[test]
fn history_contacts_are_applied_and_reported() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    let hs = wa::HistorySync {
        inline_contacts: vec![wa::InlineContact {
            pn_jid: Some(ALEX.into()),
            lid_jid: Some(LID.into()),
            full_name: Some("Alex Example".into()),
            ..Default::default()
        }],
        conversations: vec![wa::Conversation {
            id: LID.into(),
            conversation_timestamp: Some(1_794_000_000),
            ..Default::default()
        }],
        ..Default::default()
    };
    let o = apply_history(&store, &Names::default(), &hs).unwrap();
    assert_eq!((o.book_names.len(), o.links.len()), (2, 1));
    let c = store
        .chat(NET, ALEX)
        .unwrap()
        .expect("stored under the phone number");
    assert_eq!(c.title, "Alex Example");
}

// ---- LID chat and its phone-number twin: one row ---------------------------------------------

const ALEX_USER: &str = "15550100001";
const LID_USER: &str = "100200300";

fn seed_lid_chat_with_history(b: &Backend) {
    seed_chat_as(&b.store, LID, "Unknown contact", Some(2_000));
    b.store
        .upsert_messages(&[
            stored(LID, "old1", 1_000, false, "Unknown contact"),
            stored(LID, "old2", 2_000, true, "You"),
        ])
        .unwrap();
}

/// Chats titled "Unknown contact" hold messages under LID ids. Once the phone number is known,
/// a reply must land in that same chat (moved to the phone-number id with its history), never in
/// a second row; edits of older messages still find their target.
#[tokio::test]
async fn a_reply_after_the_link_is_learned_joins_the_existing_lid_chat() {
    let (mut b, mut erx, _rx) = backend();
    seed_lid_chat_with_history(&b);
    b.names.link(ALEX_USER, LID_USER);

    let now = now_ms() / 1000;
    b.on_message(&inbound(
        LID,
        LID,
        "new1",
        false,
        now,
        text("hello again"),
        "Alex Example",
    ))
    .await;

    let chats = b.store.chats(NET).unwrap();
    assert_eq!(chats.len(), 1, "one person, one row: {chats:?}");
    assert_eq!(chats[0].id, ALEX);
    let ids: Vec<_> = b
        .store
        .messages(NET, ALEX, 10)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["old1", "old2", "new1"]);
    assert!(b.store.chat(NET, LID).unwrap().is_none());
    // The TUI got the merged list (the LID row is gone from the sidebar).
    let evs = drain(&mut erx);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Out::Chats(_, c) if c.len() == 1 && c[0].id == ALEX)));

    // An edit of an old message, addressed by LID, finds it in the merged chat.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("old1".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MessageEdit),
            edited_message: MessageField::some(text("fixed")),
            ..Default::default()
        }),
        ..Default::default()
    };
    b.on_message(&inbound(LID, LID, "e1", false, now, edit, "Alex Example"))
        .await;
    let m = b.store.messages(NET, ALEX, 10).unwrap();
    assert!(m
        .iter()
        .any(|m| m.id == "old1" && m.text == "fixed" && m.edited));
}

/// Start: links saved by an earlier run merge the LID chat with its twin at once.
#[tokio::test]
async fn start_up_merges_lid_chats_with_known_phone_numbers() {
    let (mut b, mut erx, _rx) = backend();
    seed_lid_chat_with_history(&b);
    seed_chat_as(&b.store, ALEX, "Alex Example", Some(5_000));
    b.store
        .upsert_messages(&[stored(ALEX, "p1", 5_000, false, "Alex Example")])
        .unwrap();
    b.names.link(ALEX_USER, LID_USER);
    b.learn_lid_links().await;
    let chats = b.store.chats(NET).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(
        (chats[0].id.as_str(), chats[0].title.as_str()),
        (ALEX, "Alex Example")
    );
    assert_eq!(b.store.messages(NET, ALEX, 10).unwrap().len(), 3);
    assert!(drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::Chats(_, c) if c.len() == 1)));
    // A LID chat with no known number is left alone.
    let (mut b2, _e, _r) = backend();
    seed_lid_chat_with_history(&b2);
    b2.learn_lid_links().await;
    assert!(b2.store.chat(NET, LID).unwrap().is_some());
}

/// A real name on the LID chat survives the merge when the phone-number twin only has a number.
#[test]
fn merge_keeps_the_real_name_over_a_bare_number() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    seed_chat_as(&store, LID, "Alex Example", Some(2_000));
    seed_chat_as(&store, ALEX, "+15550100001", Some(1_000));
    let mut names = Names::default();
    names.link(ALEX_USER, LID_USER);
    assert_eq!(merge_linked_chats(&store, &names).unwrap().len(), 1);
    assert_eq!(
        store.chat(NET, ALEX).unwrap().unwrap().title,
        "Alex Example"
    );
}

/// A history chunk for the phone-number id, arriving while the history sits under the LID id,
/// lands in one chat instead of storing every message twice.
#[test]
fn history_sync_merges_into_the_lid_chat_instead_of_duplicating() {
    crate::test_data_dir();
    let store = Store::open_in_memory().unwrap();
    seed_chat_as(&store, LID, "Unknown contact", Some(2_000));
    store
        .upsert_messages(&[stored(LID, "old1", 1_000, false, "Unknown contact")])
        .unwrap();
    let mut names = Names::default();
    names.link(ALEX_USER, LID_USER);
    let hs = wa::HistorySync {
        conversations: vec![wa::Conversation {
            id: ALEX.into(),
            messages: vec![wa::HistorySyncMsg {
                message: MessageField::some(wa::WebMessageInfo {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("old1".into()),
                        from_me: Some(false),
                        ..Default::default()
                    }),
                    message: MessageField::some(text("hi")),
                    message_timestamp: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    apply_history(&store, &names, &hs).unwrap();
    assert_eq!(store.chats(NET).unwrap().len(), 1);
    assert!(store.chat(NET, LID).unwrap().is_none());
    assert_eq!(
        store.messages(NET, ALEX, 10).unwrap().len(),
        1,
        "no duplicate"
    );
}

/// A read done on the phone names the LID id; it must clear the merged chat.
#[tokio::test]
async fn phone_read_marks_the_merged_chat_read() {
    let (mut b, _erx, _rx) = backend();
    seed_chat_as(&b.store, LID, "Unknown contact", Some(2_000));
    b.store.set_unread(NET, LID, 2).unwrap();
    b.names.link(ALEX_USER, LID_USER);
    b.on_event(&Event::MarkChatAsReadUpdate(
        wev::MarkChatAsReadUpdate::builder()
            .jid(LID.parse().unwrap())
            .timestamp(whatsapp_rust::chrono::DateTime::from_timestamp(1, 0).unwrap())
            .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                read: Some(true),
                ..Default::default()
            }))
            .from_full_sync(false)
            .build(),
    ))
    .await;
    assert!(b.store.chat(NET, LID).unwrap().is_none());
    assert_eq!(chat(&b, ALEX).unread, 0);
}
