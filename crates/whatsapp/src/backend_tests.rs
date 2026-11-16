//! Backend behaviour against an in-memory store, with library events built by hand (no network).

use super::*;
use chatcli_core::model::{ChatKind, EventRx};
use whatsapp_rust::wacore::types::events as wev;
use whatsapp_rust::wacore::types::message::MessageInfo;

const ALEX: &str = "15550100001@s.whatsapp.net";
const SAM: &str = "15550100002@s.whatsapp.net";
const TEAM: &str = "120363000000000000@g.us";

fn backend() -> (Backend, EventRx, mpsc::UnboundedReceiver<Msg>) {
    crate::test_data_dir();
    let (etx, erx) = mpsc::unbounded_channel();
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Backend::new(Store::open_in_memory().unwrap(), etx, tx),
        erx,
        rx,
    )
}

fn drain(rx: &mut EventRx) -> Vec<Out> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn inbound(
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

fn text(t: &str) -> wa::Message {
    wa::Message {
        conversation: Some(t.to_string()),
        ..Default::default()
    }
}

fn chat(b: &Backend, id: &str) -> ChatSummary {
    b.store.chat(NET, id).unwrap().expect("chat stored")
}

fn seed_chat(store: &Store, last_ts: i64, preview: &str, unread: u32) {
    store
        .upsert_chats(&[ChatSummary {
            network: NET,
            id: ALEX.into(),
            title: "Alex Example".into(),
            kind: ChatKind::Direct,
            last_ts: Some(last_ts),
            last_preview: preview.into(),
            unread,
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
    assert_eq!(m.text, "the build (edited)");
    assert_eq!(chat(&b, ALEX).last_preview, "the build (edited)");
    assert!(drain(&mut erx)
        .iter()
        .any(|e| matches!(e, Out::Message(x) if x.id == "m1")));

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
        (MessageState::Deleted, "[message deleted]")
    );
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
        text: "yes".into(),
        result: Ok("REALID".into()),
    };
    b.on_msg(Msg::Sent(ok)).await;
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
        text: "no".into(),
        result: Err("not logged in".into()),
    };
    b.on_msg(Msg::Sent(bad)).await;
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
    assert_eq!(drain(&mut erx).len(), 1);
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
    assert_eq!(store.messages(NET, ALEX, 10).unwrap().len(), 2);
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
    run(store, crx, etx).await;
    let evs = drain(&mut erx);
    assert!(matches!(&evs[0], Out::Status(_, Status::Starting)));
    assert!(matches!(&evs[1], Out::Chats(_, c) if c.len() == 1 && c[0].last_preview == "cached"));
    assert!(matches!(
        &evs[2],
        Out::Status(_, Status::NeedsLogin { reason: None })
    ));
}
