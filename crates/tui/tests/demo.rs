//! The demo backends obey the same Event/Command contract as the real ones.

use std::time::Duration;

use chatcli_core::model::{Command, Event, EventRx, Network, Status};
use chatcli_tui::demo;

/// Waits (up to 10 s) for the first event that `pick` accepts; other events are skipped.
async fn next<T>(rx: &mut EventRx, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ev = rx.recv().await.expect("demo backend ended");
            if let Some(v) = pick(ev) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for a demo event")
}

#[tokio::test]
async fn teams_signs_in_loads_chats_and_sends() {
    let (mut rx, cmds) = demo::spawn();
    let tx = &cmds[&Network::Teams];
    next(&mut rx, |e| {
        matches!(e, Event::Status(Network::Teams, Status::NeedsLogin { .. })).then_some(())
    })
    .await;

    tx.send(Command::Login).unwrap();
    next(&mut rx, |e| {
        matches!(e, Event::Status(Network::Teams, Status::LoggingIn)).then_some(())
    })
    .await;
    next(&mut rx, |e| {
        matches!(
            e,
            Event::Status(Network::Teams, Status::Online { live: true, .. })
        )
        .then_some(())
    })
    .await;
    let chats = next(&mut rx, |e| match e {
        Event::Chats(Network::Teams, c) => Some(c),
        _ => None,
    })
    .await;
    assert_eq!(chats.len(), 8);

    let normal = chats
        .iter()
        .find(|c| !c.title.starts_with("Demo:"))
        .unwrap()
        .id
        .clone();
    tx.send(Command::OpenChat {
        chat_id: normal.clone(),
    })
    .unwrap();
    let history = next(&mut rx, |e| match e {
        Event::Messages {
            network: Network::Teams,
            chat_id,
            messages,
        } if chat_id == normal => Some(messages),
        _ => None,
    })
    .await;
    assert!(!history.is_empty());

    tx.send(Command::Send {
        chat_id: normal.clone(),
        text: "hello".into(),
        client_id: "local-1".into(),
    })
    .unwrap();
    let confirmed = next(&mut rx, |e| match e {
        Event::MessageConfirmed { client_id, message } if client_id == "local-1" => Some(message),
        _ => None,
    })
    .await;
    assert_eq!(
        (confirmed.text.as_str(), confirmed.from_me),
        ("hello", true)
    );

    let refusing = chats
        .iter()
        .find(|c| c.title.starts_with("Demo:"))
        .unwrap()
        .id
        .clone();
    tx.send(Command::Send {
        chat_id: refusing,
        text: "nope".into(),
        client_id: "local-2".into(),
    })
    .unwrap();
    let error = next(&mut rx, |e| match e {
        Event::MessageFailed {
            client_id, error, ..
        } if client_id == "local-2" => Some(error),
        _ => None,
    })
    .await;
    assert_eq!(error, "demo: this chat refuses messages");
}

#[tokio::test]
async fn whatsapp_shows_qr_then_goes_online_with_twelve_chats() {
    let (mut rx, cmds) = demo::spawn();
    let tx = &cmds[&Network::WhatsApp];
    next(&mut rx, |e| {
        matches!(
            e,
            Event::Status(Network::WhatsApp, Status::NeedsLogin { .. })
        )
        .then_some(())
    })
    .await;
    tx.send(Command::Login).unwrap();
    let code = next(&mut rx, |e| match e {
        Event::Status(Network::WhatsApp, Status::Qr { code }) => Some(code),
        _ => None,
    })
    .await;
    assert!(!code.is_empty());
    let chats = next(&mut rx, |e| match e {
        Event::Chats(Network::WhatsApp, c) => Some(c),
        _ => None,
    })
    .await;
    assert_eq!(chats.len(), 12);
}
