//! The demo backends obey the same Event/Command contract as the real ones.

use std::time::Duration;

use chatcli_core::media::MediaCache;
use chatcli_core::model::{Command, CommandTx, Event, EventRx, Network, Status};
use chatcli_core::settings::Settings;
use chatcli_core::store::Store;
use chatcli_tui::demo;
use std::collections::HashMap;

/// The demo backends with an in-memory store and a throwaway media folder.
fn spawn_demo() -> (EventRx, HashMap<Network, CommandTx>) {
    let dir = std::env::temp_dir().join(format!("chatcli-tui-demo-test-{}", std::process::id()));
    let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
    demo::spawn(Store::open_in_memory().unwrap(), media)
}

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
    let (mut rx, cmds) = spawn_demo();
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
        reply_to: None,
        mentions: vec![],
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
        reply_to: None,
        mentions: vec![],
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
    let (mut rx, cmds) = spawn_demo();
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

/// The App's debounced preview works against the demo backend: moving through the list sends
/// one OpenChat for the chat it rests on, the backend answers with its messages, and the
/// read timer's MarkRead clears the unread count there.
#[tokio::test]
async fn app_preview_requests_one_chat_and_marks_it_read() {
    use chatcli_tui::{App, Clock};
    use crossterm::event::{KeyCode, KeyEvent};

    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    tx.send(Command::Login).unwrap();
    let chats = next(&mut rx, |e| match e {
        Event::Chats(Network::Teams, c) => Some(c),
        _ => None,
    })
    .await;
    let mut app = App::new(Clock {
        now_ms: 1_000_000,
        tz: None,
    });
    app.handle_event(Event::Chats(Network::Teams, chats));

    // Down twice quickly: the middle chat is passed over.
    app.handle_key(KeyEvent::from(KeyCode::Down));
    app.tick(1_000_050);
    assert!(app.fire_timers().is_empty());
    app.handle_key(KeyEvent::from(KeyCode::Down));
    app.tick(1_000_200);
    assert!(app.fire_timers().is_empty());
    app.tick(1_000_350);
    let out = app.fire_timers();
    assert_eq!(out.len(), 1, "{out:?}");
    let Command::OpenChat { chat_id } = out[0].1.clone() else {
        panic!("{out:?}");
    };
    tx.send(out[0].1.clone()).unwrap();
    let got = next(&mut rx, |e| match e {
        Event::Messages {
            chat_id: id,
            messages,
            ..
        } if id == chat_id => Some(messages.len()),
        _ => None,
    })
    .await;
    assert!(got > 0);

    // Two seconds of preview: the demo chat list shows no unread for it any more.
    app.tick(1_002_400);
    let out = app.fire_timers();
    for (_, cmd) in out {
        tx.send(cmd).unwrap();
    }
    tx.send(Command::Refresh).unwrap();
    let list = next(&mut rx, |e| match e {
        Event::Chats(Network::Teams, c) => Some(c),
        _ => None,
    })
    .await;
    assert_eq!(
        list.iter().find(|c| c.id == chat_id).map(|c| c.unread),
        Some(0)
    );
}

/// The first chat carries the messages that let each composer, emoji and link feature be tried.
#[tokio::test]
async fn first_chat_has_links_and_emoji_to_try() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    tx.send(Command::Login).unwrap();
    next(&mut rx, |e| match e {
        Event::Chats(Network::Teams, c) => Some(c),
        _ => None,
    })
    .await;
    tx.send(Command::OpenChat {
        chat_id: "demo-teams-0".into(),
    })
    .unwrap();
    let messages = next(&mut rx, |e| match e {
        Event::Messages { messages, .. } => Some(messages),
        _ => None,
    })
    .await;
    let text: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert!(text
        .iter()
        .any(|t| t.contains("https://docs.example.invalid/")));
    assert!(text
        .iter()
        .any(|t| t.contains("https://one.example.invalid/")
            && t.contains("https://two.example.invalid/")));
    assert!(text.iter().any(
        |t| t.contains("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}")
            && t.contains("\u{1F1FA}\u{1F1F3}")
    ));
    // Oldest first, ids unique.
    assert!(messages.windows(2).all(|w| w[0].ts <= w[1].ts));
    let mut ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), messages.len());
}

/// Signs Teams in and opens its first chat (a direct one with the lab messages); returns the
/// chat id and the history.
async fn open_first_chat(
    rx: &mut EventRx,
    tx: &CommandTx,
    group: bool,
) -> (String, Vec<chatcli_core::model::ChatMessage>) {
    tx.send(Command::Login).unwrap();
    let caps = next(rx, |e| match e {
        Event::Capabilities {
            network: Network::Teams,
            caps,
        } => Some(caps),
        _ => None,
    })
    .await;
    assert_eq!(caps, chatcli_core::model::Capabilities::all());
    let chats = next(rx, |e| match e {
        Event::Chats(Network::Teams, c) => Some(c),
        _ => None,
    })
    .await;
    let chat = chats
        .iter()
        .find(|c| (c.kind == chatcli_core::model::ChatKind::Group) == group)
        .unwrap()
        .clone();
    tx.send(Command::OpenChat {
        chat_id: chat.id.clone(),
    })
    .unwrap();
    let id = chat.id.clone();
    let history = next(rx, |e| match e {
        Event::Messages {
            chat_id, messages, ..
        } if chat_id == id => Some(messages),
        _ => None,
    })
    .await;
    (chat.id, history)
}

/// The history has replies, reactions, an edited message and (in the group) a mention of you, so
/// every new display can be tried with `chatcli --demo`.
#[tokio::test]
async fn demo_history_shows_replies_reactions_edits_and_mentions() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (_, direct) = open_first_chat(&mut rx, tx, false).await;
    assert!(direct.iter().any(|m| m.reply_to.is_some()));
    assert!(direct.iter().any(|m| !m.reactions.is_empty()));
    assert!(direct.iter().any(|m| m.edited));
    assert!(direct.iter().any(|m| m.reactions.iter().any(|r| r.mine)));
    let (_, group) = open_first_chat(&mut rx, tx, true).await;
    assert!(group
        .iter()
        .any(|m| m.mentions_me && !m.mentions.is_empty()));
}

#[tokio::test]
async fn demo_reacts_edits_and_deletes_own_messages() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, history) = open_first_chat(&mut rx, tx, false).await;
    let mine = history.iter().find(|m| m.from_me).unwrap().clone();
    let theirs = history.iter().find(|m| !m.from_me).unwrap().clone();

    // One reaction per person: a new emoji replaces the old one, remove=true clears it.
    for (emoji, remove) in [("\u{2764}\u{FE0F}", false), ("\u{2764}\u{FE0F}", true)] {
        tx.send(Command::React {
            chat_id: chat.clone(),
            message_id: theirs.id.clone(),
            emoji: emoji.into(),
            remove,
        })
        .unwrap();
        let changed = next(&mut rx, |e| match e {
            Event::MessageChanged(m) if m.id == theirs.id => Some(m),
            _ => None,
        })
        .await;
        assert_eq!(
            changed.reactions.iter().any(|r| r.emoji == emoji && r.mine),
            !remove
        );
    }
    tx.send(Command::React {
        chat_id: chat.clone(),
        message_id: mine.id.clone(),
        emoji: "\u{1F389}".into(),
        remove: false,
    })
    .unwrap();
    let changed = next(&mut rx, |e| match e {
        Event::MessageChanged(m) if m.id == mine.id => Some(m),
        _ => None,
    })
    .await;
    // Their earlier thumbs-up of yours is gone (one reaction per user), the new one is yours.
    assert_eq!(changed.reactions.iter().filter(|r| r.mine).count(), 1);
    assert!(changed
        .reactions
        .iter()
        .any(|r| r.emoji == "\u{1F389}" && r.mine));

    tx.send(Command::Edit {
        chat_id: chat.clone(),
        message_id: mine.id.clone(),
        text: "edited text".into(),
    })
    .unwrap();
    let changed = next(&mut rx, |e| match e {
        Event::MessageChanged(m) if m.id == mine.id => Some(m),
        _ => None,
    })
    .await;
    assert_eq!(
        (changed.text.as_str(), changed.edited),
        ("edited text", true)
    );

    // Someone else's message: a notice, never a silent no-op.
    tx.send(Command::Edit {
        chat_id: chat.clone(),
        message_id: theirs.id.clone(),
        text: "x".into(),
    })
    .unwrap();
    next(&mut rx, |e| match e {
        Event::Notice { is_error: true, .. } => Some(()),
        _ => None,
    })
    .await;

    tx.send(Command::Delete {
        chat_id: chat.clone(),
        message_id: mine.id.clone(),
    })
    .unwrap();
    let changed = next(&mut rx, |e| match e {
        Event::MessageChanged(m) if m.id == mine.id => Some(m),
        _ => None,
    })
    .await;
    assert_eq!(changed.state, chatcli_core::model::MessageState::Deleted);
    assert!(changed.text.is_empty() && changed.reactions.is_empty());

    // The change sticks: reopening the chat brings the deleted message back as deleted.
    tx.send(Command::OpenChat {
        chat_id: chat.clone(),
    })
    .unwrap();
    let again = next(&mut rx, |e| match e {
        Event::Messages {
            chat_id, messages, ..
        } if chat_id == chat => Some(messages),
        _ => None,
    })
    .await;
    let kept = again.iter().find(|m| m.id == mine.id).unwrap();
    assert_eq!(kept.state, chatcli_core::model::MessageState::Deleted);
}

#[tokio::test]
async fn demo_sends_a_reply_and_answers_the_member_list() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, history) = open_first_chat(&mut rx, tx, true).await;
    let quoted = history.last().unwrap();
    tx.send(Command::Send {
        chat_id: chat.clone(),
        text: "@Alex Example ok".into(),
        client_id: "local-9".into(),
        reply_to: Some(chatcli_core::model::QuotedRef {
            id: quoted.id.clone(),
            sender: quoted.sender.clone(),
            sender_id: String::new(),
            preview: "x".into(),
        }),
        mentions: vec![],
    })
    .unwrap();
    let confirmed = next(&mut rx, |e| match e {
        Event::MessageConfirmed { client_id, message } if client_id == "local-9" => Some(message),
        _ => None,
    })
    .await;
    assert_eq!(
        confirmed.reply_to.map(|q| q.id),
        Some(quoted.id.clone()),
        "the reply keeps its quote"
    );
    tx.send(Command::LoadMembers {
        chat_id: chat.clone(),
    })
    .unwrap();
    let members = next(&mut rx, |e| match e {
        Event::Members {
            members,
            error: None,
            ..
        } => Some(members),
        _ => None,
    })
    .await;
    assert!(members.len() >= 4 && members.iter().all(|p| !p.id.is_empty()));
}

/// Asks the demo for attachment `which` of the first message whose attachment has `fetch_ref`.
async fn fetch_demo_media(
    rx: &mut EventRx,
    tx: &CommandTx,
    chat: &str,
    history: &[chatcli_core::model::ChatMessage],
    fetch_ref: &str,
) -> Event {
    let msg = history
        .iter()
        .find(|m| {
            m.attachments
                .iter()
                .any(|a| a.fetch_ref.as_str() == fetch_ref)
        })
        .unwrap_or_else(|| panic!("no demo message with {fetch_ref}"));
    tx.send(Command::FetchMedia {
        chat_id: chat.to_string(),
        message_id: msg.id.clone(),
        index: 0,
    })
    .unwrap();
    let id = msg.id.clone();
    next(rx, |e| match &e {
        Event::MediaReady { message_id, .. } | Event::MediaFailed { message_id, .. }
            if *message_id == id =>
        {
            Some(e)
        }
        _ => None,
    })
    .await
}

/// The history carries one attachment of each kind the media features handle, and `FetchMedia`
/// answers like a real backend: a cached file, or the documented failure.
#[tokio::test]
async fn demo_fetches_media_into_the_cache_or_fails_with_a_reason() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, history) = open_first_chat(&mut rx, tx, false).await;
    let kinds: Vec<_> = history
        .iter()
        .flat_map(|m| {
            m.attachments
                .iter()
                .map(|a| a.fetch_ref.as_str().to_string())
        })
        .collect();
    for want in [
        "demo:image",
        "demo:text",
        "demo:exe",
        "demo:expired",
        "demo:big",
        "demo:browser",
    ] {
        assert!(kinds.iter().any(|k| k == want), "missing {want}: {kinds:?}");
    }
    // Every marker stays in the text, for older code.
    assert!(history
        .iter()
        .any(|m| m.text.starts_with("[image]") && !m.attachments.is_empty()));

    let Event::MediaReady { path, .. } =
        fetch_demo_media(&mut rx, tx, &chat, &history, "demo:image").await
    else {
        panic!("the picture should download");
    };
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "a real PNG in the cache"
    );
    assert!(path.extension().is_some_and(|e| e == "png"));
    // Asking again gives the same cached file.
    let Event::MediaReady { path: again, .. } =
        fetch_demo_media(&mut rx, tx, &chat, &history, "demo:image").await
    else {
        panic!("cached");
    };
    assert_eq!(path, again);
    let Event::MediaReady { path: txt, .. } =
        fetch_demo_media(&mut rx, tx, &chat, &history, "demo:text").await
    else {
        panic!("the note should download");
    };
    assert!(txt.extension().is_some_and(|e| e == "txt"));

    let Event::MediaFailed {
        reason,
        fallback_url,
        ..
    } = fetch_demo_media(&mut rx, tx, &chat, &history, "demo:expired").await
    else {
        panic!("expired");
    };
    assert_eq!((reason.as_str(), fallback_url), ("MEDIA EXPIRED", None));
    let Event::MediaFailed { reason, .. } =
        fetch_demo_media(&mut rx, tx, &chat, &history, "demo:big").await
    else {
        panic!("over the cap");
    };
    assert!(reason.starts_with("FILE OVER 25 MB"), "{reason}");
    let Event::MediaFailed {
        reason,
        fallback_url,
        ..
    } = fetch_demo_media(&mut rx, tx, &chat, &history, "demo:browser").await
    else {
        panic!("browser file");
    };
    assert_eq!(reason, "FILE: open in browser");
    assert!(fallback_url.is_some_and(|u| u.starts_with("https://files.example.invalid/")));
}

/// A file send: progress, then the confirmed message with its attachment; a missing file and a
/// refusing chat fail like a real send.
#[tokio::test]
async fn demo_sends_a_file_with_progress_and_fails_like_a_real_send() {
    use chatcli_core::model::MediaKind;
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, _) = open_first_chat(&mut rx, tx, false).await;
    let dir = std::env::temp_dir().join(format!("chatcli-tui-demo-send-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("photo.png");
    std::fs::write(&file, vec![1u8; 3000]).unwrap();

    tx.send(Command::SendMedia {
        chat_id: chat.clone(),
        client_id: "local-m1".into(),
        path: file.clone(),
        kind: MediaKind::Image,
        caption: "look".into(),
    })
    .unwrap();
    let mut seen = Vec::new();
    let confirmed = next(&mut rx, |e| match e {
        Event::SendProgress {
            client_id,
            done,
            total,
            ..
        } if client_id == "local-m1" => {
            seen.push((done, total));
            None
        }
        Event::MessageConfirmed { client_id, message } if client_id == "local-m1" => Some(message),
        _ => None,
    })
    .await;
    assert_eq!(seen, [(1000, 3000), (2000, 3000), (3000, 3000)]);
    assert_eq!(confirmed.text, "[image] look");
    assert_eq!(confirmed.attachments.len(), 1);
    assert_eq!(confirmed.attachments[0].name.as_deref(), Some("photo.png"));
    assert_eq!(confirmed.attachments[0].size, Some(3000));
    assert!(confirmed.from_me);

    tx.send(Command::SendMedia {
        chat_id: chat.clone(),
        client_id: "local-m2".into(),
        path: dir.join("gone.png"),
        kind: MediaKind::Image,
        caption: String::new(),
    })
    .unwrap();
    let err = next(&mut rx, |e| match e {
        Event::MessageFailed {
            client_id, error, ..
        } if client_id == "local-m2" => Some(error),
        _ => None,
    })
    .await;
    assert_eq!(err, "FILE NOT FOUND");

    tx.send(Command::SendMedia {
        chat_id: "demo-refuse".into(),
        client_id: "local-m3".into(),
        path: file,
        kind: MediaKind::Document,
        caption: String::new(),
    })
    .unwrap();
    let err = next(&mut rx, |e| match e {
        Event::MessageFailed {
            client_id, error, ..
        } if client_id == "local-m3" => Some(error),
        _ => None,
    })
    .await;
    assert_eq!(err, "demo: this chat refuses messages");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Search and older history need the demo's store to hold the chats from the start, like a real
/// backend that has synced before.
#[tokio::test]
async fn demo_seeds_the_store_so_search_works_before_any_chat_is_opened() {
    let dir = std::env::temp_dir().join(format!("chatcli-tui-demo-seed-{}", std::process::id()));
    let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
    let store = Store::open_in_memory().unwrap();
    let _backends = demo::spawn(store.clone(), media);
    let hits = store.search("latest draft", None, 20).unwrap();
    assert!(
        hits.iter().any(|h| h.network == Network::Teams)
            && hits.iter().any(|h| h.network == Network::WhatsApp),
        "both networks are searchable: {hits:?}"
    );
    assert!(hits.iter().all(|h| !h.chat_title.is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Three pages of older messages, each older than the last and never repeating, then the start of
/// the conversation: what scrolling up in `chatcli --demo` shows.
#[tokio::test]
async fn demo_serves_older_pages_then_says_it_is_the_start() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, history) = open_first_chat(&mut rx, tx, false).await;
    let oldest = history.iter().min_by_key(|m| (m.ts, m.id.clone())).unwrap();
    let (mut before_ts, mut before_id) = (oldest.ts, oldest.id.clone());
    let mut seen: std::collections::HashSet<String> =
        history.iter().map(|m| m.id.clone()).collect();
    for page in 1..=3 {
        tx.send(Command::LoadOlder {
            chat_id: chat.clone(),
            before_ts,
            before_id: before_id.clone(),
        })
        .unwrap();
        let (messages, at_start) = next(&mut rx, |e| match e {
            Event::OlderMessages {
                chat_id,
                messages,
                at_start,
                ..
            } if chat_id == chat => Some((messages, at_start)),
            _ => None,
        })
        .await;
        assert_eq!(messages.len(), 30, "page {page}");
        assert!(
            messages.iter().all(|m| m.ts < before_ts),
            "older than asked"
        );
        assert!(
            messages.iter().all(|m| seen.insert(m.id.clone())),
            "no repeats in page {page}"
        );
        assert_eq!(at_start, page == 3, "only the last page reaches the start");
        let first = &messages[0];
        (before_ts, before_id) = (first.ts, first.id.clone());
    }
}

/// Pin, mute and hide: the demo stores the flag and answers with the chat as it now is.
#[tokio::test]
async fn demo_sets_and_clears_chat_flags() {
    use chatcli_core::model::{ChatFlag, MUTE_FOREVER};
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::Teams];
    let (chat, _) = open_first_chat(&mut rx, tx, false).await;
    for (flag, on) in [
        (ChatFlag::Pinned, true),
        (ChatFlag::Muted, true),
        (ChatFlag::Hidden, true),
        (ChatFlag::Pinned, false),
    ] {
        tx.send(Command::SetChatFlag {
            chat_id: chat.clone(),
            flag,
            on,
        })
        .unwrap();
        let updated = next(&mut rx, |e| match e {
            Event::ChatUpdated(c) if c.id == chat => Some(c),
            _ => None,
        })
        .await;
        match flag {
            ChatFlag::Pinned => assert_eq!(updated.flags.pinned, on),
            ChatFlag::Muted => assert_eq!(updated.flags.muted_until, Some(MUTE_FOREVER)),
            ChatFlag::Hidden => assert_eq!(updated.flags.hidden, on),
        }
    }
    // Earlier flags stay: the chat is still muted and hidden after the pin came off.
    tx.send(Command::SetChatFlag {
        chat_id: chat.clone(),
        flag: ChatFlag::Pinned,
        on: true,
    })
    .unwrap();
    let updated = next(&mut rx, |e| match e {
        Event::ChatUpdated(c) if c.id == chat => Some(c),
        _ => None,
    })
    .await;
    assert!(updated.flags.pinned && updated.flags.hidden && updated.flags.muted_until.is_some());
}

/// WhatsApp only: someone starts typing, then their message arrives. Teams never gets a typing
/// event.
#[tokio::test]
async fn demo_whatsapp_shows_someone_typing_and_then_their_message() {
    let (mut rx, cmds) = spawn_demo();
    let tx = &cmds[&Network::WhatsApp];
    tx.send(Command::Login).unwrap();
    let chats = next(&mut rx, |e| match e {
        Event::Chats(Network::WhatsApp, c) => Some(c),
        _ => None,
    })
    .await;
    let first = chats[0].id.clone();
    let (chat, sender, active) = next(&mut rx, |e| match e {
        Event::Typing {
            network: Network::WhatsApp,
            chat_id,
            sender,
            active,
        } => Some((chat_id, sender, active)),
        Event::Typing { network, .. } => panic!("typing on {network:?}"),
        _ => None,
    })
    .await;
    assert_eq!(
        (chat.as_str(), sender.as_str(), active),
        (first.as_str(), "Riley Sample", true)
    );
    let message = next(&mut rx, |e| match e {
        Event::Message(m) if m.chat_id == first && m.sender == "Riley Sample" => Some(m),
        _ => None,
    })
    .await;
    assert!(!message.from_me);
    // The TUI's own composing is acknowledged with a notice so it can be seen.
    tx.send(Command::Typing {
        chat_id: first,
        active: true,
    })
    .unwrap();
    let text = next(&mut rx, |e| match e {
        Event::Notice { text, .. } => Some(text),
        _ => None,
    })
    .await;
    assert!(text.contains("composing"), "{text}");
}
