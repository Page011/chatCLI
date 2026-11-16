//! Fake backends for `chatcli --demo`: try the whole screen with no accounts and no network.
//! Generic names only. Each network starts signed out; sign-in takes a moment, then chats
//! appear. One chat per network refuses every send so the NOT SENT state can be seen.

use std::collections::HashMap;
use std::time::Duration;

use chatcli_core::model::{
    now_ms, ChatKind, ChatMessage, ChatSummary, Command, CommandRx, CommandTx, Event, EventRx,
    EventTx, MessageState, Network, Status,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, Instant};

const REFUSING_CHAT: &str = "demo-refuse";
/// A short made-up payload: small enough to draw in a normal window.
const SAMPLE_QR: &str = "2@demo-sample-payload-not-a-real-link-code,AAAA,BBBB,CCCC";

/// (title, group, unread, minutes since last message)
const TEAMS_CHATS: [(&str, bool, u32, i64); 8] = [
    ("Alex Example", false, 2, 5),
    ("Project Team", true, 0, 80),
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

const PEOPLE: [&str; 3] = ["Alex Example", "Sam Sample", "Jordan Placeholder"];

/// Starts one fake backend per network. Call inside a tokio runtime.
pub fn spawn() -> (EventRx, HashMap<Network, CommandTx>) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut commands = HashMap::new();
    for net in Network::ALL {
        let (tx, rx) = mpsc::unbounded_channel();
        commands.insert(net, tx);
        tokio::spawn(backend(net, rx, events_tx.clone()));
    }
    (events_rx, commands)
}

fn seed_chats(net: Network) -> Vec<ChatSummary> {
    let table: &[(&str, bool, u32, i64)] = match net {
        Network::Teams => &TEAMS_CHATS,
        Network::WhatsApp => &WHATSAPP_CHATS,
    };
    let now = now_ms();
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
        })
        .collect()
}

fn history(net: Network, chat: &ChatSummary) -> Vec<ChatMessage> {
    let end = chat.last_ts.unwrap_or_else(now_ms);
    (0..LINES.len())
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
            }
        })
        .collect()
}

async fn backend(net: Network, mut commands: CommandRx, events: EventTx) {
    let send = |ev: Event| {
        let _ = events.send(ev);
    };
    let mut chats = seed_chats(net);
    let mut sent: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    let mut next_id = 0u32;
    let mut incoming_at: Option<Instant> = None;
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
        tokio::select! {
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
                        from_me: false,
                        text: "demo message that just arrived".to_string(),
                        state: MessageState::Sent,
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
                        send(Event::Chats(net, chats.clone()));
                        incoming_at = Some(Instant::now() + Duration::from_secs(6));
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
                            let mut messages = history(net, chat);
                            messages.extend(sent.get(&chat_id).cloned().unwrap_or_default());
                            send(Event::Messages { network: net, chat_id, messages });
                        }
                    }
                    Command::MarkRead { chat_id } => {
                        if let Some(chat) = chats.iter_mut().find(|c| c.id == chat_id && c.unread > 0) {
                            chat.unread = 0;
                            send(Event::ChatUpdated(chat.clone()));
                        }
                    }
                    Command::Send { chat_id, text, client_id } => {
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
                            from_me: true,
                            text,
                            state: MessageState::Sent,
                        };
                        sent.entry(chat_id).or_default().push(message.clone());
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(500)).await;
                            let _ = events.send(Event::MessageConfirmed { client_id, message });
                        });
                    }
                    Command::Shutdown => return,
                }
            }
        }
    }
}
