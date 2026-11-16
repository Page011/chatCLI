//! Fake backends for `chatcli --demo`: try the whole screen with no accounts and no network.
//! Generic names only. Each network starts signed out; sign-in takes a moment, then chats
//! appear. One chat per network refuses every send so the NOT SENT state can be seen.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::time::Duration;

use chatcli_core::media::MediaCache;
use chatcli_core::model::{
    now_ms, unsupported, Attachment, Capabilities, ChatFlag, ChatKind, ChatMessage, ChatSummary,
    Command, CommandRx, CommandTx, Event, EventRx, EventTx, FetchRef, MediaKind, MessageState,
    Network, PersonRef, QuotedRef, Reaction, Status, MUTE_FOREVER,
};
use chatcli_core::store::Store;
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, Instant};

const REFUSING_CHAT: &str = "demo-refuse";
/// A short made-up payload: small enough to draw in a normal window.
const SAMPLE_QR: &str = "2@demo-sample-payload-not-a-real-link-code,AAAA,BBBB,CCCC";

/// (title, group, unread, minutes since last message)
const TEAMS_CHATS: [(&str, bool, u32, i64); 8] = [
    ("Alex Example", false, 2, 5),
    ("Project Team", true, 1, 80),
    ("Sam Sample", false, 1, 200),
    ("Design Review", true, 0, 1500),
    ("Jordan Placeholder", false, 0, 3000),
    ("Demo: Rejects Sends", false, 0, 4000),
    ("Release Planning", true, 0, 9000),
    ("Taylor Testcase", false, 0, 20000),
];

const WHATSAPP_CHATS: [(&str, bool, u32, i64); 12] = [
    ("Riley Sample", false, 3, 2),
    ("Family Group", true, 12, 30),
    ("Morgan Mockup", false, 0, 120),
    ("Weekend Plans", true, 0, 600),
    ("Casey Example", false, 0, 1700),
    ("Demo: Rejects Sends", false, 0, 2500),
    ("Book Club", true, 0, 4000),
    ("Jamie Placeholder", false, 0, 6000),
    ("Neighbours", true, 0, 8000),
    ("Quinn Testcase", false, 0, 12000),
    ("Avery Sample", false, 0, 30000),
    ("Dana Example", false, 0, 60000),
];

const LINES: [&str; 6] = [
    "Morning, are we still on for today?",
    "Yes, see you at ten.",
    "Can you check the latest draft when you get a moment?",
    "Looks good to me, just one small change on page two.",
    "Thanks, I will update it now.",
    "Great, speak later.",
];

/// Extra messages in the first chat of each network, so the composer, emoji and link features
/// can be tried with `chatcli --demo`. Placeholder links only (example.invalid never resolves).
const LAB: [&str; 4] = [
    "Try it: Ctrl+Up selects a message (r reply, e react, E edit or d delete your own, u link, o open or v view an attachment, Esc back). Ctrl+O attaches a file, Alt+V pastes a clipboard picture, Ctrl+E opens the emoji picker, :thu then Enter completes an emoji, Ctrl+J adds a line, a pasted block stays in the box. In Project Team and Family Group type @ and a name to mention someone.",
    "Docs: https://docs.example.invalid/handbook/section/page?id=12345&view=full#top (updated today).",
    "Two links: https://one.example.invalid/a and https://two.example.invalid/b",
    "Emoji \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family, \u{1F1FA}\u{1F1F3} flag, \u{1F44D}\u{1F3FD} thumbs up, \u{2764}\u{FE0F} heart, 1\u{FE0F}\u{20E3} keycap, \u{4f60}\u{597d}",
];

/// Older history the demo "network" can give a chat: this many pages of `OLDER_PAGE` messages,
/// then the start of the conversation.
const OLDER_PAGES: u32 = 3;
const OLDER_PAGE: usize = 30;

const PEOPLE: [&str; 3] = ["Alex Example", "Sam Sample", "Jordan Placeholder"];

/// Who is in every demo group, for `@` completion. The first one is the signed-in user.
const MEMBERS: [&str; 6] = [
    "Demo User",
    "Alex Example",
    "Sam Sample",
    "Jordan Placeholder",
    "Taylor Testcase",
    "Riley Sample",
];

fn members(net: Network) -> Vec<PersonRef> {
    MEMBERS
        .iter()
        .enumerate()
        .map(|(i, name)| PersonRef {
            id: format!("demo-{}-user-{i}", net.key()),
            name: (*name).to_string(),
        })
        .collect()
}

/// Starts one fake backend per network. Call inside a tokio runtime. The store holds every chat
/// and its first messages from the start, so search (Ctrl+F) works before a chat is opened, and
/// it takes the older pages a `LoadOlder` produces; it also caches the group members for `@`
/// completion. The media cache receives the generated picture and note a `FetchMedia` asks for,
/// like a real backend's download.
pub fn spawn(store: Store, media: MediaCache) -> (EventRx, HashMap<Network, CommandTx>) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut commands = HashMap::new();
    for net in Network::ALL {
        seed_store(&store, net);
        let (tx, rx) = mpsc::unbounded_channel();
        commands.insert(net, tx);
        tokio::spawn(backend(
            net,
            rx,
            events_tx.clone(),
            store.clone(),
            media.clone(),
        ));
    }
    (events_rx, commands)
}

/// The time the demo's history is built from, fixed for the run: the store's copy and the
/// backend's copy of a message must agree, or "older than the oldest" would find its own twin.
fn base_ms() -> i64 {
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BASE.get_or_init(now_ms)
}

fn seed_chats(net: Network) -> Vec<ChatSummary> {
    let table: &[(&str, bool, u32, i64)] = match net {
        Network::Teams => &TEAMS_CHATS,
        Network::WhatsApp => &WHATSAPP_CHATS,
    };
    let now = base_ms();
    table
        .iter()
        .enumerate()
        .map(|(i, &(title, group, unread, mins))| ChatSummary {
            network: net,
            id: if title.starts_with("Demo: Rejects") {
                REFUSING_CHAT.to_string()
            } else {
                format!("demo-{}-{i}", net.key())
            },
            title: title.to_string(),
            kind: if group {
                ChatKind::Group
            } else {
                ChatKind::Direct
            },
            last_ts: Some(now - mins * 60_000),
            last_preview: format!("{}: {}", PEOPLE[i % PEOPLE.len()], LINES[i % LINES.len()]),
            unread,
            // The group's unread message mentions you (see `history`).
            mentions: u32::from(i == 1 && unread > 0),
            ..ChatSummary::blank(net)
        })
        .collect()
}

/// Writes the demo's chats and the messages each opens with into the store, like a real backend
/// that has synced before: the search box reads them from there.
fn seed_store(store: &Store, net: Network) {
    let chats = seed_chats(net);
    let mut messages: Vec<ChatMessage> = Vec::new();
    for chat in &chats {
        messages.extend(history(net, chat));
    }
    let result = store
        .upsert_chats(&chats)
        .and_then(|()| store.upsert_messages(&messages));
    if let Err(e) = result {
        chatcli_core::log::error("demo", &format!("demo history not stored: {e}"));
    }
}

/// The fake network id of a demo sender, so the TUI can ask for their photo.
fn demo_id(sender: &str) -> String {
    format!("demo:{}", if sender == "You" { "you" } else { sender })
}

/// Gives every message a sender id (the TUI asks for photos by id).
fn with_ids(mut list: Vec<ChatMessage>) -> Vec<ChatMessage> {
    for m in &mut list {
        m.sender_id = demo_id(&m.sender);
    }
    list
}

/// One page of older messages before `before_ts`, oldest first. `page` counts from 1.
fn older_page(net: Network, chat: &str, before_ts: i64, page: u32) -> Vec<ChatMessage> {
    with_ids(
        (0..OLDER_PAGE)
            .map(|i| {
                // Newest of the page last; two hours apart so the day separators show.
                let age = (OLDER_PAGE - i) as i64;
                let number = (page as usize - 1) * OLDER_PAGE + (OLDER_PAGE - i);
                ChatMessage {
                    network: net,
                    chat_id: chat.to_string(),
                    id: format!("{chat}-old{page}-{i:02}"),
                    ts: before_ts - age * 2 * 3_600_000,
                    sender: PEOPLE[i % PEOPLE.len()].to_string(),
                    text: format!("{} (earlier message {number})", LINES[i % LINES.len()]),
                    state: MessageState::Sent,
                    ..ChatMessage::blank(net)
                }
            })
            .collect(),
    )
}

fn history(net: Network, chat: &ChatSummary) -> Vec<ChatMessage> {
    let end = chat.last_ts.unwrap_or_else(now_ms);
    let mut out: Vec<ChatMessage> = (0..LINES.len())
        .map(|i| {
            let from_me = i % 2 == 1;
            ChatMessage {
                network: net,
                chat_id: chat.id.clone(),
                id: format!("{}-h{i}", chat.id),
                // Spread over a few days so the day separators show.
                ts: end - (LINES.len() - 1 - i) as i64 * 17 * 3_600_000,
                sender: if from_me {
                    "You".to_string()
                } else {
                    PEOPLE[i % PEOPLE.len()].to_string()
                },
                from_me,
                text: LINES[i].to_string(),
                state: MessageState::Sent,
                ..ChatMessage::blank(net)
            }
        })
        .collect();
    // Reactions on an incoming and on your own message, to try react and un-react.
    let reaction = |emoji: &str, count: u32, mine: bool| Reaction {
        emoji: emoji.to_string(),
        count,
        mine,
    };
    if let Some(m) = out.get_mut(1) {
        m.reactions = vec![reaction("\u{1F44D}", 1, true)];
    }
    if let Some(m) = out.get_mut(2) {
        m.reactions = vec![
            reaction("\u{1F44D}", 3, false),
            reaction("\u{2764}\u{FE0F}", 1, false),
        ];
    }
    if chat.id.ends_with("-1") {
        out.push(ChatMessage {
            network: net,
            chat_id: chat.id.clone(),
            id: format!("{}-mention", chat.id),
            ts: end + 500,
            sender: "Sam Sample".to_string(),
            text: "@Demo User can you check the draft before the meeting?".to_string(),
            state: MessageState::Sent,
            mentions: vec![members(net)[0].clone()],
            mentions_me: true,
            ..ChatMessage::blank(net)
        });
    }
    if chat.id.ends_with("-0") {
        out.extend(LAB.iter().enumerate().map(|(i, text)| ChatMessage {
            network: net,
            chat_id: chat.id.clone(),
            id: format!("{}-lab{i}", chat.id),
            ts: end + (i as i64 + 1) * 1_000,
            sender: "Alex Example".to_string(),
            text: (*text).to_string(),
            state: MessageState::Sent,
            // The docs line was edited after it was sent.
            edited: i == 1,
            ..ChatMessage::blank(net)
        }));
        // A reply to the first lab message, so a quote can be seen on arrival.
        out.push(ChatMessage {
            network: net,
            chat_id: chat.id.clone(),
            id: format!("{}-reply", chat.id),
            ts: end + 10_000,
            sender: "Sam Sample".to_string(),
            text: "Thanks, I will try the keys now.".to_string(),
            state: MessageState::Sent,
            reply_to: Some(QuotedRef {
                id: format!("{}-lab0", chat.id),
                sender: "Alex Example".to_string(),
                sender_id: String::new(),
                preview: "Try it: Ctrl+Up selects a message".to_string(),
            }),
            ..ChatMessage::blank(net)
        });
        // After the reply: the list stays in time order.
        out.extend(media_messages(net, chat, end));
    }
    with_ids(out)
}

/// The demo's attachments: (kind, name, mime, size, fetch_ref, text). What each `fetch_ref`
/// does is in `fetch_answer`.
type Sample = (
    MediaKind,
    &'static str,
    &'static str,
    u64,
    &'static str,
    &'static str,
);

const MEDIA_SAMPLES: [Sample; 5] = [
    (
        MediaKind::Image,
        "sunset.png",
        "image/png",
        52_000,
        "demo:image",
        "[image] sunset from the lab window",
    ),
    (
        MediaKind::Document,
        "notes.txt",
        "text/plain",
        1_200,
        "demo:text",
        "[document: notes.txt] meeting notes",
    ),
    (
        MediaKind::Document,
        "setup.exe",
        "application/octet-stream",
        8_400_000,
        "demo:exe",
        "[document: setup.exe] the installer (o refuses it)",
    ),
    (
        MediaKind::Image,
        "old-photo.jpg",
        "image/jpeg",
        310_000,
        "demo:expired",
        "[image] an old photo (its link has expired)",
    ),
    (
        MediaKind::Document,
        "archive.zip",
        "application/zip",
        31_457_280,
        "demo:big",
        "[document: archive.zip] over the 25 MB download limit",
    ),
];

/// Attachments to try with `o` and `v` in the first chat of each network: a picture, a text file,
/// an executable (refused), an expired link, a file over the size cap and, on Teams, a OneDrive
/// file that opens in the browser.
fn media_messages(net: Network, chat: &ChatSummary, end: i64) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = MEDIA_SAMPLES
        .iter()
        .enumerate()
        .map(
            |(i, &(kind, name, mime, size, fetch_ref, text))| ChatMessage {
                network: net,
                chat_id: chat.id.clone(),
                id: format!("{}-media{i}", chat.id),
                ts: end + 11_000 + i as i64 * 1_000,
                sender: "Alex Example".to_string(),
                text: text.to_string(),
                state: MessageState::Sent,
                attachments: vec![Attachment {
                    kind,
                    name: Some(name.to_string()),
                    mime: Some(mime.to_string()),
                    size: Some(size),
                    fetch_ref: FetchRef::new(fetch_ref),
                    open_url: None,
                }],
                ..ChatMessage::blank(net)
            },
        )
        .collect();
    if net == Network::Teams {
        out.push(ChatMessage {
            network: net,
            chat_id: chat.id.clone(),
            id: format!("{}-media-file", chat.id),
            ts: end + 20_000,
            sender: "Alex Example".to_string(),
            text: "[file] a OneDrive file".to_string(),
            state: MessageState::Sent,
            attachments: vec![Attachment {
                kind: MediaKind::Document,
                name: Some("report.docx".to_string()),
                mime: None,
                size: Some(88_000),
                fetch_ref: FetchRef::new("demo:browser"),
                open_url: Some(
                    "https://files.example.invalid/personal/alex/report.docx".to_string(),
                ),
            }],
            ..ChatMessage::blank(net)
        });
    }
    out
}

/// A small made-up picture (a sky, a sun and two hills) so `v` has something to draw.
fn demo_png() -> Vec<u8> {
    let (w, h) = (192u32, 120u32);
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let sun = ((fx - 140.0).powi(2) + (fy - 38.0).powi(2)).sqrt() < 18.0;
        let hill1 = fy > 92.0 - 14.0 * (fx / 40.0).sin();
        let hill2 = fy > 104.0 - 8.0 * ((fx + 30.0) / 25.0).cos();
        if sun {
            image::Rgb([255, 214, 64])
        } else if hill2 {
            image::Rgb([34, 110, 52])
        } else if hill1 {
            image::Rgb([70, 150, 84])
        } else {
            let t = (fy / h as f32 * 120.0) as u8;
            image::Rgb([40 + t, 100 + t / 2, 200])
        }
    });
    let mut bytes = Vec::new();
    // Writing a PNG into memory cannot fail.
    let _ = image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png);
    bytes
}

/// A made-up 96x96 portrait for a demo id: a colour from the id, a head and shoulders.
fn demo_avatar(id: &str) -> Vec<u8> {
    // FNV-1a: the same id always gets the same colour.
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let base = [
        60 + (hash & 0x7f) as u8,
        60 + ((hash >> 8) & 0x7f) as u8,
        60 + ((hash >> 16) & 0x7f) as u8,
    ];
    let light = base.map(|c| c.saturating_add(90));
    let img = image::RgbImage::from_fn(96, 96, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let head = ((fx - 48.0).powi(2) + (fy - 38.0).powi(2)).sqrt() < 16.0;
        let shoulders = ((fx - 48.0).powi(2) + (fy - 100.0).powi(2)).sqrt() < 38.0;
        image::Rgb(if head || shoulders { light } else { base })
    });
    let mut bytes = Vec::new();
    // Writing a PNG into memory cannot fail.
    let _ = image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png);
    bytes
}

/// What the demo answers for one person: a portrait for a `demo:` id, nothing for Sam Sample
/// (so the initials fallback can be seen) and for anything else.
fn avatar_for(net: Network, media: &MediaCache, id: &str) -> Option<PathBuf> {
    if !id.starts_with("demo:") || id == "demo:Sam Sample" {
        return None;
    }
    media.store_avatar(net, id, &demo_avatar(id)).ok()
}

/// Puts `bytes` in the media cache like a real download (once: later asks find it).
fn cache_file(
    media: &MediaCache,
    net: Network,
    fetch_ref: &FetchRef,
    ext: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    if let Some(path) = media.lookup(net, fetch_ref, ext) {
        return Ok(path);
    }
    let mut part = media.begin(net, fetch_ref, ext)?;
    part.write_all(bytes)
        .map_err(|e| format!("demo: could not write the cache file ({e})."))?;
    media.commit(part)
}

/// What the demo answers to `FetchMedia`: the same events a real backend sends, so every state
/// of an attachment can be tried (the file opens, the executable is refused by the TUI, an
/// expired link, a file over the cap, a OneDrive file with only a browser address).
fn fetch_answer(
    net: Network,
    media: &MediaCache,
    chat_id: String,
    message_id: String,
    index: u32,
    att: Option<Attachment>,
) -> Event {
    let failed = |reason: String, fallback_url: Option<String>| Event::MediaFailed {
        network: net,
        chat_id: chat_id.clone(),
        message_id: message_id.clone(),
        index,
        reason,
        fallback_url,
    };
    let Some(att) = att else {
        return failed("That attachment is not here any more.".to_string(), None);
    };
    if let Err(e) = media.check_size(att.size) {
        return failed(e, None);
    }
    let cached = match att.fetch_ref.as_str() {
        "demo:image" => cache_file(media, net, &att.fetch_ref, "png", &demo_png()),
        "demo:text" => cache_file(
            media,
            net,
            &att.fetch_ref,
            "txt",
            b"Demo notes\r\n\r\nThis file came from the chatcli demo backend.\r\n",
        ),
        "demo:expired" => Err("MEDIA EXPIRED".to_string()),
        "demo:browser" => return failed("FILE: open in browser".to_string(), att.open_url),
        _ => Err("The demo cannot fetch this attachment.".to_string()),
    };
    match cached {
        Ok(path) => Event::MediaReady {
            network: net,
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
            index,
            path,
        },
        Err(reason) => failed(reason, None),
    }
}

/// Sets, replaces or removes the user's one reaction on `m`, keeping the contract's order
/// (count descending, then emoji).
fn react(m: &mut ChatMessage, emoji: &str, remove: bool) {
    if let Some(i) = m.reactions.iter().position(|r| r.mine) {
        m.reactions[i].mine = false;
        m.reactions[i].count -= 1;
        if m.reactions[i].count == 0 {
            m.reactions.remove(i);
        }
    }
    if !remove {
        match m.reactions.iter_mut().find(|r| r.emoji == emoji) {
            Some(r) => {
                r.count += 1;
                r.mine = true;
            }
            None => m.reactions.push(Reaction {
                emoji: emoji.to_string(),
                count: 1,
                mine: true,
            }),
        }
    }
    m.reactions
        .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.emoji.cmp(&b.emoji)));
}

async fn backend(
    net: Network,
    mut commands: CommandRx,
    events: EventTx,
    store: Store,
    mut media: MediaCache,
) {
    let send = |ev: Event| {
        let _ = events.send(ev);
    };
    let mut chats = seed_chats(net);
    // Every message of every opened chat, so edits, deletes and reactions stick.
    let mut held: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    let mut next_id = 0u32;
    let mut incoming_at: Option<Instant> = None;
    // WhatsApp's typing script: someone starts typing, then their message arrives.
    let mut typing_at: Option<(Instant, bool)> = None;
    // Older pages given per chat.
    let mut older_pages: HashMap<String, u32> = HashMap::new();
    send(Event::Status(net, Status::Starting));
    send(Event::Status(net, Status::NeedsLogin { reason: None }));
    loop {
        let due = incoming_at;
        let incoming = async move {
            match due {
                Some(t) => sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        let typing_due = typing_at.map(|t| t.0);
        let typing_timer = async move {
            match typing_due {
                Some(t) => sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = typing_timer => {
                let Some((_, arrives)) = typing_at.take() else { continue };
                let Some(chat) = chats.first() else { continue };
                let (chat_id, sender) = (chat.id.clone(), "Riley Sample".to_string());
                if arrives {
                    // The message ends the indicator, before its 10 s are up.
                    next_id += 1;
                    send(Event::Message(ChatMessage {
                        network: net,
                        chat_id: chat_id.clone(),
                        id: format!("demo-typed-{next_id}"),
                        ts: now_ms(),
                        sender,
                        text: "demo: the typing indicator ended because this message arrived".to_string(),
                        state: MessageState::Sent,
                        ..ChatMessage::blank(net)
                    }));
                } else {
                    send(Event::Typing { network: net, chat_id, sender, active: true });
                    typing_at = Some((Instant::now() + Duration::from_secs(4), true));
                }
            }
            _ = incoming => {
                incoming_at = None;
                if let Some(chat) = chats.iter_mut().find(|c| c.id != REFUSING_CHAT) {
                    next_id += 1;
                    chat.unread += 1;
                    chat.last_ts = Some(now_ms());
                    chat.last_preview = "Alex Example: demo message that just arrived".to_string();
                    send(Event::Message(ChatMessage {
                        network: net,
                        chat_id: chat.id.clone(),
                        id: format!("demo-in-{next_id}"),
                        ts: now_ms(),
                        sender: "Alex Example".to_string(),
                        sender_id: demo_id("Alex Example"),
                        from_me: false,
                        text: "demo message that just arrived".to_string(),
                        state: MessageState::Sent,
                        ..ChatMessage::blank(net)
                    }));
                    send(Event::ChatUpdated(chat.clone()));
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    Command::Login => {
                        if net == Network::WhatsApp {
                            send(Event::Status(net, Status::Qr { code: SAMPLE_QR.to_string() }));
                            sleep(Duration::from_secs(4)).await;
                        } else {
                            send(Event::Status(net, Status::LoggingIn));
                            sleep(Duration::from_millis(1500)).await;
                        }
                        send(Event::Status(net, Status::Online { live: true, detail: None }));
                        send(Event::Capabilities { network: net, caps: Capabilities::all() });
                        send(Event::Chats(net, chats.clone()));
                        incoming_at = Some(Instant::now() + Duration::from_secs(6));
                        if net == Network::WhatsApp {
                            typing_at = Some((Instant::now() + Duration::from_secs(7), false));
                        }
                    }
                    Command::Logout => {
                        send(Event::Status(net, Status::NeedsLogin { reason: None }));
                    }
                    Command::Refresh => {
                        send(Event::Chats(net, chats.clone()));
                        send(Event::Notice { network: net, text: "demo: chat list refreshed".to_string(), is_error: false });
                    }
                    Command::OpenChat { chat_id } => {
                        if let Some(chat) = chats.iter().find(|c| c.id == chat_id) {
                            let messages = held
                                .entry(chat_id.clone())
                                .or_insert_with(|| history(net, chat))
                                .clone();
                            send(Event::Messages { network: net, chat_id, messages });
                        }
                    }
                    Command::MarkRead { chat_id } => {
                        if let Some(chat) = chats.iter_mut().find(|c| c.id == chat_id && c.unread > 0) {
                            chat.unread = 0;
                            send(Event::ChatUpdated(chat.clone()));
                        }
                    }
                    Command::Send { chat_id, text, client_id, reply_to, mentions } => {
                        let events = events.clone();
                        if chat_id == REFUSING_CHAT {
                            tokio::spawn(async move {
                                sleep(Duration::from_millis(500)).await;
                                let _ = events.send(Event::MessageFailed {
                                    network: net,
                                    chat_id,
                                    client_id,
                                    error: "demo: this chat refuses messages".to_string(),
                                });
                            });
                            continue;
                        }
                        next_id += 1;
                        let message = ChatMessage {
                            network: net,
                            chat_id: chat_id.clone(),
                            id: format!("demo-out-{next_id}"),
                            ts: now_ms(),
                            sender: "You".to_string(),
                            sender_id: demo_id("You"),
                            from_me: true,
                            text,
                            state: MessageState::Sent,
                            reply_to,
                            mentions,
                            ..ChatMessage::blank(net)
                        };
                        held.entry(chat_id).or_default().push(message.clone());
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(500)).await;
                            let _ = events.send(Event::MessageConfirmed { client_id, message });
                        });
                    }
                    Command::React { chat_id, message_id, emoji, remove } => {
                        let found = held
                            .get_mut(&chat_id)
                            .and_then(|l| l.iter_mut().find(|m| m.id == message_id));
                        if let Some(m) = found {
                            react(m, &emoji, remove);
                            send(Event::MessageChanged(m.clone()));
                        }
                    }
                    Command::Edit { chat_id, message_id, text } => {
                        let found = held
                            .get_mut(&chat_id)
                            .and_then(|l| l.iter_mut().find(|m| m.id == message_id));
                        match found {
                            Some(m) if m.from_me => {
                                m.text = text;
                                m.edited = true;
                                send(Event::MessageChanged(m.clone()));
                            }
                            _ => send(Event::Notice {
                                network: net,
                                text: "demo: only your own messages can be edited.".to_string(),
                                is_error: true,
                            }),
                        }
                    }
                    Command::Delete { chat_id, message_id } => {
                        let found = held
                            .get_mut(&chat_id)
                            .and_then(|l| l.iter_mut().find(|m| m.id == message_id));
                        match found {
                            Some(m) if m.from_me => {
                                m.state = MessageState::Deleted;
                                m.text.clear();
                                m.reactions.clear();
                                m.attachments.clear();
                                send(Event::MessageChanged(m.clone()));
                            }
                            _ => send(Event::Notice {
                                network: net,
                                text: "demo: only your own messages can be deleted.".to_string(),
                                is_error: true,
                            }),
                        }
                    }
                    Command::LoadMembers { chat_id } => {
                        let list = members(net);
                        // Like a real backend: the cache first, then the event.
                        if let Err(e) = store.replace_members(net, &chat_id, &list) {
                            chatcli_core::log::error("demo", &format!("members not cached: {e}"));
                        }
                        send(Event::Members { network: net, chat_id, members: list, error: None });
                    }
                    Command::FetchMedia { chat_id, message_id, index } => {
                        let att = held
                            .get(&chat_id)
                            .and_then(|l| l.iter().find(|m| m.id == message_id))
                            .and_then(|m| m.attachments.get(index as usize))
                            .cloned();
                        let (events, media) = (events.clone(), media.clone());
                        // A short wait, so the "downloading" state can be seen.
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(700)).await;
                            let _ = events.send(fetch_answer(net, &media, chat_id, message_id, index, att));
                        });
                    }
                    Command::FetchAvatars { people } => {
                        let (events, media) = (events.clone(), media.clone());
                        // A short wait, so the initials can be seen first.
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(400)).await;
                            for person_id in people {
                                let path = avatar_for(net, &media, &person_id);
                                let _ = events.send(Event::Avatar { network: net, person_id, path });
                            }
                        });
                    }
                    Command::SendMedia { chat_id, client_id, path, kind, caption } => {
                        let events = events.clone();
                        if chat_id == REFUSING_CHAT {
                            tokio::spawn(async move {
                                sleep(Duration::from_millis(500)).await;
                                let _ = events.send(Event::MessageFailed {
                                    network: net,
                                    chat_id,
                                    client_id,
                                    error: "demo: this chat refuses messages".to_string(),
                                });
                            });
                            continue;
                        }
                        // The backend reads the file, never the TUI: the demo only measures it.
                        let total = match std::fs::metadata(&path) {
                            Ok(m) if m.is_file() => m.len(),
                            _ => {
                                send(Event::MessageFailed {
                                    network: net,
                                    chat_id,
                                    client_id,
                                    error: "FILE NOT FOUND".to_string(),
                                });
                                continue;
                            }
                        };
                        next_id += 1;
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let marker = match kind {
                            MediaKind::Image => "[image]".to_string(),
                            _ => format!("[document: {name}]"),
                        };
                        let message = ChatMessage {
                            network: net,
                            chat_id: chat_id.clone(),
                            id: format!("demo-out-{next_id}"),
                            ts: now_ms(),
                            sender: "You".to_string(),
                            sender_id: demo_id("You"),
                            from_me: true,
                            text: if caption.is_empty() { marker } else { format!("{marker} {caption}") },
                            state: MessageState::Sent,
                            attachments: vec![Attachment {
                                kind,
                                name: Some(name),
                                mime: None,
                                size: Some(total),
                                fetch_ref: FetchRef::new(if kind == MediaKind::Image { "demo:image" } else { "demo:text" }),
                                open_url: None,
                            }],
                            ..ChatMessage::blank(net)
                        };
                        held.entry(chat_id.clone()).or_default().push(message.clone());
                        tokio::spawn(async move {
                            for step in 1..=3u64 {
                                sleep(Duration::from_millis(350)).await;
                                let _ = events.send(Event::SendProgress {
                                    network: net,
                                    chat_id: chat_id.clone(),
                                    client_id: client_id.clone(),
                                    done: total * step / 3,
                                    total,
                                });
                            }
                            let _ = events.send(Event::MessageConfirmed { client_id, message });
                        });
                    }
                    Command::LoadOlder { chat_id, before_ts, before_id } => {
                        // Like a real backend: the store first, the "network" only when the
                        // store has nothing older. The demo network has OLDER_PAGES pages.
                        let mut batch = store
                            .messages_before(net, &chat_id, before_ts, &before_id, 50)
                            .unwrap_or_default();
                        let mut at_start = false;
                        if batch.is_empty() {
                            let page = older_pages.entry(chat_id.clone()).or_insert(0);
                            if *page < OLDER_PAGES {
                                *page += 1;
                                batch = older_page(net, &chat_id, before_ts, *page);
                                if let Err(e) = store.upsert_messages(&batch) {
                                    chatcli_core::log::error("demo", &format!("older page not stored: {e}"));
                                }
                            }
                            at_start = *page >= OLDER_PAGES;
                        }
                        let list = held.entry(chat_id.clone()).or_default();
                        let fresh: Vec<ChatMessage> = batch
                            .iter()
                            .filter(|m| !list.iter().any(|h| h.id == m.id))
                            .cloned()
                            .collect();
                        list.extend(fresh);
                        list.sort_by_key(|m| m.ts);
                        // A moment of "loading older...", so it can be seen.
                        let events = events.clone();
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(600)).await;
                            let _ = events.send(Event::OlderMessages { network: net, chat_id, messages: batch, at_start });
                        });
                    }
                    // Typing: only WhatsApp has it (the TUI never sends it to Teams, and a real
                    // Teams backend drops it). The demo says what it received.
                    Command::Typing { active, .. } if net == Network::WhatsApp => {
                        send(Event::Notice {
                            network: net,
                            text: format!("demo: typing {} sent", if active { "composing" } else { "paused" }),
                            is_error: false,
                        });
                    }
                    cmd @ Command::Typing { .. } => {
                        for ev in unsupported(net, &cmd) {
                            send(ev);
                        }
                    }
                    Command::SetChatFlag { chat_id, flag, on } => {
                        // Like a real backend: store the flag, then say how the chat looks.
                        if let Some(chat) = chats.iter_mut().find(|c| c.id == chat_id) {
                            match flag {
                                ChatFlag::Pinned => chat.flags.pinned = on,
                                ChatFlag::Muted => chat.flags.muted_until = on.then_some(MUTE_FOREVER),
                                ChatFlag::Hidden => chat.flags.hidden = on,
                            }
                            send(Event::ChatUpdated(chat.clone()));
                        }
                    }
                    // Like the real backends: new `[media]` limits apply at once.
                    Command::Settings(s) => media = media.with_limits(&s.media),
                    Command::Shutdown => return,
                }
            }
        }
    }
}
