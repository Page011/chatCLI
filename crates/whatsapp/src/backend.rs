//! The WhatsApp backend task: one tokio task that owns the whatsapp-rust session, the TUI's
//! commands and the store. Everything that touches state runs on this one task (library events
//! are forwarded into its channel), so there are no locks and no races between handlers.
//!
//! Failures are never silent: every way the link can end (unlinked, replaced, banned, outdated, QR
//! expired) is a Status the TUI shows plus a log line, and the library's reconnect loop is
//! stopped so nothing hammers WhatsApp in the background.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chatcli_core::model::{
    now_ms, ChatMessage, ChatSummary, Command, CommandRx, Event as Out, EventTx, MessageState,
    Network, Status,
};
use chatcli_core::store::Store;
use chatcli_core::{chain, log, paths};
use tokio::sync::mpsc;
use tokio::time::Instant;
use whatsapp_rust::prelude::*;
use whatsapp_rust::transport::TokioWebSocketTransportFactory;
use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;
use whatsapp_rust::wacore::types::events::ConnectFailureReason;
use whatsapp_rust::wacore_binary::JidExt;

use crate::map::{self, Content, Names};
use crate::tls;

const AREA: &str = "whatsapp";
const NET: Network = Network::WhatsApp;
/// WhatsApp has no server-side history fetch, so a chat opens with what the store holds.
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
const LINK_FILE_BUSY: &str = "The old WhatsApp link file is still in use, so it is deleted later.";
/// Deleting the session file is retried this often, this many times (5 s in all).
const DELETE_TRIES: u32 = 25;
const DELETE_STEP: Duration = Duration::from_millis(200);

/// Runs until `Command::Shutdown` or the command channel closes.
pub async fn run(store: Store, mut commands: CommandRx, events: EventTx) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut b = Backend::new(store, events, tx);
    b.set_status(Status::Starting);
    b.clear_pending_delete().await;
    b.emit_chats();
    b.start(false).await;
    loop {
        let retitle_at = b.retitle_at;
        let retitle = async {
            match retitle_at {
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
            _ = connect_wait => {
                b.connect_by = None;
                b.connect_timeout();
            }
        }
    }
    b.stop_session().await;
}

/// Everything that reaches the task besides TUI commands.
pub(crate) enum Msg {
    /// A library event, tagged with the session generation that produced it.
    Wa(u64, Arc<Event>),
    Sent(Sent),
}

pub(crate) struct Sent {
    chat_id: String,
    client_id: String,
    text: String,
    result: Result<String, String>,
}

struct Job {
    jid: Jid,
    chat_id: String,
    text: String,
    client_id: String,
}

struct Session {
    handle: BotHandle,
    client: Arc<Client>,
    /// Sends run one at a time on their own task, so messages keep their order and a slow send
    /// (first message to a big group) never blocks incoming events.
    jobs: mpsc::UnboundedSender<Job>,
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
            EventKind::QrScannedWithoutMultidevice,
            EventKind::ClientOutdated,
            EventKind::Messages,
            EventKind::UndecryptableMessage,
            EventKind::PushNameUpdate,
            EventKind::ContactUpdate,
            EventKind::GroupUpdate,
            EventKind::HistorySync,
            EventKind::StreamReplaced,
            EventKind::TemporaryBan,
            EventKind::ConnectFailure,
            EventKind::MarkChatAsReadUpdate,
            EventKind::AppStateSyncFailed,
        ])
    }
}

pub(crate) struct Backend {
    store: Store,
    events: EventTx,
    tx: mpsc::UnboundedSender<Msg>,
    session: Option<Session>,
    /// Bumped whenever a session starts or stops; events from older sessions are dropped.
    gen: u64,
    last_status: Status,
    names: Names,
    senders: HashMap<String, VecDeque<(String, Jid)>>,
    pub(crate) retitle_at: Option<Instant>,
    /// When a connection attempt counts as stuck; None while connected or not trying.
    pub(crate) connect_by: Option<Instant>,
    undecryptable: u64,
}

impl Backend {
    pub(crate) fn new(store: Store, events: EventTx, tx: mpsc::UnboundedSender<Msg>) -> Backend {
        Backend {
            store,
            events,
            tx,
            session: None,
            gen: 0,
            last_status: Status::Starting,
            names: Names::default(),
            senders: HashMap::new(),
            retitle_at: None,
            connect_by: None,
            undecryptable: 0,
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
            Command::OpenChat { chat_id } => match self.store.messages(NET, &chat_id, OPEN_LIMIT) {
                Ok(messages) => self.emit(Out::Messages {
                    network: NET,
                    chat_id,
                    messages,
                }),
                Err(e) => self.store_failed("read messages", &e),
            },
            Command::Send {
                chat_id,
                text,
                client_id,
            } => self.send(chat_id, text, client_id),
            Command::MarkRead { chat_id } => self.mark_read(&chat_id),
            Command::Shutdown => {}
        }
    }

    /// Reports a failed send and keeps the message as NOT SENT in the store, so it is still
    /// there after the chat is reopened.
    fn send_failed(&self, chat_id: String, client_id: String, text: String, error: String) {
        let failed = ChatMessage {
            network: NET,
            chat_id: chat_id.clone(),
            id: client_id.clone(),
            ts: now_ms(),
            sender: map::OWN_LABEL.to_string(),
            from_me: true,
            text,
            state: MessageState::Failed(error.clone()),
        };
        if let Err(e) = self.store.upsert_messages(&[failed]) {
            self.store_failed("save unsent message", &e);
        }
        self.emit(Out::MessageFailed {
            network: NET,
            chat_id,
            client_id,
            error,
        });
    }

    fn send(&mut self, chat_id: String, text: String, client_id: String) {
        const NOT_CONNECTED: &str =
            "WhatsApp is not connected. Wait for LIVE or press Enter to link.";
        let Some(session) = &self.session else {
            return self.send_failed(chat_id, client_id, text, NOT_CONNECTED.into());
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            let error = "This chat has no valid WhatsApp address. Press r to refresh.".to_string();
            return self.send_failed(chat_id, client_id, text, error);
        };
        let job = Job {
            jid,
            chat_id: chat_id.clone(),
            text: text.clone(),
            client_id: client_id.clone(),
        };
        if session.jobs.send(job).is_err() {
            self.send_failed(chat_id, client_id, text, NOT_CONNECTED.into());
        }
    }

    async fn on_sent(&mut self, s: Sent) {
        match s.result {
            Ok(id) => {
                let msg = ChatMessage {
                    network: NET,
                    chat_id: s.chat_id.clone(),
                    id,
                    ts: now_ms(),
                    sender: map::OWN_LABEL.to_string(),
                    from_me: true,
                    text: s.text,
                    state: MessageState::Sent,
                };
                if let Err(e) = self.store.confirm_message(&s.client_id, &msg) {
                    self.store_failed("save sent message", &e);
                }
                self.emit(Out::MessageConfirmed {
                    client_id: s.client_id,
                    message: msg.clone(),
                });
                if let Ok(jid) = s.chat_id.parse::<Jid>() {
                    self.update_chat(&jid, &msg, false, "").await;
                }
            }
            Err(reason) => {
                log::error(AREA, &format!("send failed: {reason}"));
                self.send_failed(s.chat_id, s.client_id, s.text, reason);
            }
        }
    }

    fn mark_read(&mut self, chat_id: &str) {
        let chat = match self.store.chat(NET, chat_id) {
            Ok(c) => c,
            Err(e) => return self.store_failed("read chat", &e),
        };
        let unread = chat.as_ref().map_or(0, |c| c.unread);
        if let Err(e) = self.store.set_unread(NET, chat_id, 0) {
            return self.store_failed("clear unread", &e);
        }
        if let Some(mut c) = chat {
            c.unread = 0;
            self.emit(Out::ChatUpdated(c));
        }
        if unread == 0 {
            return;
        }
        // Read receipts: the newest `unread` incoming messages. Cheap, so done; a failure only
        // means the sender keeps grey ticks, so it is logged and not shown.
        let (Some(session), Ok(jid)) = (&self.session, chat_id.parse::<Jid>()) else {
            return;
        };
        let Ok(recent) = self.store.messages(NET, chat_id, OPEN_LIMIT) else {
            return;
        };
        let incoming: Vec<&ChatMessage> = recent.iter().filter(|m| !m.from_me).collect();
        let tail = &incoming[incoming.len().saturating_sub(unread as usize)..];
        // Group receipts must name the message's author; messages from before this run have
        // none remembered and are skipped.
        let mut by_sender: HashMap<Option<Jid>, Vec<String>> = HashMap::new();
        for m in tail {
            if jid.is_group() {
                if let Some(sender) = self.sender_of(chat_id, &m.id) {
                    by_sender
                        .entry(Some(sender))
                        .or_default()
                        .push(m.id.clone());
                }
            } else {
                by_sender.entry(None).or_default().push(m.id.clone());
            }
        }
        for (sender, ids) in by_sender {
            let client = session.client.clone();
            let jid = jid.clone();
            tokio::spawn(async move {
                let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                if let Err(e) = client.mark_as_read(&jid, sender.as_ref(), &refs).await {
                    log::error(AREA, &format!("read receipt failed: {}", chain(e.as_ref())));
                }
            });
        }
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

        let (jobs, mut job_rx) = mpsc::unbounded_channel::<Job>();
        let sender_client = client.clone();
        let out = self.tx.clone();
        tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                // A send that waits on a dead connection must not hold every later message in
                // the queue as "sending" forever.
                let sent = tokio::time::timeout(
                    NETWORK_TIMEOUT,
                    sender_client.send_text(job.jid.clone(), job.text.clone()),
                );
                let result = match sent.await {
                    Ok(Ok(r)) => Ok(r.message_id),
                    Ok(Err(e)) => Err(chain(&e)),
                    Err(_) => Err("WhatsApp did not answer in 15 s. Check the phone's connection, then send again.".to_string()),
                };
                let _ = out.send(Msg::Sent(Sent {
                    chat_id: job.chat_id,
                    client_id: job.client_id,
                    text: job.text,
                    result,
                }));
            }
        });
        log::info(
            AREA,
            if paired {
                "resuming linked session"
            } else {
                "starting new link (QR)"
            },
        );
        self.session = Some(Session {
            handle,
            client,
            jobs,
        });
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
        let Some(s) = self.session.take() else { return };
        let Session {
            handle,
            client,
            jobs,
        } = s;
        drop(jobs);
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
        self.set_status(Status::NeedsLogin { reason: None });
        if !had_session {
            self.notice("Local link deleted. If chatcli is still listed in WhatsApp > Linked devices, remove it there.", false);
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
            Msg::Sent(s) => self.on_sent(s).await,
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
                self.set_status(Status::Online {
                    live: true,
                    detail: None,
                });
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
                log::error(
                    AREA,
                    &format!(
                        "app state sync failed: {} fatal, {} retryable",
                        a.fatal.len(),
                        a.retryable.len()
                    ),
                );
            }
            Event::PushNameUpdate(p) => {
                if self.names.set_push(p.jid.user.as_str(), &p.new_push_name) {
                    self.schedule_retitle();
                }
            }
            Event::ContactUpdate(c) => {
                let a = &c.action;
                let name = [a.full_name.as_deref(), a.first_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .find(|n| !n.is_empty());
                if let Some(name) = name {
                    let mut users = vec![c.jid.user.to_string()];
                    users.extend(
                        [a.lid_jid.as_deref(), a.pn_jid.as_deref()]
                            .into_iter()
                            .flatten()
                            .map(map::user_of),
                    );
                    let mut changed = false;
                    for u in users {
                        changed |= self.names.set_book(&u, name);
                    }
                    if changed {
                        self.schedule_retitle();
                    }
                }
            }
            Event::GroupUpdate(g) => {
                if let GroupNotificationAction::Subject { subject, .. } = &g.action {
                    self.set_group_title(&g.group_jid.to_string(), subject);
                }
            }
            Event::MarkChatAsReadUpdate(m) => {
                if m.action.read == Some(true) {
                    let id = map::canonical_chat(&m.jid, None).to_string();
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
                        "history sync type={kind} progress={progress:?}: {} chats, {} messages, {} skipped, {} names",
                        o.chats,
                        o.messages,
                        o.skipped,
                        o.push_names.len()
                    ),
                );
                for (user, name) in &o.push_names {
                    self.names.set_push(user, name);
                }
                if !o.push_names.is_empty() {
                    self.schedule_retitle();
                }
                self.emit_chats();
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

    /// Applies names learned since the chats were last written.
    pub(crate) fn retitle(&mut self) {
        let Ok(chats) = self.store.chats(NET) else {
            return;
        };
        let mut changed = Vec::new();
        for mut c in chats {
            let Ok(jid) = c.id.parse::<Jid>() else {
                continue;
            };
            if let Some(t) = self.names.better_title(&jid, &c.title) {
                c.title = t;
                changed.push(c);
            }
        }
        if changed.is_empty() {
            return;
        }
        match self.store.upsert_chats(&changed) {
            Ok(()) => self.emit_chats(),
            Err(e) => self.store_failed("save names", &e),
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
    async fn pn_for(&self, lid: &Jid) -> Option<Jid> {
        let client = &self.session.as_ref()?.client;
        let entry = client.get_lid_pn_entry(lid).await.ok().flatten()?;
        Some(Jid::pn(&*entry.phone_number))
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
        let mut chat = map::canonical_chat(&raw_chat, alt);
        if chat.is_lid() {
            if let Some(pn) = self.pn_for(&chat).await {
                chat = pn;
            }
        }
        let chat_id = chat.to_string();
        let push = info.push_name.as_str();
        if !src.is_from_me {
            // Under the sender's address and, in a direct chat, under the chat's (they can differ
            // when the peer is addressed by LID).
            let mut changed = self.names.set_push(src.sender.user.as_str(), push);
            if !chat.is_group() {
                changed |= self.names.set_push(chat.user.as_str(), push);
            }
            if changed {
                self.schedule_retitle();
            }
        }
        match map::content(&m.message) {
            Content::Skip => {}
            Content::Revoke { target } => {
                self.amend(&chat_id, &target, |m| {
                    m.state = MessageState::Deleted;
                    m.text = "[message deleted]".to_string();
                });
            }
            Content::Edit { target, text } => {
                self.amend(&chat_id, &target, |m| m.text = format!("{text} (edited)"));
            }
            Content::Text(text) => {
                if chat.is_group() && !src.is_from_me {
                    self.remember_sender(&chat_id, &info.id, src.sender.to_non_ad());
                }
                let msg = ChatMessage {
                    network: NET,
                    chat_id,
                    id: info.id.clone(),
                    ts: info.timestamp.timestamp_millis(),
                    sender: map::sender_label(src.is_from_me, &src.sender, push, &self.names),
                    from_me: src.is_from_me,
                    text,
                    state: MessageState::Sent,
                };
                self.store_message(&chat, msg, push).await;
            }
        }
    }

    async fn store_message(&mut self, chat: &Jid, msg: ChatMessage, push: &str) {
        // A redelivered message (offline queue after a reconnect) must not count as unread twice.
        let seen = self
            .store
            .messages(NET, &msg.chat_id, 50)
            .map(|v| v.iter().any(|m| m.id == msg.id))
            .unwrap_or(false);
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&msg)) {
            return self.store_failed("save message", &e);
        }
        self.emit(Out::Message(msg.clone()));
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
                id: chat_id,
                title: self.new_chat_title(chat, push).await,
                kind: map::chat_kind(chat),
                last_ts: None,
                last_preview: String::new(),
                unread: 0,
            },
        };
        if c.last_ts.is_none_or(|t| msg.ts >= t) {
            c.last_ts = Some(msg.ts);
            c.last_preview = map::preview(&msg.sender, &msg.text, chat.is_group() || msg.from_me);
        }
        if count_unread {
            c.unread += 1;
        }
        match self.store.upsert_chats(std::slice::from_ref(&c)) {
            Ok(()) => self.emit(Out::ChatUpdated(c)),
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

    /// Edit or delete of a stored message. Only the newest `OPEN_LIMIT` are searched; an older
    /// target is logged and left alone.
    fn amend(&mut self, chat_id: &str, target: &str, change: impl FnOnce(&mut ChatMessage)) {
        let found = match self.store.messages(NET, chat_id, OPEN_LIMIT) {
            Ok(v) => v.into_iter().find(|m| m.id == target),
            Err(e) => return self.store_failed("read messages", &e),
        };
        let Some(mut m) = found else {
            log::info(
                AREA,
                "edit or delete of a message that is not stored (older than the last 200); ignored",
            );
            return;
        };
        change(&mut m);
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&m)) {
            return self.store_failed("update message", &e);
        }
        if let Ok(Some(mut c)) = self.store.chat(NET, chat_id) {
            if c.last_ts == Some(m.ts) {
                let group = chat_id.parse::<Jid>().is_ok_and(|j| j.is_group());
                c.last_preview = map::preview(&m.sender, &m.text, group || m.from_me);
                if self.store.upsert_chats(std::slice::from_ref(&c)).is_ok() {
                    self.emit(Out::ChatUpdated(c));
                }
            }
        }
        self.emit(Out::Message(m));
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
    pub messages: usize,
    pub skipped: usize,
    pub push_names: Vec<(String, String)>,
}

/// Decodes nothing itself: maps one history chunk and writes it to the store in batches.
pub(crate) fn apply_history(
    store: &Store,
    names: &Names,
    hs: &wa::HistorySync,
) -> chatcli_core::Result<HistoryOutcome> {
    let batch = map::map_history(hs, names);
    let existing: HashMap<String, ChatSummary> = store
        .chats(NET)?
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    for chunk in batch.messages.chunks(1000) {
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
    store.upsert_chats(&chats)?;
    Ok(HistoryOutcome {
        chats: chats.len(),
        messages: batch.messages.len(),
        skipped: batch.skipped,
        push_names: batch.push_names,
    })
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;

/// One sentence for the sign-in panel when WhatsApp ended the link.
pub(crate) fn unlinked_reason(reason: &ConnectFailureReason) -> String {
    match reason {
        ConnectFailureReason::AccountLocked => {
            "WhatsApp locked this account. Open WhatsApp on the phone, then press Enter to link again.".to_string()
        }
        _ => "WhatsApp unlinked this device. Press Enter to link again.".to_string(),
    }
}
