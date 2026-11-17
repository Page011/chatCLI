//! The WhatsApp backend task: one tokio task that owns the whatsapp-rust session, the TUI's
//! commands and the store. Everything that touches state runs on this one task (library events
//! are forwarded into its channel), so there are no locks and no races between handlers.
//!
//! Failures are never silent: every way the link can end (unlinked, replaced, banned, outdated, QR
//! expired) is a Status the TUI shows plus a log line, and the library's reconnect loop is
//! stopped so nothing hammers WhatsApp in the background.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chatcli_core::media::MediaCache;
use chatcli_core::model::{
    now_ms, Capabilities, ChatFlag, ChatFlags, ChatMessage, ChatSummary, Command, CommandRx,
    Event as Out, EventTx, MessageState, Network, PersonRef, QuotedRef, Status,
};
use chatcli_core::runtime::BackendCtx;
use chatcli_core::settings::Settings;
use chatcli_core::store::Store;
use chatcli_core::{chain, log, paths};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;
use whatsapp_rust::prelude::*;
use whatsapp_rust::transport::TokioWebSocketTransportFactory;
use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;
use whatsapp_rust::wacore::types::events::ConnectFailureReason;
use whatsapp_rust::wacore_binary::JidExt;

use crate::map::{self, Content, Names};
use crate::rich;
use crate::tls;
use crate::wa::{RealWa, Wa};

#[path = "ops.rs"]
mod ops;
#[path = "receipts.rs"]
mod receipts;

const AREA: &str = "whatsapp";
const NET: Network = Network::WhatsApp;
/// A chat opens with what the store holds; older messages come on demand (`LoadOlder`).
const OPEN_LIMIT: u32 = 200;
/// Names arrive in bursts (thousands of contacts on first sync): retitle once, after they stop.
const RETITLE_DELAY: Duration = Duration::from_millis(1500);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// A resumed session that has not connected by now is shown as NOT LIVE, not as CONNECTING forever.
const CONNECT_WAIT: Duration = Duration::from_secs(20);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
/// Senders remembered per chat so read receipts in groups can name who wrote a message.
const SENDERS_PER_CHAT: usize = 200;
const DELETE_PENDING_KEY: &str = "whatsapp.delete_pending";
const NOT_CONNECTED: &str = "WhatsApp is not connected. Wait for LIVE or press Enter to link.";
const LINK_FILE_BUSY: &str = "The old WhatsApp link file is still in use, so it is deleted later.";
/// Deleting the session file is retried this often, this many times (5 s in all).
const DELETE_TRIES: u32 = 25;
const DELETE_STEP: Duration = Duration::from_millis(200);
/// Profile photo lookups: at most this many a minute, this far apart, this many waiting.
const AVATAR_PER_MIN: usize = 10;
const AVATAR_GAP: Duration = Duration::from_secs(2);
const AVATAR_QUEUE_MAX: usize = 64;
/// Most lookups in one run: past this, everyone is answered with no photo.
const AVATAR_SESSION_MAX: usize = 200;

/// `HistorySyncType::ON_DEMAND`: the answer to a `fetch_message_history` request.
const ON_DEMAND_SYNC: i32 = 6;

/// How long Shutdown waits for running uploads, downloads and timers to stop.
const TASK_STOP: Duration = Duration::from_secs(5);

/// Runs until `Command::Shutdown` or the command channel closes.
pub async fn run(ctx: BackendCtx, mut commands: CommandRx, events: EventTx) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut b = Backend::new(ctx, events, tx);
    b.set_status(Status::Starting);
    b.clear_pending_delete().await;
    // Stored data first, so the sidebar the user sees on start is already corrected: saved
    // names back in memory, future-dated messages moved, titles improved from what is stored.
    b.load_names();
    b.repair_future_dates();
    b.retitle();
    b.emit_chats();
    b.start(false).await;
    b.learn_lid_links().await;
    loop {
        let retitle_at = b.retitle_at;
        let retitle = async {
            match retitle_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        let avatar_at = b.avatar_at;
        let avatar = async {
            match avatar_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        let connect_by = b.connect_by;
        let connect_wait = async {
            match connect_by {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(Command::Shutdown) | None => break,
                Some(c) => b.on_command(c).await,
            },
            Some(m) = rx.recv() => b.on_msg(m).await,
            _ = retitle => {
                b.retitle_at = None;
                b.retitle();
            }
            _ = avatar => b.schedule_avatar(),
            _ = connect_wait => {
                b.connect_by = None;
                b.connect_timeout();
            }
        }
    }
    b.stop_session().await;
    b.stop_tasks().await;
    b.flush_names();
}

/// Everything that reaches the task besides TUI commands.
pub(crate) enum Msg {
    /// A library event, tagged with the session generation that produced it.
    Wa(u64, Arc<Event>),
    /// Boxed: a finished send carries the whole message.
    Sent(Box<Sent>),
    /// The result of a slow operation that ran on its own task. State changes happen here, on
    /// the one task that owns the store.
    Done(Done),
}

/// A finished send (text or media). `draft` is the message as it will be stored: its id is the
/// client id until the network's id replaces it.
pub(crate) struct Sent {
    chat_id: String,
    client_id: String,
    draft: ChatMessage,
    result: Result<String, String>,
}

pub(crate) enum Done {
    Reacted {
        chat_id: String,
        message_id: String,
        /// None = the reaction was removed.
        emoji: Option<String>,
        result: Result<(), String>,
    },
    Edited {
        chat_id: String,
        message_id: String,
        text: String,
        mentions: Vec<PersonRef>,
        result: Result<(), String>,
    },
    Deleted {
        chat_id: String,
        message_id: String,
        result: Result<(), String>,
    },
    Fetched {
        chat_id: String,
        message_id: String,
        index: u32,
        result: Result<PathBuf, String>,
    },
    Members {
        chat_id: String,
        result: Result<Vec<PersonRef>, String>,
    },
    /// The member list fetched for receipts (not an answer to LoadMembers), as the library
    /// gives it (each member's phone number kept). `gen` is the session it started under; an
    /// answer from an older one is dropped.
    ReadMembers {
        gen: u64,
        chat_id: String,
        result: Result<Vec<crate::wa::Member>, String>,
    },
    /// The phone-history request left (Ok) or failed to leave (Err).
    OlderAsked {
        chat_id: String,
        token: u64,
        result: Result<String, String>,
    },
    /// The 30 s deadline of one older-history request passed.
    OlderDeadline { chat_id: String, token: u64 },
    /// The phone was told the chat is read (Ok), or the call failed (Err).
    MarkedRead {
        chat_id: String,
        unread: u32,
        mentions: u32,
        result: Result<(), String>,
    },
    /// A pin, mute or archive write-back finished. `prior` is what to put back on failure.
    Flagged {
        chat_id: String,
        prior: ChatFlags,
        result: Result<(), String>,
    },
    /// One profile photo lookup finished: the stored picture, or None. `gen` is the
    /// `avatar_gen` it started under; an older one is dropped.
    Avatar {
        gen: u64,
        person_id: String,
        path: Option<PathBuf>,
    },
}

struct Job {
    jid: Jid,
    chat_id: String,
    client_id: String,
    message: wa::Message,
    draft: ChatMessage,
}

struct Session {
    handle: BotHandle,
    client: Arc<Client>,
}

/// The connection's send side: the seam to the library, and the queue that keeps text messages
/// in order. A slow send (first message to a big group) never blocks incoming events.
struct Link {
    wa: Arc<dyn Wa>,
    jobs: mpsc::UnboundedSender<Job>,
}

/// One outstanding "older messages" request to the phone.
struct OlderPending {
    token: u64,
    /// The request id the phone's answer carries; None until the request has left.
    request_id: Option<String>,
    /// The oldest stored message when asked: the answer is whatever lies before it.
    before_ts: i64,
    before_id: String,
}

/// Forwards library events into the task's channel. Inline, in dispatch order, no client
/// reference held (a closure handler would keep the session database open after logout).
struct Forward {
    tx: mpsc::UnboundedSender<Msg>,
    gen: u64,
}

impl EventHandler for Forward {
    fn handle_event(&self, event: Arc<Event>) {
        let _ = self.tx.send(Msg::Wa(self.gen, event));
    }

    fn interest(&self) -> EventInterest {
        EventInterest::of(&[
            EventKind::Connected,
            EventKind::Disconnected,
            EventKind::PairSuccess,
            EventKind::PairError,
            EventKind::LoggedOut,
            EventKind::PairingQrCode,
            EventKind::PairingQrCodesExhausted,
            EventKind::Receipt,
            EventKind::QrScannedWithoutMultidevice,
            EventKind::ClientOutdated,
            EventKind::Messages,
            EventKind::UndecryptableMessage,
            EventKind::PushNameUpdate,
            EventKind::ContactUpdate,
            EventKind::GroupUpdate,
            EventKind::HistorySync,
            EventKind::ChatPresence,
            EventKind::StreamReplaced,
            EventKind::TemporaryBan,
            EventKind::ConnectFailure,
            EventKind::MarkChatAsReadUpdate,
            EventKind::PinUpdate,
            EventKind::MuteUpdate,
            EventKind::ArchiveUpdate,
            EventKind::AppStateSyncFailed,
        ])
    }
}

pub(crate) struct Backend {
    store: Store,
    settings: Arc<Settings>,
    media: MediaCache,
    events: EventTx,
    tx: mpsc::UnboundedSender<Msg>,
    session: Option<Session>,
    link: Option<Link>,
    /// Uploads, downloads and deadlines. Shutdown aborts them.
    tasks: JoinSet<()>,
    /// Bumped whenever a session starts or stops; events from older sessions are dropped.
    gen: u64,
    last_status: Status,
    names: Names,
    senders: HashMap<String, VecDeque<(String, Jid)>>,
    pub(crate) retitle_at: Option<Instant>,
    /// Profile photo ids waiting for a lookup, oldest first.
    avatar_queue: VecDeque<String>,
    /// Ids queued or looked up this session: never asked twice.
    avatar_seen: HashSet<String>,
    /// When the lookups of the last minute started.
    avatar_sent: VecDeque<Instant>,
    /// Lookups started this run (one past the cap once the cap was logged).
    avatar_total: usize,
    /// Bumped when the session stops: a lookup started before then writes nothing and is not shown.
    avatar_gen: Arc<std::sync::atomic::AtomicU64>,
    /// When the next lookup may start (the run loop wakes then).
    pub(crate) avatar_at: Option<Instant>,
    /// When a connection attempt counts as stuck; None while connected or not trying.
    pub(crate) connect_by: Option<Instant>,
    undecryptable: u64,
    /// The chat the TUI last asked for: history imported into it is re-sent.
    open_chat: Option<String>,
    older: HashMap<String, OlderPending>,
    older_token: u64,
    /// When a "typing" stanza last went out, per chat (the 5 s throttle).
    typing_sent: HashMap<String, Instant>,
    /// Chats where we sent "composing" and no "paused" has followed.
    typing_open: HashSet<String>,
    /// (chat, sender label) pairs shown as typing, so a message from them can clear it.
    typing_in: HashSet<(String, String)>,
    /// Groups whose members were fetched this run for read ticks.
    read_members_asked: HashSet<String>,
}

impl Backend {
    pub(crate) fn new(ctx: BackendCtx, events: EventTx, tx: mpsc::UnboundedSender<Msg>) -> Backend {
        Backend {
            store: ctx.store,
            settings: ctx.settings,
            media: ctx.media,
            events,
            tx,
            session: None,
            link: None,
            tasks: JoinSet::new(),
            gen: 0,
            last_status: Status::Starting,
            names: Names::default(),
            senders: HashMap::new(),
            retitle_at: None,
            avatar_queue: VecDeque::new(),
            avatar_seen: HashSet::new(),
            avatar_sent: VecDeque::new(),
            avatar_total: 0,
            avatar_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            avatar_at: None,
            connect_by: None,
            undecryptable: 0,
            open_chat: None,
            older: HashMap::new(),
            older_token: 0,
            typing_sent: HashMap::new(),
            typing_open: HashSet::new(),
            typing_in: HashSet::new(),
            read_members_asked: HashSet::new(),
        }
    }

    // -- output -----------------------------------------------------------------------------

    fn emit(&self, e: Out) {
        // The TUI going away is not this task's problem: Shutdown or a closed channel ends it.
        let _ = self.events.send(e);
    }

    pub(crate) fn set_status(&mut self, s: Status) {
        self.last_status = s.clone();
        self.emit(Out::Status(NET, s));
    }

    fn notice(&self, text: &str, is_error: bool) {
        if is_error {
            log::error(AREA, text);
        } else {
            log::info(AREA, text);
        }
        self.emit(Out::Notice {
            network: NET,
            text: text.to_string(),
            is_error,
        });
    }

    /// A store failure is user-visible (chats may be missing) and logged.
    fn store_failed(&self, what: &str, e: &str) {
        log::error(AREA, &format!("{what}: {e}"));
        self.emit(Out::Notice {
            network: NET,
            text: "Could not save WhatsApp messages locally. See chatcli.log.".to_string(),
            is_error: true,
        });
    }

    /// The first connection of a resumed session is overdue (network down, WhatsApp unreachable).
    /// The library keeps retrying by itself; this only makes the wait visible.
    pub(crate) fn connect_timeout(&mut self) {
        if matches!(self.last_status, Status::Connecting) {
            log::error(
                AREA,
                "no connection to WhatsApp after 20 s; the library keeps retrying",
            );
            self.set_status(Status::Online {
                live: false,
                detail: Some("cannot reach WhatsApp, retrying".to_string()),
            });
        }
    }

    pub(crate) fn emit_chats(&self) {
        match self.store.chats(NET) {
            Ok(chats) => self.emit(Out::Chats(NET, chats)),
            Err(e) => self.store_failed("read chats", &e),
        }
    }

    // -- commands ---------------------------------------------------------------------------

    pub(crate) async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Login => {
                if self.session.is_some() {
                    // Already linking or running: show where it stands instead of restarting.
                    self.emit(Out::Status(NET, self.last_status.clone()));
                } else {
                    self.start(true).await;
                }
            }
            Command::Logout => self.logout().await,
            Command::Refresh => {
                self.emit_chats();
                self.refresh_groups().await;
            }
            Command::OpenChat { chat_id } => {
                self.open_chat = Some(chat_id.clone());
                match self.store.messages(NET, &chat_id, OPEN_LIMIT) {
                    Ok(messages) => self.emit(Out::Messages {
                        network: NET,
                        chat_id: chat_id.clone(),
                        messages,
                    }),
                    Err(e) => self.store_failed("read messages", &e),
                }
                self.emit_receipts(&chat_id);
            }
            // Sending in a chat reads it, as on the phone. `mark_read` does nothing when
            // nothing is unread, so a normal reply costs no extra call.
            Command::Send {
                chat_id,
                text,
                client_id,
                reply_to,
                mentions,
            } => {
                self.mark_read(&chat_id);
                self.send(chat_id, text, client_id, reply_to, mentions);
            }
            Command::MarkRead { chat_id } => self.mark_read(&chat_id),
            Command::SendMedia {
                chat_id,
                client_id,
                path,
                kind: _,
                caption,
            } => {
                self.mark_read(&chat_id);
                self.send_media(chat_id, client_id, path, caption);
            }
            Command::React {
                chat_id,
                message_id,
                emoji,
                remove,
            } => self.react(chat_id, message_id, emoji, remove),
            Command::Edit {
                chat_id,
                message_id,
                text,
            } => self.edit(chat_id, message_id, text),
            Command::Delete {
                chat_id,
                message_id,
            } => self.delete(chat_id, message_id),
            Command::FetchMedia {
                chat_id,
                message_id,
                index,
            } => self.fetch_media(chat_id, message_id, index),
            Command::LoadOlder {
                chat_id,
                before_ts,
                before_id,
            } => self.load_older(chat_id, before_ts, before_id),
            Command::LoadMembers { chat_id } => self.load_members(chat_id),
            Command::FetchAvatars { people } => self.fetch_avatars(people),
            Command::Typing { chat_id, active } => self.typing(chat_id, active).await,
            Command::SetChatFlag { chat_id, flag, on } => self.set_chat_flag(chat_id, flag, on),
            Command::Settings(s) => {
                self.media = self.media.with_limits(&s.media);
                self.settings = s;
            }
            Command::Shutdown => {}
        }
    }

    /// Reports a failed send and keeps the message as NOT SENT in the store, so it is still
    /// there after the chat is reopened.
    fn send_failed(&self, draft: ChatMessage, error: String) {
        let failed = ChatMessage {
            ts: now_ms(),
            state: MessageState::Failed(error.clone()),
            ..draft
        };
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&failed)) {
            self.store_failed("save unsent message", &e);
        }
        self.emit(Out::MessageFailed {
            network: NET,
            chat_id: failed.chat_id,
            client_id: failed.id,
            error,
        });
    }

    /// The message we are about to send, as the store will hold it. Its id is the client id until
    /// the network's id replaces it.
    fn draft(&self, chat_id: &str, client_id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            network: NET,
            chat_id: chat_id.to_string(),
            id: client_id.to_string(),
            ts: now_ms(),
            sender: map::OWN_LABEL.to_string(),
            from_me: true,
            text: text.to_string(),
            state: MessageState::Pending,
            sender_id: self.own_jid_string(),
            ..ChatMessage::blank(NET)
        }
    }

    fn own_jid_string(&self) -> String {
        self.names
            .own_pn()
            .map(|u| Jid::pn(u).to_string())
            .unwrap_or_default()
    }

    fn send(
        &mut self,
        chat_id: String,
        text: String,
        client_id: String,
        reply_to: Option<QuotedRef>,
        mentions: Vec<PersonRef>,
    ) {
        let mut draft = self.draft(&chat_id, &client_id, &text);
        if !chatcli_core::limits::text_fits(NET, &text) {
            let error = format!(
                "MESSAGE TOO LONG: WhatsApp takes up to {} characters, so shorten it.",
                chatcli_core::limits::WHATSAPP_TEXT_MAX_CHARS
            );
            return self.send_failed(draft, error);
        }
        let Some(link) = &self.link else {
            return self.send_failed(draft, NOT_CONNECTED.into());
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            let error = "This chat has no valid WhatsApp address. Press r to refresh.".to_string();
            return self.send_failed(draft, error);
        };
        // A reply must never go out as a plain message: the quote would silently vanish.
        let quote = match &reply_to {
            Some(q) => match self.quote_context(&jid, q) {
                Ok(ctx) => Some(ctx),
                Err(error) => return self.send_failed(draft, error),
            },
            None => None,
        };
        // Mentions only work in groups; in a direct chat the text goes out as typed.
        let wanted: &[PersonRef] = if jid.is_group() { &mentions } else { &[] };
        let (message, applied) = rich::text_message(&text, quote, wanted);
        draft.reply_to = reply_to;
        draft.mentions = applied;
        let job = Job {
            jid,
            chat_id,
            client_id,
            message,
            draft: draft.clone(),
        };
        if link.jobs.send(job).is_err() {
            self.send_failed(draft, NOT_CONNECTED.into());
        }
    }

    /// The `ContextInfo` that quotes `q`. WhatsApp needs the quoted author: stored with the
    /// message, else (rows from before 0.2) the sender remembered this run. A reply to an own
    /// message names us, in a direct chat the other side is the chat itself.
    fn quote_context(&self, chat: &Jid, q: &QuotedRef) -> Result<wa::ContextInfo, String> {
        let mut participant = q.sender_id.clone();
        if participant.is_empty() {
            participant = if q.sender == map::OWN_LABEL {
                self.own_jid_string()
            } else if !chat.is_group() {
                chat.to_string()
            } else {
                self.sender_of(&chat.to_string(), &q.id)
                    .map(|j| j.to_string())
                    .unwrap_or_default()
            };
        }
        if participant.is_empty() {
            return Err("Cannot reply to this older message: its sender is unknown.".to_string());
        }
        let preview = if q.preview.is_empty() {
            self.store
                .message(NET, &chat.to_string(), &q.id)
                .ok()
                .flatten()
                .map(|m| rich::one_line(&m.text, rich::QUOTE_PREVIEW_CHARS))
                .unwrap_or_default()
        } else {
            q.preview.clone()
        };
        Ok(rich::quote_context(q, &participant, &preview))
    }

    async fn on_sent(&mut self, s: Sent) {
        match s.result {
            Ok(id) => {
                let msg = ChatMessage {
                    id,
                    ts: now_ms(),
                    state: MessageState::Sent,
                    ..s.draft
                };
                if let Err(e) = self.store.confirm_message(&s.client_id, &msg) {
                    self.store_failed("save sent message", &e);
                }
                self.emit(Out::MessageConfirmed {
                    client_id: s.client_id,
                    message: msg.clone(),
                });
                self.own_message_stored(&s.chat_id, msg.ts);
                if let Ok(jid) = s.chat_id.parse::<Jid>() {
                    self.update_chat(&jid, &msg, false, "").await;
                }
            }
            Err(reason) => {
                log::error(AREA, &format!("send failed: {reason}"));
                self.send_failed(s.draft, reason);
            }
        }
    }

    /// The user read this chat. The local count clears at once. Then the read state goes to the
    /// phone (`mark_chat_as_read`, which clears the unread dot on the other devices). Read
    /// receipts, which tell the sender "seen", go out only when `send_read_receipts` is on.
    /// Offline there is nothing to tell: the count just clears locally.
    fn mark_read(&mut self, chat_id: &str) {
        let chat = match self.store.chat(NET, chat_id) {
            Ok(c) => c,
            Err(e) => return self.store_failed("read chat", &e),
        };
        let unread = chat.as_ref().map_or(0, |c| c.unread);
        let mentions = chat.as_ref().map_or(0, |c| c.mentions);
        if let Err(e) = self.store.set_unread(NET, chat_id, 0) {
            return self.store_failed("clear unread", &e);
        }
        if let Some(mut c) = chat {
            c.unread = 0;
            c.mentions = 0;
            self.emit(Out::ChatUpdated(c));
        }
        if unread == 0 {
            return;
        }
        let (Some(link), Ok(jid)) = (&self.link, chat_id.parse::<Jid>()) else {
            return;
        };
        let wa = link.wa.clone();
        let receipts = if self.settings.whatsapp.send_read_receipts {
            self.receipt_batches(chat_id, &jid, unread)
        } else {
            Vec::new()
        };
        let chat_id = chat_id.to_string();
        self.spawn_done(async move {
            let marked = tokio::time::timeout(NETWORK_TIMEOUT, wa.mark_chat_read(jid.clone()))
                .await
                .unwrap_or_else(|_| Err("WhatsApp did not answer in time".to_string()));
            if marked.is_ok() {
                for (sender, ids) in receipts {
                    // A failed receipt only means the sender keeps grey ticks: logged, not shown.
                    if let Err(e) = wa.mark_receipts(jid.clone(), sender, ids).await {
                        log::error(AREA, &format!("read receipt failed: {e}"));
                    }
                }
            }
            Done::MarkedRead {
                chat_id,
                unread,
                mentions,
                result: marked,
            }
        });
    }

    /// The newest `unread` incoming messages grouped by who wrote them, for read receipts.
    /// Group receipts must name the author; messages whose author is unknown are skipped.
    fn receipt_batches(
        &self,
        chat_id: &str,
        jid: &Jid,
        unread: u32,
    ) -> Vec<(Option<Jid>, Vec<String>)> {
        let Ok(recent) = self.store.messages(NET, chat_id, OPEN_LIMIT) else {
            return Vec::new();
        };
        let incoming: Vec<&ChatMessage> = recent.iter().filter(|m| !m.from_me).collect();
        let tail = &incoming[incoming.len().saturating_sub(unread as usize)..];
        let mut by_sender: HashMap<Option<Jid>, Vec<String>> = HashMap::new();
        for m in tail {
            if jid.is_group() {
                let stored = m.sender_id.parse::<Jid>().ok();
                if let Some(sender) = stored.or_else(|| self.sender_of(chat_id, &m.id)) {
                    by_sender
                        .entry(Some(sender))
                        .or_default()
                        .push(m.id.clone());
                }
            } else {
                by_sender.entry(None).or_default().push(m.id.clone());
            }
        }
        by_sender.into_iter().collect()
    }

    fn sender_of(&self, chat_id: &str, msg_id: &str) -> Option<Jid> {
        self.senders
            .get(chat_id)?
            .iter()
            .find(|(id, _)| id == msg_id)
            .map(|(_, j)| j.clone())
    }

    fn remember_sender(&mut self, chat_id: &str, msg_id: &str, sender: Jid) {
        let q = self.senders.entry(chat_id.to_string()).or_default();
        if q.len() >= SENDERS_PER_CHAT {
            q.pop_front();
        }
        q.push_back((msg_id.to_string(), sender));
    }

    /// Re-reads group names from WhatsApp (user pressed r). Groups only: direct chat names
    /// come from the phone's contact sync.
    async fn refresh_groups(&mut self) {
        let Some(session) = &self.session else { return };
        if !session.client.is_logged_in() {
            return;
        }
        let client = session.client.clone();
        let groups = match tokio::time::timeout(
            NETWORK_TIMEOUT,
            client.groups().get_participating(),
        )
        .await
        {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                self.notice(
                    &format!(
                        "Could not refresh WhatsApp groups ({}). Press r to retry.",
                        chain(&e)
                    ),
                    true,
                );
                return;
            }
            Err(_) => {
                self.notice(
                    "WhatsApp groups did not answer in time. Press r to retry.",
                    true,
                );
                return;
            }
        };
        let Ok(chats) = self.store.chats(NET) else {
            return;
        };
        let mut changed = Vec::new();
        for mut c in chats {
            let subject = groups
                .iter()
                .find(|(j, _)| j.to_string() == c.id)
                .map(|(_, m)| m.subject.clone());
            if let Some(s) = subject.filter(|s| !s.is_empty() && *s != c.title) {
                c.title = s;
                changed.push(c);
            }
        }
        if !changed.is_empty() {
            match self.store.upsert_chats(&changed) {
                Ok(()) => self.emit_chats(),
                Err(e) => self.store_failed("save group names", &e),
            }
        }
    }

    // -- session lifecycle ------------------------------------------------------------------

    /// `link` = the user asked to link (Command::Login): create a session when there is none.
    /// Without it, only an existing paired session is resumed.
    pub(crate) async fn start(&mut self, link: bool) {
        let path = match paths::whatsapp_db() {
            Ok(p) => p,
            Err(e) => return self.fail_status(e),
        };
        if self.clear_pending_delete().await {
            return self.fail_status(format!("{LINK_FILE_BUSY} Wait a minute, then press Enter."));
        }
        if !link && !path.exists() {
            return self.set_status(Status::NeedsLogin { reason: None });
        }
        self.gen += 1;
        let bot = match self.build_bot(&path).await {
            Ok(b) => b,
            Err(e) => {
                return self.fail_status(format!(
                    "WhatsApp session could not be opened ({e}). Delete whatsapp.db in the chatcli data folder and link again."
                ));
            }
        };
        let client = bot.client();
        let paired = client.pn().is_some();
        if !paired && !link {
            drop(bot);
            return self.set_status(Status::NeedsLogin { reason: None });
        }
        self.set_status(if paired {
            Status::Connecting
        } else {
            Status::LoggingIn
        });
        self.connect_by = paired.then(|| Instant::now() + CONNECT_WAIT);
        let handle = bot.spawn();

        self.attach(Arc::new(RealWa {
            client: client.clone(),
        }));
        log::info(
            AREA,
            if paired {
                "resuming linked session"
            } else {
                "starting new link (QR)"
            },
        );
        self.session = Some(Session { handle, client });
    }

    /// Connects the send side to a client (the library, or a fake in tests): the text queue, and
    /// who "me" is for mentions and quotes.
    pub(crate) fn attach(&mut self, wa: Arc<dyn Wa>) {
        let (jobs, mut job_rx) = mpsc::unbounded_channel::<Job>();
        let sender = wa.clone();
        let out = self.tx.clone();
        tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                // A send that waits on a dead connection must not hold every later message in
                // the queue as "sending" forever.
                let sent = tokio::time::timeout(
                    NETWORK_TIMEOUT,
                    sender.send(job.jid.clone(), job.message),
                );
                let result = match sent.await {
                    Ok(r) => r,
                    Err(_) => Err("WhatsApp did not answer in 15 s. Check the phone's connection, then send again.".to_string()),
                };
                let _ = out.send(Msg::Sent(Box::new(Sent {
                    chat_id: job.chat_id,
                    client_id: job.client_id,
                    draft: job.draft,
                    result,
                })));
            }
        });
        self.names.set_own(wa.own_users());
        self.link = Some(Link { wa, jobs });
    }

    /// Our own addresses are known only once pairing finished: learn them again then.
    fn refresh_identity(&mut self) {
        if let Some(link) = &self.link {
            self.names.set_own(link.wa.own_users());
        }
    }

    async fn build_bot(&self, path: &std::path::Path) -> Result<Bot, String> {
        let db = path.to_string_lossy().to_string();
        let backend = SqliteStore::new(&db).await.map_err(|e| chain(&e))?;
        let transport = TokioWebSocketTransportFactory::new().with_connector(tls::ws_connector()?);
        Bot::builder()
            .with_backend(backend)
            .with_transport_factory(transport)
            .with_http_client(tls::http_client())
            .with_event_handler(Forward {
                tx: self.tx.clone(),
                gen: self.gen,
            })
            .build()
            .await
            .map_err(|e| chain(&e))
    }

    fn fail_status(&mut self, message: String) {
        log::error(AREA, &message);
        self.set_status(Status::Error { message });
    }

    /// Disconnects cleanly and drops the session, so its database handle is released. Events
    /// from the old session that are still queued carry the old generation and are ignored.
    pub(crate) async fn stop_session(&mut self) {
        self.gen += 1;
        self.connect_by = None;
        self.drop_avatar_queue();
        self.avatar_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The queue ends once its pending sends are answered; the transfers on their own tasks
        // fail by themselves on the dead client and report it.
        self.link = None;
        let Some(s) = self.session.take() else { return };
        let Session { handle, client } = s;
        if tokio::time::timeout(STOP_TIMEOUT, handle.shutdown())
            .await
            .is_err()
        {
            log::error(AREA, "WhatsApp session did not stop in 10 s; dropped it");
        }
        drop(client);
    }

    async fn logout(&mut self) {
        let had_session = self.session.is_some();
        if let Some(s) = &self.session {
            // Deregisters this device on WhatsApp's side. Best effort: it needs a connection.
            let client = s.client.clone();
            self.gen += 1; // the LoggedOut event this raises is ours, not a surprise
            if tokio::time::timeout(STOP_TIMEOUT, client.logout())
                .await
                .is_err()
            {
                log::error(AREA, "logout request timed out");
            }
        }
        self.stop_session().await;
        if self.delete_session_files().await {
            self.notice(LINK_FILE_BUSY, true);
        }
        self.forget_contacts();
        self.set_status(Status::NeedsLogin { reason: None });
        if !had_session {
            self.notice("Local link deleted. If chatcli is still listed in WhatsApp > Linked devices, remove it there.", false);
        }
    }

    /// Unlinking removes the address-book names, phone numbers and id links saved for this
    /// network (they are personal data of the account that was linked), in the store and in memory.
    fn forget_contacts(&mut self) {
        match self.store.clear_names(NET) {
            Ok(n) => log::info(
                AREA,
                &format!("unlinked: deleted {n} saved contact names and links"),
            ),
            Err(e) => self.store_failed("delete contact names", &e),
        }
        let own = self.link.as_ref().map(|l| l.wa.own_users());
        self.names = Names::default();
        self.retitle_at = None;
        // Photos of the unlinked account's contacts are personal data too.
        self.media.clear_avatars();
        self.avatar_seen.clear();
        if let Some(own) = own {
            self.names.set_own(own);
        }
    }

    /// Removes the session database. Returns true when files are LEFT (still in use). The
    /// library's pairing timers can hold the store open for up to a minute after a stop, and
    /// Windows will not delete an open file; the leftover is remembered and cleared at the
    /// next start or link.
    async fn delete_session_files(&mut self) -> bool {
        let Ok(db) = paths::whatsapp_db() else {
            return false;
        };
        let left = remove_db_files(&db).await;
        let _ = self.store.set_meta(DELETE_PENDING_KEY, left.then_some("1"));
        left
    }

    /// A file left by an earlier unlink must go before a new session opens it. True = still there.
    async fn clear_pending_delete(&mut self) -> bool {
        if self.store.meta(DELETE_PENDING_KEY).ok().flatten().is_some() {
            self.delete_session_files().await
        } else {
            false
        }
    }

    // -- library events ---------------------------------------------------------------------

    pub(crate) async fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Sent(s) => {
                self.reap();
                self.on_sent(*s).await
            }
            Msg::Done(d) => {
                self.reap();
                self.on_done(d).await
            }
            Msg::Wa(gen, ev) if gen == self.gen => self.on_event(&ev).await,
            Msg::Wa(..) => {}
        }
    }

    async fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::PairingQrCode(qr) => self.set_status(Status::Qr {
                code: qr.code.clone(),
            }),
            Event::QrScannedWithoutMultidevice(_) => {
                self.notice("That phone has an old WhatsApp. Update WhatsApp on the phone, then link again.", true);
            }
            Event::PairSuccess(_) => {
                log::info(AREA, "paired");
                self.refresh_identity();
                // A new link has no resume deadline yet: without one a failed first connect
                // would show CONNECTING for ever.
                self.connect_by = Some(Instant::now() + CONNECT_WAIT);
                self.set_status(Status::Connecting);
            }
            Event::PairError(e) => {
                log::error(AREA, &format!("pairing failed: {}", e.error));
                self.unlinked("WhatsApp pairing failed. Press Enter to try again.", true)
                    .await;
            }
            Event::PairingQrCodesExhausted(_) => {
                // The library would reconnect and ask for new QRs forever: stop and wait.
                self.unlinked("QR code expired. Press Enter for a new one.", true)
                    .await;
            }
            Event::Connected(_) => {
                log::info(AREA, "connected");
                self.connect_by = None;
                self.refresh_identity();
                self.bind_account();
                self.set_status(Status::Online {
                    live: true,
                    detail: None,
                });
                self.emit_caps();
            }
            Event::Disconnected(d) => {
                log::info(
                    AREA,
                    &format!("disconnected ({}); the library is reconnecting", d.reason),
                );
                // Only a link that was up becomes NOT LIVE; pairing and first connects keep their state.
                if matches!(self.last_status, Status::Online { .. }) {
                    self.set_status(Status::Online {
                        live: false,
                        detail: Some("reconnecting".to_string()),
                    });
                }
            }
            Event::LoggedOut(l) => {
                let reason = unlinked_reason(&l.reason);
                self.unlinked(&reason, true).await;
                self.forget_contacts();
            }
            Event::StreamReplaced(_) => {
                self.unlinked(
                    "Another WhatsApp Web session replaced this one. Press Enter to reconnect.",
                    false,
                )
                .await;
            }
            Event::TemporaryBan(b) => {
                let msg = format!(
                    "WhatsApp banned this account for about {} min ({}). Stop using chatcli until it ends.",
                    b.expire.num_minutes(),
                    b.code
                );
                self.stopped_with_error(msg).await;
            }
            Event::ClientOutdated(_) => {
                self.stopped_with_error(
                    "WhatsApp rejected this chatcli version as outdated. Update chatcli."
                        .to_string(),
                )
                .await;
            }
            Event::ConnectFailure(f) => {
                // Logged-out reasons also raise LoggedOut and retryable ones are retried by
                // the library; anything else ends the connection for good.
                if !f.reason.should_reconnect() && !f.reason.is_logged_out() {
                    let msg = format!(
                        "WhatsApp refused the connection ({:?}). See chatcli.log, then press Enter to retry.",
                        f.reason
                    );
                    self.stopped_with_error(msg).await;
                }
            }
            Event::Messages(batch) => {
                for m in batch.iter() {
                    self.on_message(m).await;
                }
            }
            Event::UndecryptableMessage(_) => {
                self.undecryptable += 1;
                log::error(
                    AREA,
                    &format!(
                        "could not decrypt a message ({} so far); the phone is asked to resend",
                        self.undecryptable
                    ),
                );
            }
            Event::AppStateSyncFailed(a) => {
                let (text, is_error) = map::app_state_report(&a.fatal, &a.retryable, &a.skipped);
                if is_error {
                    log::error(AREA, &text);
                } else {
                    log::info(AREA, &text);
                }
            }
            Event::PushNameUpdate(p) => {
                let mut changed = self.names.set_push(p.jid.user.as_str(), &p.new_push_name);
                let src = &p.message.source;
                changed |= self.link_jids(&src.sender, src.sender_alt.as_ref());
                if changed {
                    self.schedule_retitle();
                }
            }
            Event::ContactUpdate(c) => {
                let a = &c.action;
                let name = [a.full_name.as_deref(), a.first_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .find(|n| !n.is_empty());
                let (users, link) =
                    map::contact_addresses(&c.jid, a.lid_jid.as_deref(), a.pn_jid.as_deref());
                let mut changed = false;
                if let Some((pn, lid)) = link {
                    changed |= self.names.link(&pn, &lid);
                }
                if let Some(name) = name {
                    for u in users {
                        changed |= self.names.set_book(&u, name);
                    }
                }
                if changed {
                    self.schedule_retitle();
                }
            }
            Event::GroupUpdate(g) => {
                if let GroupNotificationAction::Subject { subject, .. } = &g.action {
                    self.set_group_title(&g.group_jid.to_string(), subject);
                }
            }
            Event::MarkChatAsReadUpdate(m) => {
                if m.action.read == Some(true) {
                    let mut chat = map::canonical_chat(&m.jid, None);
                    if chat.is_lid() {
                        if let Some(pn) = self.pn_for(&chat).await {
                            self.adopt_pn_chat(&chat, &pn);
                            chat = pn;
                        }
                    }
                    let id = chat.to_string();
                    if let Ok(Some(mut c)) = self.store.chat(NET, &id) {
                        if c.unread > 0 {
                            c.unread = 0;
                            let _ = self.store.set_unread(NET, &id, 0);
                            self.emit(Out::ChatUpdated(c));
                        }
                    }
                }
            }
            Event::HistorySync(_) => self.on_history(ev).await,
            Event::ChatPresence(p) => self.on_chat_presence(p),
            Event::Receipt(r) => self.on_receipt(r),
            Event::PinUpdate(u) => self.on_pin_update(&u.jid, u.action.pinned == Some(true)),
            Event::MuteUpdate(u) => {
                let a = &u.action;
                let until = map::mute_until(a.muted == Some(true), a.mute_end_timestamp, now_ms());
                self.on_mute_update(&u.jid, until);
            }
            Event::ArchiveUpdate(u) => {
                self.on_archive_update(&u.jid, u.action.archived == Some(true));
            }
            _ => {}
        }
    }

    /// The link ended or must be redone: stop the session, show NEEDS LOGIN with `reason`.
    /// `forget` deletes the local keys too (they are dead); a replaced session keeps them.
    async fn unlinked(&mut self, reason: &str, forget: bool) {
        self.stop_session().await;
        if forget && self.delete_session_files().await {
            self.notice(LINK_FILE_BUSY, true);
        }
        self.notice(reason, true);
        self.set_status(Status::NeedsLogin {
            reason: Some(reason.to_string()),
        });
    }

    async fn stopped_with_error(&mut self, message: String) {
        self.stop_session().await;
        self.notice(&message, true);
        self.set_status(Status::Error { message });
    }

    async fn on_history(&mut self, ev: &Event) {
        let Event::HistorySync(lazy) = ev else { return };
        let (kind, progress) = (lazy.sync_type(), lazy.progress());
        let session_id = lazy.peer_data_request_session_id().map(str::to_string);
        // Decoding and writing a chunk is heavy: off the async threads.
        let store = self.store.clone();
        let names = self.names.clone();
        let lazy = lazy.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let hs = lazy
                .get()
                .ok_or_else(|| "history chunk could not be decoded".to_string())?;
            apply_history(&store, &names, hs)
        })
        .await;
        match outcome {
            Ok(Ok(o)) => {
                log::info(
                    AREA,
                    &format!(
                        "history sync type={kind} progress={progress:?}: {} chats, {} messages, {} skipped, {} push names, {} contacts, {} address links, {} future-dated messages re-timed",
                        o.chats,
                        o.messages,
                        o.skipped,
                        o.push_names.len(),
                        o.book_names.len(),
                        o.links.len(),
                        o.future_fixed
                    ),
                );
                let mut changed = false;
                for (user, name) in &o.push_names {
                    changed |= self.names.set_push(user, name);
                }
                for (user, name) in &o.book_names {
                    changed |= self.names.set_book(user, name);
                }
                for (a, b) in &o.links {
                    changed |= self.names.link(a, b);
                }
                if changed {
                    self.schedule_retitle();
                }
                self.emit_chats();
                self.after_history(&o, kind == ON_DEMAND_SYNC, session_id.as_deref());
            }
            Ok(Err(e)) => self.notice(
                &format!("WhatsApp history could not be read ({e}). See chatcli.log."),
                true,
            ),
            Err(e) => self.notice(
                &format!("WhatsApp history task failed ({e}). See chatcli.log."),
                true,
            ),
        }
    }

    // -- chats and messages -----------------------------------------------------------------

    fn schedule_retitle(&mut self) {
        self.retitle_at = Some(Instant::now() + RETITLE_DELAY);
    }

    /// Saves names learned since the last save. WhatsApp sends address-book names once per
    /// device, so a name that is not saved here is gone after a restart.
    pub(crate) fn flush_names(&mut self) -> usize {
        let rows = self.names.take_pending();
        if rows.is_empty() {
            return 0;
        }
        if let Err(e) = self.store.upsert_names(NET, &rows) {
            self.store_failed("save contact names", &e);
            return 0;
        }
        rows.len()
    }

    /// Puts back the names saved by earlier runs.
    pub(crate) fn load_names(&mut self) {
        match self.store.names(NET) {
            Ok(rows) => {
                let n = rows.len();
                self.names.load(rows);
                log::info(AREA, &format!("loaded {n} saved contact names and links"));
            }
            Err(e) => self.store_failed("read contact names", &e),
        }
    }

    /// Applies names learned since the chats were last written. Never replaces a real name with
    /// a worse one: a chat whose title is a bare number or "Unknown contact" is the only one a
    /// push name, a sibling chat or a stored sender label may rename.
    pub(crate) fn retitle(&mut self) {
        let saved = self.flush_names();
        let Ok(chats) = self.store.chats(NET) else {
            return;
        };
        let mut changed = Vec::new();
        for mut c in chats {
            let Ok(jid) = c.id.parse::<Jid>() else {
                continue;
            };
            let better = self
                .names
                .better_title(&jid, &c.title)
                .or_else(|| self.stored_name(&jid, &c));
            if let Some(t) = better {
                c.title = t;
                changed.push(c);
            }
        }
        if saved > 0 || !changed.is_empty() {
            log::info(
                AREA,
                &format!(
                    "contact names: {saved} saved, {} chat titles improved",
                    changed.len()
                ),
            );
        }
        if changed.is_empty() {
            return;
        }
        match self.store.upsert_chats(&changed) {
            Ok(()) => self.emit_chats(),
            Err(e) => self.store_failed("save names", &e),
        }
    }

    /// A name for a direct chat that still has a number or "Unknown contact" as title, from what
    /// is already stored: the chat under the person's other address, else the label their own
    /// messages carry.
    fn stored_name(&self, jid: &Jid, chat: &ChatSummary) -> Option<String> {
        if jid.is_group() || !map::is_fallback_title(&chat.title) {
            return None;
        }
        if let Some(other) = self.names.linked(&jid.user) {
            let other = if jid.is_lid() {
                Jid::pn(other)
            } else {
                Jid::lid(other)
            };
            let sibling = self.store.chat(NET, &other.to_string()).ok().flatten();
            if let Some(s) = sibling.filter(|s| !map::is_fallback_title(&s.title)) {
                return Some(s.title);
            }
        }
        let counts = self.store.sender_counts(NET, &chat.id).ok()?;
        map::name_from_senders(&counts)
    }

    /// Moves messages dated in the future to the chat's newest believable time and recomputes
    /// the sidebar time and preview of every chat that was ahead. Idempotent, deletes nothing,
    /// logs counts only. See `Store::fix_future_messages` for why a stored row can only be
    /// approximated.
    pub(crate) fn repair_future_dates(&mut self) {
        let now = now_ms();
        let moved = match self.store.fix_future_messages(
            NET,
            now,
            map::STORED_REPAIR_SKEW_MS,
            map::EARLIEST_SANE_MS,
        ) {
            Ok(m) => m,
            Err(e) => return self.store_failed("repair future-dated messages", &e),
        };
        let Ok(chats) = self.store.chats(NET) else {
            return;
        };
        let mut changed = Vec::new();
        for mut c in chats {
            let ahead = c.last_ts.is_some_and(|t| map::is_future(t, now));
            if !ahead && !moved.iter().any(|(id, _)| *id == c.id) {
                continue;
            }
            let (ts, preview) = match newest_of(&self.store, &c.id) {
                Ok(t) => t,
                Err(e) => return self.store_failed("read newest message", &e),
            };
            if c.last_ts != ts || c.last_preview != preview {
                c.last_ts = ts;
                c.last_preview = preview;
                changed.push(c);
            }
        }
        if moved.is_empty() && changed.is_empty() {
            return;
        }
        log::info(
            AREA,
            &format!(
                "repaired {} future-dated messages in {} chats; {} chat times recomputed",
                moved.iter().map(|(_, n)| n).sum::<usize>(),
                moved.len(),
                changed.len()
            ),
        );
        if let Err(e) = self.store.upsert_chats(&changed) {
            self.store_failed("save repaired chat times", &e);
        }
    }

    /// Two addresses of one person (phone number and LID) go into the name table, which saves
    /// them. Returns true when this was news.
    fn link_jids(&mut self, a: &Jid, b: Option<&Jid>) -> bool {
        match b {
            Some(b) if (a.is_lid() && b.is_pn()) || (a.is_pn() && b.is_lid()) => {
                let (pn, lid) = if a.is_pn() { (a, b) } else { (b, a) };
                self.names.link(&pn.user, &lid.user)
            }
            _ => false,
        }
    }

    /// After a start: asks the library (which keeps its own saved LID table in whatsapp.db) for
    /// the phone number behind every stored LID chat, so those chats can be named and merged
    /// with their phone-number twin.
    pub(crate) async fn learn_lid_links(&mut self) {
        let Ok(chats) = self.store.chats(NET) else {
            return;
        };
        let mut found = 0;
        for c in chats {
            let Ok(jid) = c.id.parse::<Jid>() else {
                continue;
            };
            if !jid.is_lid() || self.names.linked(&jid.user).is_some() {
                continue;
            }
            if let Some(pn) = self.pn_for(&jid).await {
                found += usize::from(self.names.link(&pn.user, &jid.user));
            }
        }
        if found > 0 {
            log::info(AREA, &format!("learned {found} LID to phone number links"));
            self.schedule_retitle();
        }
        // Links from earlier runs count too: a LID chat that still has a known phone number is
        // the same person's second row.
        match merge_linked_chats(&self.store, &self.names) {
            Ok(moved) if !moved.is_empty() => {
                log::info(
                    AREA,
                    &format!(
                        "merged {} LID chats into their phone-number chats",
                        moved.len()
                    ),
                );
                self.emit_chats();
            }
            Ok(_) => {}
            Err(e) => self.store_failed("merge linked chats", &e),
        }
    }

    fn set_group_title(&mut self, chat_id: &str, subject: &str) {
        if let Ok(Some(mut c)) = self.store.chat(NET, chat_id) {
            if c.title != subject {
                c.title = subject.to_string();
                if self.store.upsert_chats(std::slice::from_ref(&c)).is_ok() {
                    self.emit(Out::ChatUpdated(c));
                }
            }
        }
    }

    /// The other address of a LID peer (its phone number), when the library has learned it.
    /// The library's table first (it learns pairs from the stanzas it handles), then our own
    /// saved links (address book, history sync).
    async fn pn_for(&self, lid: &Jid) -> Option<Jid> {
        if let Some(session) = &self.session {
            if let Ok(Some(entry)) = session.client.get_lid_pn_entry(lid).await {
                return Some(Jid::pn(&*entry.phone_number));
            }
        }
        self.names.linked(&lid.user).map(Jid::pn)
    }

    /// Folds a stored LID chat into the phone-number chat of the same person.
    fn adopt_pn_chat(&mut self, lid: &Jid, pn: &Jid) {
        let Ok(Some(from)) = self.store.chat(NET, &lid.to_string()) else {
            return;
        };
        match fold_chat(&self.store, &from, &pn.to_string()) {
            Ok(true) => self.emit_chats(),
            Ok(false) => {}
            Err(e) => self.store_failed("merge chat", &e),
        }
    }

    pub(crate) async fn on_message(&mut self, m: &InboundMessage) {
        let info = &*m.info;
        let src = &info.source;
        let raw_chat = src.chat.to_non_ad();
        if !map::is_supported_chat(&raw_chat) {
            return;
        }
        let alt = if src.is_from_me {
            src.recipient_alt.as_ref()
        } else {
            src.sender_alt.as_ref()
        };
        let mut changed = self.link_jids(&src.sender, src.sender_alt.as_ref());
        changed |= self.link_jids(&raw_chat, src.recipient_alt.as_ref());
        let mut chat = map::canonical_chat(&raw_chat, alt);
        // The first message of a conversation can arrive with only the hidden (LID) address; a
        // later one names the phone number. What was stored under the LID id moves to the
        // phone-number id now, or the sidebar would hold the same person twice.
        if raw_chat.is_lid() && chat.is_pn() {
            self.adopt_pn_chat(&raw_chat, &chat);
        }
        if chat.is_lid() {
            if let Some(pn) = self.pn_for(&chat).await {
                changed |= self.names.link(&pn.user, &chat.user);
                // The conversation moves to the phone-number id only together with what is
                // already stored under the LID id, never leaving a second row behind.
                self.adopt_pn_chat(&chat, &pn);
                chat = pn;
            }
        }
        let chat_id = chat.to_string();
        let push = info.push_name.as_str();
        if !src.is_from_me {
            // Under the sender's address and, in a direct chat, under the chat's (they can differ
            // when the peer is addressed by LID).
            changed |= self.names.set_push(src.sender.user.as_str(), push);
            if !chat.is_group() {
                changed |= self.names.set_push(chat.user.as_str(), push);
            }
        }
        if changed {
            self.schedule_retitle();
        }
        match map::content(&m.message) {
            Content::Skip => {}
            Content::Revoke { target } => {
                if self.authored_by(
                    &chat_id,
                    &target,
                    src.is_from_me,
                    &src.sender,
                    chat.is_group(),
                ) {
                    self.apply_deleted(&chat_id, &target);
                }
            }
            Content::Edit {
                target,
                text,
                mentioned,
            } => {
                if self.authored_by(
                    &chat_id,
                    &target,
                    src.is_from_me,
                    &src.sender,
                    chat.is_group(),
                ) {
                    self.apply_edited(&chat_id, &target, &text, &mentioned);
                }
            }
            Content::Reaction { target, emoji } => {
                let reactor = rich::reactor_id(&src.sender, src.is_from_me, &self.names);
                let ts = map::live_ts(info.timestamp.timestamp_millis(), now_ms());
                self.apply_reaction(&chat_id, &target, &reactor, src.is_from_me, &emoji, ts);
            }
            Content::Text(text) => {
                if chat.is_group() && !src.is_from_me {
                    self.remember_sender(&chat_id, &info.id, src.sender.to_non_ad());
                }
                let now = now_ms();
                let ts = map::live_ts(info.timestamp.timestamp_millis(), now);
                if ts != info.timestamp.timestamp_millis() {
                    log::info(
                        AREA,
                        "a live message was dated in the future; used the current time",
                    );
                }
                let sender_id = src.sender.to_non_ad().to_string();
                // Someone who was shown as typing has sent their message: the indicator ends.
                let sender = map::sender_label(src.is_from_me, &src.sender, push, &self.names);
                self.typing_ended(&chat_id, &sender);
                let d = rich::details(&m.message, &chat, &self.names);
                let (text, mentions) = rich::normalise_mentions(&text, &d.mentioned, &self.names);
                let reply_to = d.reply_to.map(|q| self.fill_preview(&chat_id, q));
                let msg = ChatMessage {
                    network: NET,
                    chat_id,
                    id: info.id.clone(),
                    ts,
                    sender,
                    from_me: src.is_from_me,
                    text,
                    state: MessageState::Sent,
                    sender_id,
                    reply_to,
                    attachments: d.attachments,
                    mentions_me: !src.is_from_me && rich::mentions_me(&d.mentioned, &self.names),
                    mentions,
                    ..ChatMessage::blank(NET)
                };
                self.store_message(&chat, msg, push).await;
            }
        }
    }

    /// A quote that arrived without a copy of the quoted text gets one from the store when the
    /// quoted message is stored; otherwise the preview stays empty and the TUI shows a placeholder.
    fn fill_preview(&self, chat_id: &str, mut q: QuotedRef) -> QuotedRef {
        if q.preview.is_empty() {
            if let Ok(Some(orig)) = self.store.message(NET, chat_id, &q.id) {
                q.preview = rich::one_line(&orig.text, rich::QUOTE_PREVIEW_CHARS);
            }
        }
        q
    }

    async fn store_message(&mut self, chat: &Jid, msg: ChatMessage, push: &str) {
        // A redelivered message (offline queue after a reconnect) is not new: it must not count
        // as unread twice, toast again, or put back text the user already saw edited or deleted.
        // An indexed lookup by id: a message older than the newest page is found too.
        let stored = match self.store.message(NET, &msg.chat_id, &msg.id) {
            Ok(m) => m,
            Err(e) => return self.store_failed("read message", &e),
        };
        let seen = stored.is_some();
        if stored.is_some_and(|old| old.edited || old.state == MessageState::Deleted) {
            return;
        }
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&msg)) {
            return self.store_failed("save message", &e);
        }
        // `Message` is for a message that has just arrived; a redelivery only refreshes a row.
        if seen {
            self.emit(Out::MessageChanged(msg.clone()));
        } else {
            self.emit(Out::Message(msg.clone()));
            if msg.from_me {
                self.own_message_stored(&msg.chat_id, msg.ts);
            }
        }
        self.update_chat(chat, &msg, !msg.from_me && !seen, push)
            .await;
    }

    /// Moves the chat's last message forward (never back) and emits the new sidebar row.
    async fn update_chat(&mut self, chat: &Jid, msg: &ChatMessage, count_unread: bool, push: &str) {
        let chat_id = chat.to_string();
        let existing = self.store.chat(NET, &chat_id).unwrap_or_else(|e| {
            log::error(AREA, &format!("read chat: {e}"));
            None
        });
        let mut c = match existing {
            Some(c) => c,
            None => ChatSummary {
                network: NET,
                id: chat_id.clone(),
                title: self.new_chat_title(chat, push).await,
                kind: map::chat_kind(chat),
                last_ts: None,
                last_preview: String::new(),
                unread: 0,
                ..ChatSummary::blank(NET)
            },
        };
        // A stored time ahead of the clock is bad data (the start-up repair fixes those); it must
        // not keep every newer message from becoming the chat's last one.
        if c.last_ts
            .is_none_or(|t| msg.ts >= t || map::is_future(t, now_ms()))
        {
            c.last_ts = Some(msg.ts);
            c.last_preview = map::preview(&msg.sender, &msg.text, chat.is_group() || msg.from_me);
        }
        if count_unread {
            c.unread += 1;
        }
        match self.store.upsert_chats(std::slice::from_ref(&c)) {
            Ok(()) => {
                // The mention counter is the store's: it only goes up here, and only for a new
                // unread message that mentions us.
                if count_unread && msg.mentions_me {
                    match self.store.bump_mentions(NET, &chat_id) {
                        Ok(()) => c.mentions += 1,
                        Err(e) => log::error(AREA, &format!("count mention: {e}")),
                    }
                }
                self.emit(Out::ChatUpdated(c));
            }
            Err(e) => self.store_failed("save chat", &e),
        }
    }

    /// Title for a chat first seen live (not in the synced list): the group's subject from
    /// WhatsApp, or the contact's name.
    async fn new_chat_title(&self, chat: &Jid, push: &str) -> String {
        if !chat.is_group() {
            return self.names.label_with(chat, Some(push));
        }
        let Some(session) = &self.session else {
            return "Group".to_string();
        };
        let client = session.client.clone();
        match tokio::time::timeout(NETWORK_TIMEOUT, client.groups().get_metadata(chat)).await {
            Ok(Ok(meta)) if !meta.subject.is_empty() => meta.subject,
            Ok(Ok(_)) => "Group".to_string(),
            Ok(Err(e)) => {
                log::error(AREA, &format!("group name lookup failed: {}", chain(&e)));
                "Group".to_string()
            }
            Err(_) => "Group".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------------------------

/// Deletes `db` and its SQLite side files, retrying while something still holds them open.
/// Returns true when any file is left.
async fn remove_db_files(db: &std::path::Path) -> bool {
    let mut left = false;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        let file = std::path::PathBuf::from(name);
        let mut tries = 0;
        loop {
            match std::fs::remove_file(&file) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) if tries < DELETE_TRIES => {
                    tries += 1;
                    tokio::time::sleep(DELETE_STEP).await;
                }
                Err(e) => {
                    log::error(AREA, &format!("cannot delete {}: {e}", file.display()));
                    left = true;
                    break;
                }
            }
        }
    }
    left
}

#[derive(Debug)]
pub(crate) struct HistoryOutcome {
    pub chats: usize,
    /// Ids of every chat the chunk carried messages or a chat row for.
    pub touched: Vec<String>,
    pub messages: usize,
    pub skipped: usize,
    pub push_names: Vec<(String, String)>,
    pub book_names: Vec<(String, String)>,
    pub links: Vec<(String, String)>,
    pub future_fixed: usize,
}

/// The newest stored message's time and sidebar preview; `(None, "")` for a chat with none.
pub(crate) fn newest_of(
    store: &Store,
    chat_id: &str,
) -> chatcli_core::Result<(Option<i64>, String)> {
    let group = chat_id.parse::<Jid>().is_ok_and(|j| j.is_group());
    Ok(match store.messages(NET, chat_id, 1)?.pop() {
        Some(m) => (
            Some(m.ts),
            map::preview(&m.sender, map::preview_text(&m), group || m.from_me),
        ),
        None => (None, String::new()),
    })
}

/// Folds every stored LID chat whose phone number is known into the phone-number chat: one
/// person, one sidebar row. Returns the ids moved, as `(from, to)`.
pub(crate) fn merge_linked_chats(
    store: &Store,
    names: &Names,
) -> chatcli_core::Result<Vec<(String, String)>> {
    let mut moved = Vec::new();
    for c in store.chats(NET)? {
        let Some(user) = c.id.strip_suffix("@lid") else {
            continue;
        };
        let Some(pn) = names.linked(user) else {
            continue;
        };
        let to = Jid::pn(pn).to_string();
        if fold_chat(store, &c, &to)? {
            moved.push((c.id, to));
        }
    }
    Ok(moved)
}

/// Folds `from` into chat `to` (`Store::rename_chat`), then keeps the better title of the two:
/// the target's own, unless it is only a number or "Unknown contact" and the source had a name.
pub(crate) fn fold_chat(store: &Store, from: &ChatSummary, to: &str) -> chatcli_core::Result<bool> {
    if !store.rename_chat(NET, &from.id, to)? {
        return Ok(false);
    }
    if let Some(mut kept) = store.chat(NET, to)? {
        if map::is_fallback_title(&kept.title) && !map::is_fallback_title(&from.title) {
            kept.title = from.title.clone();
            store.upsert_chats(&[kept])?;
        }
    }
    Ok(true)
}

/// Decodes nothing itself: maps one history chunk and writes it to the store in batches.
pub(crate) fn apply_history(
    store: &Store,
    names: &Names,
    hs: &wa::HistorySync,
) -> chatcli_core::Result<HistoryOutcome> {
    let batch = map::map_history(hs, names, now_ms());
    // Links this chunk carries make a stored LID chat and its phone-number twin one chat; do
    // that before writing, so the chunk's messages land in the surviving chat, not a second one.
    let mut linked = names.clone();
    for (a, b) in &batch.links {
        linked.link(a, b);
    }
    merge_linked_chats(store, &linked)?;
    let existing: HashMap<String, ChatSummary> = store
        .chats(NET)?
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    // A history chunk is a snapshot of the past: a message the user already saw edited or
    // deleted must not be put back to its first text. Rows with no such change are rewritten (the
    // snapshot may know more, such as the sender). One indexed lookup per message.
    let mut fresh: Vec<ChatMessage> = Vec::with_capacity(batch.messages.len());
    for m in &batch.messages {
        let amended = store
            .message(NET, &m.chat_id, &m.id)?
            .is_some_and(|old| old.edited || old.state == MessageState::Deleted);
        if !amended {
            fresh.push(m.clone());
        }
    }
    for chunk in fresh.chunks(1000) {
        store.upsert_messages(chunk)?;
    }
    let chats: Vec<ChatSummary> = batch
        .chats
        .into_iter()
        .map(|c| {
            let old = existing.get(&c.summary.id);
            map::merge_chat(c, old)
        })
        .collect();
    let mut chats = chats;
    // merge_chat keeps the older of two sidebar times, which would pin a start-up approximation
    // above the faithful message times just written. The newest stored message is the truth.
    for c in &mut chats {
        if let (Some(ts), preview) = newest_of(store, &c.id)? {
            c.last_ts = Some(ts);
            c.last_preview = preview;
        }
    }
    store.upsert_chats(&chats)?;
    let mut touched: Vec<String> = batch.messages.iter().map(|m| m.chat_id.clone()).collect();
    touched.extend(chats.iter().map(|c| c.id.clone()));
    touched.sort_unstable();
    touched.dedup();
    Ok(HistoryOutcome {
        chats: chats.len(),
        touched,
        messages: batch.messages.len(),
        skipped: batch.skipped,
        push_names: batch.push_names,
        book_names: batch.book_names,
        links: batch.links,
        future_fixed: batch.future_fixed,
    })
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ops_tests.rs"]
mod ops_tests;

/// One sentence for the sign-in panel when WhatsApp ended the link.
pub(crate) fn unlinked_reason(reason: &ConnectFailureReason) -> String {
    match reason {
        ConnectFailureReason::AccountLocked => {
            "WhatsApp locked this account. Open WhatsApp on the phone, then press Enter to link again.".to_string()
        }
        _ => "WhatsApp unlinked this device. Press Enter to link again.".to_string(),
    }
}
