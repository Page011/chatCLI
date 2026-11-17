//! The Teams backend task: one tokio task that owns sign-in, Graph, the relay socket and the
//! Graph subscription, and talks to the TUI only through `Command` and `Event`.
//!
//! No timers call Graph to poll Teams (Microsoft allows at most one poll a day). Chats are
//! listed at start, on the user's Refresh, and in a catch-up after a relay reconnect; messages
//! are fetched when the user opens a chat, scrolls to the top, or a change notification names
//! one. The only timer is subscription upkeep (renew at half-life, retry after a failure), which
//! is not polling.
//!
//! Uploads and downloads run on spawned tasks with cloned handles, so a slow transfer never
//! blocks the command loop; `Shutdown` aborts them.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use chatcli_core::limits::{text_fits, TEAMS_TEXT_MAX_BYTES};
use chatcli_core::media::{self, MediaCache};
use chatcli_core::model::{
    now_ms, Capabilities, ChatFlag, ChatKind, ChatMessage, ChatSummary, Command, CommandRx, Event,
    EventTx, MediaKind, MessageState, Network, PersonRef, QuotedRef, Status, MUTE_FOREVER,
};
use chatcli_core::runtime::BackendCtx;
use chatcli_core::settings::Settings;
use chatcli_core::store::{Store, REACTOR_ME};
use chatcli_core::{config, log};

use crate::auth::{self, AuthFailure, ScopeSet};
use crate::body;
use crate::graph::{self, ApiResult, Graph};
use crate::map::{self, has_scope, FetchSpec, Local, RemoteLast, TOKEN_MARGIN_MS};
use crate::relay::{self, Frame, LifecycleItem, Notification, RelayEvent};
use crate::subscription::{self, Spec, SubState, SUB_ID_KEY};
use crate::transfer;

const NET: Network = Network::Teams;
/// 20 pages of 50 = 1000 chats. More is reported, never dropped silently.
const MAX_CHAT_PAGES: u32 = 20;
/// Messages fetched from Graph when a chat opens or a gap is filled.
const FETCH_MESSAGES: u32 = 50;
/// Messages shown from the local store.
const STORE_MESSAGES: u32 = 200;
/// Older messages answered per scroll past the top.
const OLDER_PAGE: u32 = 50;
/// Chats fetched in one catch-up, so a long absence cannot trigger a request storm.
const CATCH_UP_MAX_CHATS: usize = 25;
/// After a failed subscription create or renew, try again after this long.
const SUBSCRIPTION_RETRY_MS: i64 = 10 * 60 * 1000;
const SHUTDOWN_CLEANUP: Duration = Duration::from_secs(5);
/// A saved member list younger than this is served without asking Teams again.
const MEMBERS_FRESH_MS: i64 = 10 * 60 * 1000;
/// Pages of members read for one chat (50 each): well past any real chat.
const MEMBERS_MAX_PAGES: u32 = 21;
/// Upload progress goes to the TUI at most four times a second per transfer.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Fixed user-facing texts (contract section 2).
const REPLY_NEEDS_SCOPE: &str = "REPLY NEEDS ChatMessage.Send, sign in again";
const NOT_SIGNED_IN: &str = "Not signed in to Teams. Sign in first.";

/// Why a Graph call failed, as far as the backend needs to tell.
enum Fail {
    /// Microsoft will not give a token without a new sign-in: show the sign-in panel.
    NeedsLogin(String),
    /// Anything else: message plus the HTTP status (0 = no HTTP answer).
    Other(String, u16),
}

type Outcome<T> = std::result::Result<T, Fail>;

struct CachedToken {
    access: String,
    expires_at_ms: i64,
}

/// What the status chip says about live updates. Pure, so every combination is tested.
/// Live = relay set up, socket up and subscription active.
pub fn live_state(
    relay_note: Option<&str>,
    socket_up: bool,
    socket_note: Option<&str>,
    subscription_active: bool,
    subscription_note: Option<&str>,
) -> (bool, Option<String>) {
    if let Some(note) = relay_note {
        return (false, Some(note.to_string()));
    }
    if !socket_up {
        return (
            false,
            Some(socket_note.unwrap_or("connecting to relay").to_string()),
        );
    }
    if !subscription_active {
        return (
            false,
            Some(
                subscription_note
                    .unwrap_or("no Teams subscription yet")
                    .to_string(),
            ),
        );
    }
    (true, None)
}

/// When subscription upkeep next runs: a pending retry wins, otherwise the renew time.
fn next_upkeep_ms(sub: Option<&SubState>, retry_at_ms: Option<i64>) -> Option<i64> {
    retry_at_ms.or(sub.map(|s| s.renew_at_ms))
}

struct Backend {
    store: Store,
    settings: Arc<Settings>,
    media: MediaCache,
    events: EventTx,
    http: reqwest::Client,
    relay_tx: mpsc::UnboundedSender<RelayEvent>,
    token: Option<CachedToken>,
    /// The scopes the current token carries.
    granted: Vec<String>,
    last_caps: Option<Capabilities>,
    /// The "scopes not granted yet" notice goes out once per sign-in, not on every refresh.
    scope_warned: bool,
    /// Uploads and downloads in flight.
    tasks: JoinSet<()>,
    /// Photo batches run one after another; the value is "throttled until" after a 429.
    avatar_gate: Arc<tokio::sync::Mutex<Option<Instant>>>,
    /// Bumped on sign-out: a running photo batch stops and answers nothing.
    avatar_gen: Arc<std::sync::atomic::AtomicU64>,
    me_id: Option<String>,
    tenant_id: Option<String>,
    relay: Option<relay::Client>,
    spec: Option<Spec>,
    sub: Option<SubState>,
    /// Set while relay is not configured at all ("relay not set up").
    relay_note: Option<String>,
    socket_up: bool,
    socket_note: Option<String>,
    sub_note: Option<String>,
    retry_at_ms: Option<i64>,
    /// Chat ids that exist on Teams but are not shown (meeting, other types, unknown after a relist).
    /// A notification for one of these is dropped without a Graph call.
    not_shown: HashSet<String>,
    /// 1:1 and group chats the user hid. A new message unhides them on Teams, so a "created"
    /// notification for one is followed up (re-list, then fetch) instead of trusting the flag.
    hidden: HashSet<String>,
    last_status: Option<Status>,
    /// Tests point Graph at a local mock server.
    #[cfg(test)]
    graph_base: Option<String>,
}

pub async fn run(ctx: BackendCtx, mut commands: CommandRx, events: EventTx) {
    let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();
    let http = match auth::http_client() {
        Ok(h) => h,
        Err(e) => {
            log::error("teams", &e);
            let _ = events.send(Event::Status(NET, Status::Error { message: e }));
            while let Some(c) = commands.recv().await {
                if c == Command::Shutdown {
                    break;
                }
            }
            return;
        }
    };
    let mut b = Backend::new(ctx, events, http, relay_tx);
    b.start().await;
    loop {
        let deadline = b.next_deadline();
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None | Some(Command::Shutdown) => break,
                Some(c) => {
                    if b.command(c, &mut commands).await {
                        break;
                    }
                }
            },
            Some(ev) = relay_rx.recv() => b.relay_event(ev).await,
            Some(done) = b.tasks.join_next(), if !b.tasks.is_empty() => {
                if let Err(e) = done {
                    if !e.is_cancelled() {
                        log::error("teams", &format!("a transfer task stopped: {e}"));
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => b.upkeep().await,
        }
    }
    b.shutdown().await;
}

// --- helpers that need no backend state: shared by the command loop and spawned tasks ----------

/// What storing a fetched message found.
struct Stored {
    /// As read back from the store: attachments, reactions and quote included.
    message: ChatMessage,
    existed: bool,
    /// Differs from what was stored before (an edit, a delete, a reaction).
    changed: bool,
}

/// Maps fetched messages, stores them with their reactions (Graph gives the full list each
/// time) and reads them back. A message without a timestamp is skipped and logged.
fn store_fetched(
    store: &Store,
    chat_id: &str,
    fetched: &[graph::Message],
    me: &str,
) -> Result<Vec<Stored>, String> {
    let mapped: Vec<(&graph::Message, ChatMessage)> = fetched
        .iter()
        .filter_map(|m| map::map_message(m, chat_id, me).map(|c| (m, c)))
        .collect();
    if mapped.len() < fetched.len() {
        log::error("teams", "some messages had no timestamp and were skipped");
    }
    let before = mapped
        .iter()
        .map(|(_, c)| store.message(NET, chat_id, &c.id))
        .collect::<Result<Vec<_>, _>>()?;
    let all: Vec<ChatMessage> = mapped.iter().map(|(_, c)| c.clone()).collect();
    store.upsert_messages(&all)?;
    let mut out = Vec::with_capacity(mapped.len());
    for ((raw, mapped), before) in mapped.iter().zip(before) {
        let rows = if mapped.state == MessageState::Deleted {
            Vec::new()
        } else {
            map::reaction_rows(raw, me)
        };
        store.replace_reactions(NET, chat_id, &mapped.id, &rows)?;
        let after = store
            .message(NET, chat_id, &mapped.id)?
            .unwrap_or_else(|| mapped.clone());
        out.push(Stored {
            existed: before.is_some(),
            changed: before.as_ref().is_some_and(|b| *b != after),
            message: after,
        });
    }
    Ok(out)
}

/// Moves a chat's last-message fields forward for one message and sends ChatUpdated.
/// `unread`: the new count, or None to leave it.
fn touch_chat(
    store: &Store,
    events: &EventTx,
    chat_id: &str,
    message: &ChatMessage,
    unread: Option<u32>,
) {
    let Ok(Some(mut chat)) = store.chat(NET, chat_id) else {
        return;
    };
    if message.ts >= chat.last_ts.unwrap_or(i64::MIN) {
        chat.last_ts = Some(message.ts);
        chat.last_preview = map::preview_line(&message.sender, message.from_me, &message.text);
    }
    if let Some(n) = unread {
        chat.unread = n;
    }
    if let Err(e) = store.upsert_chats(std::slice::from_ref(&chat)) {
        log::error("teams", &e);
    }
    // Read back: the stored copy carries the mention counter and the flags.
    let chat = store.chat(NET, chat_id).ok().flatten().unwrap_or(chat);
    let _ = events.send(Event::ChatUpdated(chat));
}

/// The deleted form of a stored message: no text, files, quote, mentions or reactions. Read back
/// from the store before and after, so nothing else about the message is lost.
fn apply_deleted(
    store: &Store,
    chat_id: &str,
    message_id: &str,
) -> Result<Option<ChatMessage>, String> {
    let Some(mut m) = store.message(NET, chat_id, message_id)? else {
        return Ok(None);
    };
    m.state = MessageState::Deleted;
    m.text.clear();
    m.attachments.clear();
    m.reactions.clear();
    m.mentions.clear();
    m.mentions_me = false;
    m.reply_to = None;
    store.upsert_messages(std::slice::from_ref(&m))?;
    store.replace_reactions(NET, chat_id, message_id, &[])?;
    store.message(NET, chat_id, message_id)
}

/// Ends a send: confirms the pending message with Graph's answer, or keeps it as failed so it
/// still shows NOT SENT after reopening. Returns the failure text when it failed.
#[allow(clippy::too_many_arguments)]
fn finish_send(
    store: &Store,
    events: &EventTx,
    me: &str,
    chat_id: &str,
    client_id: &str,
    fallback_text: &str,
    reply_to: Option<QuotedRef>,
    result: Result<graph::Message, String>,
) -> Option<String> {
    match result {
        Ok(sent) => {
            // Graph's answer has the real id and time; without a time, use ours.
            let mut message = map::map_message(&sent, chat_id, me).unwrap_or_else(|| ChatMessage {
                chat_id: chat_id.to_string(),
                id: sent.id.clone(),
                ts: now_ms(),
                sender: "You".into(),
                from_me: true,
                text: fallback_text.to_string(),
                ..ChatMessage::blank(NET)
            });
            message.from_me = true;
            // Graph's answer carries the quote; the one the user chose is the fallback.
            if message.reply_to.is_none() {
                message.reply_to = reply_to;
            }
            if let Err(e) = store.confirm_message(client_id, &message) {
                log::error("teams", &e);
                let _ = events.send(Event::Notice {
                    network: NET,
                    text: e,
                    is_error: true,
                });
            }
            let _ = events.send(Event::MessageConfirmed {
                client_id: client_id.to_string(),
                message: message.clone(),
            });
            touch_chat(store, events, chat_id, &message, None);
            None
        }
        Err(error) => {
            log::error("teams-send", &format!("send failed: {error}"));
            let failed = ChatMessage {
                chat_id: chat_id.to_string(),
                id: client_id.to_string(),
                ts: now_ms(),
                sender: "You".into(),
                from_me: true,
                text: fallback_text.to_string(),
                state: MessageState::Failed(error.clone()),
                reply_to,
                ..ChatMessage::blank(NET)
            };
            if let Err(e) = store.upsert_messages(&[failed]) {
                log::error("teams", &e);
            }
            let _ = events.send(Event::MessageFailed {
                network: NET,
                chat_id: chat_id.to_string(),
                client_id: client_id.to_string(),
                error: error.clone(),
            });
            Some(error)
        }
    }
}

/// Mutable parts of one chat refresh.
struct Refreshed {
    shown: Vec<ChatSummary>,
    /// What each chat's list entry says its last message is.
    last: HashMap<String, RemoteLast>,
}

impl Backend {
    fn new(
        ctx: BackendCtx,
        events: EventTx,
        http: reqwest::Client,
        relay_tx: mpsc::UnboundedSender<RelayEvent>,
    ) -> Self {
        Backend {
            store: ctx.store,
            settings: ctx.settings,
            media: ctx.media,
            events,
            http,
            relay_tx,
            token: None,
            granted: Vec::new(),
            last_caps: None,
            scope_warned: false,
            tasks: JoinSet::new(),
            avatar_gate: Arc::new(tokio::sync::Mutex::new(None)),
            avatar_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            me_id: None,
            tenant_id: None,
            relay: None,
            spec: None,
            sub: None,
            relay_note: None,
            socket_up: false,
            socket_note: None,
            sub_note: None,
            retry_at_ms: None,
            not_shown: HashSet::new(),
            hidden: HashSet::new(),
            last_status: None,
            #[cfg(test)]
            graph_base: None,
        }
    }

    // --- output ----------------------------------------------------------------------------

    fn emit(&self, event: Event) {
        // The TUI going away is the end of the program; nothing to report.
        let _ = self.events.send(event);
    }

    fn emit_status(&mut self, status: Status) {
        self.last_status = Some(status.clone());
        self.emit(Event::Status(NET, status));
    }

    /// One short line for the status bar, also in the log. Never put message text in `text`.
    fn notice(&self, text: &str, is_error: bool) {
        if is_error {
            log::error("teams", text);
        } else {
            log::info("teams", text);
        }
        self.emit(Event::Notice {
            network: NET,
            text: text.to_string(),
            is_error,
        });
    }

    fn emit_chats(&self) {
        match self.store.chats(NET) {
            Ok(chats) => self.emit(Event::Chats(NET, chats)),
            Err(e) => self.notice(&e, true),
        }
    }

    fn emit_chat(&self, chat_id: &str) {
        if let Ok(Some(chat)) = self.store.chat(NET, chat_id) {
            self.emit(Event::ChatUpdated(chat));
        }
    }

    fn emit_history(&self, chat_id: &str) {
        match self.store.messages(NET, chat_id, STORE_MESSAGES) {
            Ok(messages) => self.emit(Event::Messages {
                network: NET,
                chat_id: chat_id.to_string(),
                messages,
            }),
            Err(e) => self.notice(&e, true),
        }
    }

    /// Emits the Online chip when it differs from the last status sent.
    /// A no-op while signed out: a failed refresh inside the caller already sent NeedsLogin,
    /// and an Online chip over it would hide the sign-in prompt.
    fn publish_online(&mut self) {
        if self.me_id.is_none() || matches!(self.last_status, Some(Status::NeedsLogin { .. })) {
            return;
        }
        let active = self.sub.as_ref().is_some_and(|s| s.active(now_ms()));
        let (live, detail) = live_state(
            self.relay_note.as_deref(),
            self.socket_up,
            self.socket_note.as_deref(),
            active,
            self.sub_note.as_deref(),
        );
        let status = Status::Online { live, detail };
        if self.last_status.as_ref() != Some(&status) {
            self.emit_status(status);
        }
    }

    /// What Teams can do now (the token's scopes and the settings).
    fn caps(&self) -> Capabilities {
        map::capabilities(&self.granted, &self.settings.teams)
    }

    /// The user changed settings in the TUI: use the new limits and capability switches now.
    /// File scopes themselves are asked for at the next sign-in, which reads `settings.toml`
    /// again (`auth::settings_scopes`).
    fn apply_settings(&mut self, settings: Arc<Settings>) {
        self.media = self.media.with_limits(&settings.media);
        self.settings = settings;
        // Nothing sent yet (not signed in): the sign-in publishes the first set.
        if self.last_caps.is_some() {
            self.publish_caps();
        }
    }

    /// Sends the capabilities when they differ from the last ones sent.
    fn publish_caps(&mut self) {
        let caps = self.caps();
        if self.last_caps != Some(caps) {
            self.last_caps = Some(caps);
            self.emit(Event::Capabilities { network: NET, caps });
        }
    }

    /// A refresh can fall back to fewer scopes (a new one was never consented to), and the TUI
    /// only dims the action. Say once, in words, which scopes are missing and what to do.
    fn warn_missing_scopes(&mut self) {
        if self.scope_warned {
            return;
        }
        let caps = self.caps();
        let teams = &self.settings.teams;
        let mut missing = Vec::new();
        if !caps.reply {
            missing.push("ChatMessage.Send (replies)");
        }
        if teams.files_read && !caps.fetch_files {
            missing.push("Files.Read.All (file downloads)");
        }
        if teams.files_write && !caps.send_file {
            missing.push("Files.ReadWrite (sending files)");
        }
        if teams.photos && !has_scope(&self.granted, auth::PHOTOS_SCOPE) {
            missing.push("User.ReadBasic.All (profile photos)");
        }
        if !missing.is_empty() {
            self.scope_warned = true;
            self.notice(
                &format!(
                    "Not granted yet: {}. Log out of Teams and sign in again to get them.",
                    missing.join(", ")
                ),
                false,
            );
        }
    }

    // --- tokens and Graph calls ------------------------------------------------------------

    /// Profile photos need the setting and the consent.
    fn photos_allowed(&self) -> bool {
        self.settings.teams.photos && has_scope(&self.granted, auth::PHOTOS_SCOPE)
    }

    fn scopes(&self) -> ScopeSet {
        ScopeSet::from_settings(&self.settings.teams)
    }

    fn graph(&self, token: String) -> Graph {
        let g = Graph::new(self.http.clone(), token);
        #[cfg(test)]
        let g = match &self.graph_base {
            Some(base) => g.with_base(base),
            None => g,
        };
        g
    }

    fn set_token(&mut self, token: auth::Token) {
        if let Some(tid) = map::tid_from_jwt(&token.access) {
            self.tenant_id = Some(tid);
        }
        let expires_at_ms =
            now_ms() + i64::try_from(token.expires_in_secs).unwrap_or(i64::MAX / 2_000) * 1000;
        self.granted = token.granted;
        self.token = Some(CachedToken {
            access: token.access,
            expires_at_ms,
        });
        // A refresh that had to drop a scope, or a new sign-in that gained one, changes what
        // the TUI may offer.
        if self.me_id.is_some() {
            self.publish_caps();
        }
    }

    /// The cached access token, refreshed a minute early (or now, when `force`).
    async fn token(&mut self, force: bool) -> Outcome<String> {
        if !force {
            if let Some(t) = &self.token {
                if !map::needs_refresh(t.expires_at_ms, now_ms(), TOKEN_MARGIN_MS) {
                    return Ok(t.access.clone());
                }
            }
        }
        match auth::refresh_with(&self.http, &self.scopes()).await {
            Ok(token) => {
                let access = token.access.clone();
                self.set_token(token);
                Ok(access)
            }
            Err(AuthFailure::Rejected(reason)) => {
                self.token = None;
                Err(Fail::NeedsLogin(reason))
            }
            Err(AuthFailure::Other(message)) => Err(Fail::Other(message, 0)),
        }
    }

    /// Runs one Graph call. A 401 refreshes the token once and retries once; a second 401
    /// means the sign-in is unusable.
    async fn call<T, F, Fut>(&mut self, f: F) -> Outcome<T>
    where
        F: Fn(Graph) -> Fut,
        Fut: Future<Output = ApiResult<T>>,
    {
        let token = self.token(false).await?;
        match f(self.graph(token)).await {
            Ok(v) => Ok(v),
            Err(e) if e.status == 401 => {
                log::info("teams", "Graph answered 401, refreshing the token once");
                let token = self.token(true).await?;
                f(self.graph(token)).await.map_err(|e| {
                    if e.status == 401 {
                        Fail::NeedsLogin(e.message)
                    } else {
                        Fail::Other(e.message, e.status)
                    }
                })
            }
            Err(e) => Err(Fail::Other(e.message, e.status)),
        }
    }

    /// A Graph handle for a spawned task, or the reason there is none (signed out).
    async fn task_graph(&mut self) -> Result<Graph, String> {
        if self.me_id.is_none() {
            return Err(NOT_SIGNED_IN.to_string());
        }
        match self.token(false).await {
            Ok(token) => Ok(self.graph(token)),
            Err(Fail::NeedsLogin(reason)) => {
                self.signed_out(Some(reason.clone()));
                Err(reason)
            }
            Err(Fail::Other(message, _)) => Err(message),
        }
    }

    /// Shows a failure. `as_status`: also set the tab's status to Error (start-up failures,
    /// where there is nothing else on screen to say it).
    fn fail(&mut self, f: Fail, what: &str, as_status: bool) {
        match f {
            Fail::NeedsLogin(reason) => self.signed_out(Some(reason)),
            Fail::Other(message, _) => {
                let text = format!("{what}: {message}");
                self.notice(&text, true);
                if as_status {
                    self.emit_status(Status::Error { message: text });
                }
            }
        }
    }

    fn teardown_live(&mut self) {
        self.relay = None;
        self.spec = None;
        self.sub = None;
        self.socket_up = false;
        self.socket_note = None;
        self.sub_note = None;
        self.retry_at_ms = None;
    }

    /// Forgets who is signed in. Capabilities are re-sent after the next sign-in.
    fn forget_session(&mut self) {
        self.avatar_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.token = None;
        self.granted.clear();
        self.last_caps = None;
        self.scope_warned = false;
        self.me_id = None;
        self.tenant_id = None;
    }

    /// Back to the sign-in panel, with Microsoft's reason when there is one.
    fn signed_out(&mut self, reason: Option<String>) {
        self.teardown_live();
        self.forget_session();
        if let Some(r) = &reason {
            self.notice(r, true);
        }
        self.emit_status(Status::NeedsLogin { reason });
    }

    // --- start, sign-in, chat list ---------------------------------------------------------

    async fn start(&mut self) {
        self.emit_status(Status::Starting);
        self.emit_chats();
        if let Err(e) = auth::app() {
            // The text always ends with auth::NOT_SET_UP; a broken config.json says so first.
            log::error("teams", &e);
            self.emit_status(Status::NeedsLogin { reason: Some(e) });
            return;
        }
        match auth::has_stored_sign_in() {
            Ok(false) => self.emit_status(Status::NeedsLogin { reason: None }),
            Err(e) => {
                log::error("teams", &e);
                self.emit_status(Status::Error { message: e });
            }
            Ok(true) => self.go_online().await,
        }
    }

    /// Signed in: identify the user, load the chat list, then bring up live updates.
    async fn go_online(&mut self) {
        self.emit_status(Status::Connecting);
        let me = match self.call(|g| async move { g.me().await }).await {
            Ok(me) => me,
            Err(f) => return self.fail(f, "Could not reach Teams", true),
        };
        // The cached rows carry account-relative facts (from_me, unread): another account
        // signing in over them would show the old account's messages as "You".
        match self.store.bind_account(NET, &me.id) {
            Ok(true) => {
                log::info(
                    "teams",
                    "a different account signed in: cached Teams data cleared",
                );
                self.emit_chats();
            }
            Ok(false) => {}
            Err(e) => self.notice(&e, true),
        }
        self.me_id = Some(me.id);
        if let Err(f) = self.refresh_chats().await {
            return self.fail(f, "Could not load chats", true);
        }
        self.setup_live().await;
        self.publish_caps();
        self.warn_missing_scopes();
    }

    /// Lists every chat, keeps 1:1 and group chats (hidden ones flagged, not dropped), replaces
    /// the store's copy and sends the sidebar. Returns the fresh summaries.
    async fn refresh_chats(&mut self) -> Outcome<Refreshed> {
        let page = self
            .call(|g| async move { g.chats_all(MAX_CHAT_PAGES).await })
            .await?;
        let me = self.me_id.clone().unwrap_or_default();
        let mut shown = Vec::new();
        let mut last = HashMap::new();
        self.not_shown.clear();
        self.hidden.clear();
        for chat in &page.chats {
            match map::chat_summary(chat, &me) {
                Some(summary) => {
                    if summary.flags.hidden {
                        self.hidden.insert(chat.id.clone());
                    }
                    if let Some(l) = map::remote_last(chat) {
                        last.insert(chat.id.clone(), l);
                    }
                    shown.push(summary);
                }
                None => {
                    self.not_shown.insert(chat.id.clone());
                }
            }
        }
        let saved = if page.capped {
            // The list is incomplete, so a chat missing from it is not proof it is gone:
            // replacing would delete its stored messages.
            self.notice(
                &format!(
                    "Chat list stopped at {MAX_CHAT_PAGES} pages ({} chats): older chats are not shown.",
                    MAX_CHAT_PAGES * crate::graph::MAX_PAGE
                ),
                true,
            );
            self.store.upsert_chats(&shown)
        } else {
            self.store.replace_chats(NET, &shown)
        };
        if let Err(e) = saved {
            self.notice(&e, true);
        }
        self.sync_hidden_flags(&shown);
        self.emit_chats();
        Ok(Refreshed { shown, last })
    }

    /// Teams knows which chats the user hid: its answer wins over the stored flag. Pin and mute
    /// are local only and are never touched here.
    fn sync_hidden_flags(&self, shown: &[ChatSummary]) {
        for chat in shown {
            let Ok(mut flags) = self.store.chat_flags(NET, &chat.id) else {
                continue;
            };
            if flags.hidden != chat.flags.hidden {
                flags.hidden = chat.flags.hidden;
                if let Err(e) = self.store.set_chat_flags(NET, &chat.id, flags) {
                    self.notice(&e, true);
                }
            }
        }
    }

    async fn login(&mut self, commands: &mut CommandRx) -> bool {
        self.teardown_live();
        self.emit_status(Status::LoggingIn);
        let events = self.events.clone();
        let http = self.http.clone();
        let scopes = self.scopes();
        let sign_in = auth::sign_in_token_with(&http, &scopes, move |url| {
            let _ = events.send(Event::Notice {
                network: NET,
                text: format!("If no browser opened, sign in at: {url}"),
                is_error: false,
            });
        });
        tokio::pin!(sign_in);
        // The pinned sign-in future borrows nothing of self, but keep the newest settings and
        // apply them once it is done.
        let mut new_settings = None;
        let result = loop {
            tokio::select! {
                result = &mut sign_in => break result,
                cmd = commands.recv() => {
                    // Quit stays possible while the browser is open; anything else waits.
                    match cmd {
                        None | Some(Command::Shutdown) => return true,
                        Some(Command::Settings(s)) => new_settings = Some(s),
                        Some(_) => {}
                    }
                }
            }
        };
        if let Some(s) = new_settings {
            self.apply_settings(s);
        }
        match result {
            Ok(token) => {
                log::info("teams", "sign-in finished");
                self.set_token(token);
                self.go_online().await;
            }
            Err(reason) => {
                log::error("teams", &format!("sign-in failed: {reason}"));
                self.signed_out(Some(reason));
            }
        }
        false
    }

    async fn logout(&mut self) {
        self.remove_subscription("logout").await;
        self.teardown_live();
        if let Err(e) = auth::sign_out() {
            self.notice(
                &format!("Could not clear the stored Teams sign-in: {e}"),
                true,
            );
        }
        self.forget_session();
        self.emit_status(Status::NeedsLogin { reason: None });
    }

    // --- commands --------------------------------------------------------------------------

    /// Returns true when the task should end. Every command is answered (contract section 3);
    /// `Typing` is the one documented silent drop: the TUI never sends it to Teams.
    async fn command(&mut self, cmd: Command, commands: &mut CommandRx) -> bool {
        match cmd {
            Command::Login => return self.login(commands).await,
            Command::Logout => self.logout().await,
            Command::Refresh => self.refresh().await,
            Command::OpenChat { chat_id } => self.open_chat(&chat_id).await,
            Command::Send {
                chat_id,
                text,
                client_id,
                reply_to,
                mentions,
            } => {
                self.send(&chat_id, &text, &client_id, reply_to, &mentions)
                    .await
            }
            Command::MarkRead { chat_id } => self.mark_read(&chat_id).await,
            Command::SendMedia {
                chat_id,
                client_id,
                path,
                kind: _,
                caption,
            } => self.send_media(chat_id, client_id, path, caption).await,
            Command::React {
                chat_id,
                message_id,
                emoji,
                remove,
            } => self.react(&chat_id, &message_id, &emoji, remove).await,
            Command::Edit {
                chat_id,
                message_id,
                text,
            } => self.edit(&chat_id, &message_id, &text).await,
            Command::Delete {
                chat_id,
                message_id,
            } => self.delete(&chat_id, &message_id).await,
            Command::FetchMedia {
                chat_id,
                message_id,
                index,
            } => self.fetch_media(chat_id, message_id, index).await,
            Command::LoadOlder {
                chat_id,
                before_ts,
                before_id,
            } => self.load_older(&chat_id, before_ts, &before_id).await,
            Command::LoadMembers { chat_id } => self.load_members(&chat_id).await,
            Command::Typing { .. } => {}
            Command::FetchAvatars { people } => self.fetch_avatars(people).await,
            Command::Settings(s) => self.apply_settings(s),
            Command::SetChatFlag { chat_id, flag, on } => {
                self.set_chat_flag(&chat_id, flag, on).await
            }
            Command::Shutdown => return true,
        }
        false
    }

    async fn refresh(&mut self) {
        if self.me_id.is_none() {
            // Not signed in yet (or the last attempt failed): redo the start-up checks.
            return self.start().await;
        }
        match self.refresh_chats().await {
            Ok(_) => self.setup_live().await,
            Err(f) => self.fail(f, "Could not refresh chats", false),
        }
    }

    async fn open_chat(&mut self, chat_id: &str) {
        self.emit_history(chat_id);
        if self.me_id.is_none() {
            return;
        }
        match self.fetch_history(chat_id).await {
            Ok(_) => self.emit_history(chat_id),
            Err(f) => self.fail(f, "Could not load messages", false),
        }
    }

    /// Stores the newest messages of a chat and says which are new, which changed.
    async fn fetch_history(&mut self, chat_id: &str) -> Outcome<History> {
        let newest = self.newest_stored(chat_id);
        let me = self.me_id.clone().unwrap_or_default();
        let fetched = self
            .call(move |g| async move { g.messages(chat_id, FETCH_MESSAGES).await })
            .await?;
        let stored =
            store_fetched(&self.store, chat_id, &fetched, &me).map_err(|e| Fail::Other(e, 0))?;
        let is_new = |s: &Stored| !s.existed && newest.is_none_or(|n| s.message.ts > n);
        let gap = newest.is_some()
            && fetched.len() >= FETCH_MESSAGES as usize
            && stored
                .iter()
                .all(|s| s.message.ts > newest.unwrap_or(i64::MIN));
        Ok(History {
            new: stored
                .iter()
                .filter(|s| is_new(s))
                .map(|s| s.message.clone())
                .collect(),
            // A message that was missing from the middle counts as changed: no toast for it.
            changed: stored
                .iter()
                .filter(|s| s.changed || (!s.existed && !is_new(s)))
                .map(|s| s.message.clone())
                .collect(),
            gap,
        })
    }

    /// Newest message Teams itself reported: local failed sends are left out, see the store.
    fn newest_stored(&self, chat_id: &str) -> Option<i64> {
        self.store.newest_synced_ts(NET, chat_id).ok().flatten()
    }

    // --- sending ---------------------------------------------------------------------------

    async fn send(
        &mut self,
        chat_id: &str,
        text: &str,
        client_id: &str,
        reply_to: Option<QuotedRef>,
        mentions: &[PersonRef],
    ) {
        let me = self.me_id.clone().unwrap_or_default();
        let result = self
            .post_text(chat_id, text, reply_to.as_ref(), mentions)
            .await;
        let (error, login) = match result {
            Ok(sent) => {
                finish_send(
                    &self.store,
                    &self.events,
                    &me,
                    chat_id,
                    client_id,
                    text,
                    reply_to,
                    Ok(sent),
                );
                return;
            }
            Err(Fail::NeedsLogin(reason)) => (reason.clone(), Some(Fail::NeedsLogin(reason))),
            Err(Fail::Other(message, _)) => (message, None),
        };
        finish_send(
            &self.store,
            &self.events,
            &me,
            chat_id,
            client_id,
            text,
            reply_to,
            Err(error),
        );
        if let Some(f) = login {
            self.fail(f, "Could not send", false);
        }
    }

    async fn post_text(
        &mut self,
        chat_id: &str,
        text: &str,
        reply_to: Option<&QuotedRef>,
        mentions: &[PersonRef],
    ) -> Outcome<graph::Message> {
        if self.me_id.is_none() {
            return Err(Fail::NeedsLogin(NOT_SIGNED_IN.into()));
        }
        // A reply never goes out as a plain message: the quote would silently vanish.
        if reply_to.is_some() && !self.caps().reply {
            return Err(Fail::Other(REPLY_NEEDS_SCOPE.into(), 403));
        }
        let too_long = || {
            Fail::Other(
                "MESSAGE TOO LONG: Teams takes about 80 KB per message, so split it.".into(),
                0,
            )
        };
        if !text_fits(NET, text) {
            return Err(too_long());
        }
        // @mentions work in group chats; in a 1:1 chat they stay plain text.
        let group =
            matches!(self.store.chat(NET, chat_id), Ok(Some(c)) if c.kind == ChatKind::Group);
        let built = body::text_body(text, if group { mentions } else { &[] });
        if built.html.len() > TEAMS_TEXT_MAX_BYTES {
            return Err(too_long());
        }
        let message = body::message_json(&built);
        match reply_to {
            None => {
                let message = &message;
                self.call(move |g| async move { g.post_message(chat_id, message).await })
                    .await
            }
            Some(quoted) => {
                let request = body::reply_json(&quoted.id, message);
                let request = &request;
                self.call(move |g| async move { g.reply_with_quote(chat_id, request).await })
                    .await
                    // The scope was revoked or never consented: say so, never retry plain.
                    .map_err(|f| match f {
                        Fail::Other(_, 403) => Fail::Other(REPLY_NEEDS_SCOPE.into(), 403),
                        other => other,
                    })
            }
        }
    }

    async fn send_media(
        &mut self,
        chat_id: String,
        client_id: String,
        path: PathBuf,
        caption: String,
    ) {
        let me = self.me_id.clone().unwrap_or_default();
        // The backend decides by content what the file is, whatever the TUI guessed.
        let as_image = path.is_file() && media::kind_for_path(&path) == MediaKind::Image;
        let fallback = if caption.is_empty() {
            if as_image { "[image]" } else { "[file]" }.to_string()
        } else {
            caption.clone()
        };
        let early = if !path.exists() {
            Some(transfer::FILE_NOT_FOUND.to_string())
        } else if !text_fits(NET, &caption) {
            Some("MESSAGE TOO LONG: shorten the caption.".to_string())
        } else if !as_image && !self.caps().send_file {
            // Nothing is uploaded without the opt-in setting and the consented scope.
            Some(transfer::FILE_SEND_NEEDS_SCOPE.to_string())
        } else {
            None
        };
        if let Some(error) = early {
            finish_send(
                &self.store,
                &self.events,
                &me,
                &chat_id,
                &client_id,
                &fallback,
                None,
                Err(error),
            );
            return;
        }
        let graph = match self.task_graph().await {
            Ok(g) => g,
            Err(reason) => {
                finish_send(
                    &self.store,
                    &self.events,
                    &me,
                    &chat_id,
                    &client_id,
                    &fallback,
                    None,
                    Err(reason),
                );
                return;
            }
        };
        let (store, events) = (self.store.clone(), self.events.clone());
        log::info(
            "teams",
            &format!(
                "sending {} in the background",
                if as_image { "a picture" } else { "a file" }
            ),
        );
        self.tasks.spawn(async move {
            let result = if as_image {
                transfer::send_image(&graph, &chat_id, &path, &caption).await
            } else {
                let mut last: Option<Instant> = None;
                let progress_events = events.clone();
                let (progress_chat, progress_client) = (chat_id.clone(), client_id.clone());
                let mut progress = move |done: u64, total: u64| {
                    let due = last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
                    if due || done >= total {
                        last = Some(Instant::now());
                        let _ = progress_events.send(Event::SendProgress {
                            network: NET,
                            chat_id: progress_chat.clone(),
                            client_id: progress_client.clone(),
                            done,
                            total,
                        });
                    }
                };
                transfer::send_file(
                    &graph,
                    &chat_id,
                    &path,
                    &caption,
                    transfer::UPLOAD_CHUNK,
                    &mut progress,
                )
                .await
            };
            let result = result.map_err(|e| {
                // A 403 on the upload means OneDrive writes are not allowed for this sign-in.
                if !as_image && e.status == 403 {
                    transfer::FILE_SEND_NEEDS_SCOPE.to_string()
                } else {
                    e.message
                }
            });
            finish_send(
                &store, &events, &me, &chat_id, &client_id, &fallback, None, result,
            );
        });
    }

    // --- reactions, edit, delete -----------------------------------------------------------

    /// Sets or removes the user's reaction. Teams lets a user hold several, so setting a new
    /// emoji first removes any other one of theirs: one per user per message. The stored copy
    /// follows each successful step, and the change goes out as `MessageChanged`.
    async fn react(&mut self, chat_id: &str, message_id: &str, emoji: &str, remove: bool) {
        if self.me_id.is_none() {
            return self.notice(NOT_SIGNED_IN, true);
        }
        let mine: Vec<String> = match self.store.message(NET, chat_id, message_id) {
            Ok(Some(m)) => m
                .reactions
                .into_iter()
                .filter(|r| r.mine)
                .map(|r| r.emoji)
                .collect(),
            Ok(None) => {
                log::info("teams", "reaction for a message chatcli does not hold");
                Vec::new()
            }
            Err(e) => return self.notice(&e, true),
        };
        let steps = reaction_steps(&mine, emoji, remove);
        let mut failure = None;
        let mut applied = false;
        for (e, set) in &steps {
            let result = self
                .call(move |g| async move { g.react(chat_id, message_id, e, *set).await })
                .await;
            if let Err(f) = result {
                failure = Some(f);
                break;
            }
            applied = true;
            let key = map::reactor_key(REACTOR_ME, e);
            match self.store.set_reaction(
                NET,
                chat_id,
                message_id,
                &key,
                true,
                set.then_some(e.as_str()),
                now_ms(),
            ) {
                Ok(true) => {}
                // Unknown message: log and drop, never a panic.
                Ok(false) => log::info("teams", "reaction for a message chatcli does not hold"),
                Err(err) => self.notice(&err, true),
            }
        }
        if applied {
            if let Ok(Some(m)) = self.store.message(NET, chat_id, message_id) {
                self.emit(Event::MessageChanged(m));
            }
        }
        if let Some(f) = failure {
            // Which emoji Teams accepts is not documented: its own answer says why.
            self.fail(f, "Could not react", false);
        }
    }

    /// The stored message when it is the user's own and can be edited or deleted; else the
    /// notice that says why not.
    fn own_message(&self, chat_id: &str, message_id: &str, verb: &str) -> Option<ChatMessage> {
        let m = match self.store.message(NET, chat_id, message_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                self.notice(
                    "That message is not stored: reopen the chat and try again.",
                    true,
                );
                return None;
            }
            Err(e) => {
                self.notice(&e, true);
                return None;
            }
        };
        if !m.from_me || m.state != MessageState::Sent {
            self.notice(
                &format!("You can only {verb} your own sent messages."),
                true,
            );
            return None;
        }
        Some(m)
    }

    async fn edit(&mut self, chat_id: &str, message_id: &str, text: &str) {
        if self.me_id.is_none() {
            return self.notice(NOT_SIGNED_IN, true);
        }
        let Some(old) = self.own_message(chat_id, message_id, "edit") else {
            return;
        };
        if text.trim().is_empty() {
            return self.notice(
                "An edited message cannot be empty: delete it instead.",
                true,
            );
        }
        if !text_fits(NET, text) {
            return self.notice(
                "MESSAGE TOO LONG: Teams takes about 80 KB per message.",
                true,
            );
        }
        // The new body replaces the whole old one, so a quote or a file in it would be lost.
        if old.reply_to.is_some() || !old.attachments.is_empty() {
            return self.notice(
                "This message has a quote or a file: edit it in Teams.",
                true,
            );
        }
        // The old mentions go back in, or the people would stop counting as mentioned.
        let built = body::text_body(text, &old.mentions);
        let request = body::edit_json(&built);
        let request = &request;
        if let Err(f) = self
            .call(move |g| async move { g.edit_message(chat_id, message_id, request).await })
            .await
        {
            // The stored text stays as it was: pane and server remain in step.
            return self.fail(f, "Could not edit the message", false);
        }
        let mut m = self
            .store
            .message(NET, chat_id, message_id)
            .ok()
            .flatten()
            .unwrap_or(old);
        m.text = crate::html::to_text(&chatcli_core::limits::teams_escape(text));
        m.edited = true;
        // Only the mentions whose token is still in the text survive the edit.
        m.mentions = built.used;
        m.mentions_me = false;
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&m)) {
            return self.notice(&e, true);
        }
        if let Ok(Some(m)) = self.store.message(NET, chat_id, message_id) {
            self.touch_if_last(chat_id, &m);
            self.emit(Event::MessageChanged(m));
        }
    }

    async fn delete(&mut self, chat_id: &str, message_id: &str) {
        let Some(me) = self.me_id.clone() else {
            return self.notice(NOT_SIGNED_IN, true);
        };
        if self.own_message(chat_id, message_id, "delete").is_none() {
            return;
        }
        let me = me.as_str();
        if let Err(f) = self
            .call(move |g| async move { g.soft_delete(me, chat_id, message_id).await })
            .await
        {
            return self.fail(f, "Could not delete the message", false);
        }
        match apply_deleted(&self.store, chat_id, message_id) {
            Ok(Some(m)) => self.emit(Event::MessageChanged(m)),
            Ok(None) => {}
            Err(e) => self.notice(&e, true),
        }
    }

    /// After an edit of the chat's newest message the sidebar preview follows.
    fn touch_if_last(&self, chat_id: &str, message: &ChatMessage) {
        touch_chat(&self.store, &self.events, chat_id, message, None);
    }

    // --- media -----------------------------------------------------------------------------

    /// Answers every person. Without the setting or the consent nothing touches the network.
    /// Cached photos are answered at once; the rest are fetched one at a time by one task, and
    /// a 429 stops the loop. A failed lookup keeps an older photo. Logs counts only.
    async fn fetch_avatars(&mut self, people: Vec<String>) {
        let answer = |events: &EventTx, id: String, path: Option<PathBuf>| {
            let _ = events.send(Event::Avatar {
                network: NET,
                person_id: id,
                path,
            });
        };
        let asked = people.len();
        if !self.photos_allowed() {
            for id in people {
                answer(&self.events, id, None);
            }
            return;
        }
        let now = now_ms();
        let (mut todo, mut cached) = (Vec::new(), 0usize);
        for id in people {
            if !config::is_guid(&id) {
                answer(&self.events, id, None);
                continue;
            }
            match self.media.avatar_lookup(NET, &id, now) {
                media::AvatarLookup::Fresh(p) => {
                    cached += 1;
                    answer(&self.events, id, Some(p));
                }
                media::AvatarLookup::NoPhoto => {
                    cached += 1;
                    answer(&self.events, id, None);
                }
                media::AvatarLookup::Stale(p) => todo.push((id, Some(p))),
                media::AvatarLookup::Missing => todo.push((id, None)),
            }
        }
        if todo.is_empty() {
            log::info(
                "teams",
                &format!("profile photos: {asked} asked, 0 found, {cached} cached"),
            );
            return;
        }
        let graph = match self.task_graph().await {
            Ok(g) => g,
            Err(_) => {
                for (id, stale) in todo {
                    answer(&self.events, id, stale);
                }
                return;
            }
        };
        let (cache, events) = (self.media.clone(), self.events.clone());
        let gate = self.avatar_gate.clone();
        let gen_now = self.avatar_gen.clone();
        let gen_at = gen_now.load(std::sync::atomic::Ordering::SeqCst);
        self.tasks.spawn(async move {
            let mut gate = gate.lock().await;
            let mut found = 0usize;
            let mut stop = gate.is_some_and(|t| Instant::now() < t);
            for (id, stale) in todo {
                if gen_now.load(std::sync::atomic::Ordering::SeqCst) != gen_at {
                    return;
                }
                if stop {
                    answer(&events, id, stale);
                    continue;
                }
                let got = graph.user_photo(&id).await;
                // Signed out while the request was in flight: write and answer nothing.
                if gen_now.load(std::sync::atomic::Ordering::SeqCst) != gen_at {
                    return;
                }
                let path = match got {
                    Ok(Some(bytes)) => {
                        let (c, i) = (cache.clone(), id.clone());
                        let stored = tokio::task::spawn_blocking(move || {
                            c.store_avatar(NET, &i, &bytes).or_else(|e| {
                                c.store_no_avatar(NET, &i)?;
                                Err(e)
                            })
                        })
                        .await;
                        match stored {
                            Ok(Ok(p)) => {
                                found += 1;
                                Some(p)
                            }
                            _ => None,
                        }
                    }
                    Ok(None) => {
                        let _ = cache.store_no_avatar(NET, &id);
                        None
                    }
                    // Over the size cap counts as no photo.
                    Err(e) if e.status == 413 => {
                        let _ = cache.store_no_avatar(NET, &id);
                        None
                    }
                    Err(e) => {
                        // Throttled, or no network: end the batch, later ones wait a minute.
                        if e.status == 429 || e.status == 0 {
                            stop = true;
                            *gate = Some(Instant::now() + std::time::Duration::from_secs(60));
                        }
                        stale
                    }
                };
                if gen_now.load(std::sync::atomic::Ordering::SeqCst) != gen_at {
                    return;
                }
                answer(&events, id, path);
            }
            // One cache walk per batch, not per photo.
            let c = cache.clone();
            let _ = tokio::task::spawn_blocking(move || c.evict(now_ms())).await;
            log::info(
                "teams",
                &format!("profile photos: {asked} asked, {found} found, {cached} cached"),
            );
        });
    }

    async fn fetch_media(&mut self, chat_id: String, message_id: String, index: u32) {
        let failed = |b: &Backend, reason: String, fallback_url: Option<String>| {
            b.emit(Event::MediaFailed {
                network: NET,
                chat_id: chat_id.clone(),
                message_id: message_id.clone(),
                index,
                reason,
                fallback_url,
            });
        };
        let att = match self.store.message(NET, &chat_id, &message_id) {
            Ok(Some(m)) => m.attachments.into_iter().nth(index as usize),
            Ok(None) => None,
            Err(e) => return failed(self, e, None),
        };
        let Some(att) = att else {
            return failed(self, "That attachment is no longer available.".into(), None);
        };
        let Some(spec) = FetchSpec::from_ref(&att.fetch_ref) else {
            return failed(
                self,
                "That attachment cannot be fetched from Teams.".into(),
                att.open_url,
            );
        };
        let files_allowed = self.caps().fetch_files;
        // A linked file without the scope never touches the network: the browser opens it.
        if matches!(spec, FetchSpec::File { .. }) && !files_allowed {
            return failed(self, transfer::FILE_OPEN_IN_BROWSER.into(), att.open_url);
        }
        let graph = match self.task_graph().await {
            Ok(g) => g,
            Err(reason) => return failed(self, reason, att.open_url),
        };
        let (cache, events) = (self.media.clone(), self.events.clone());
        self.tasks.spawn(async move {
            let event =
                match transfer::fetch_media(&graph, &cache, &att, &spec, files_allowed).await {
                    Ok(path) => Event::MediaReady {
                        network: NET,
                        chat_id,
                        message_id,
                        index,
                        path,
                    },
                    Err(f) => {
                        log::error("teams", "a media download failed");
                        Event::MediaFailed {
                            network: NET,
                            chat_id,
                            message_id,
                            index,
                            reason: f.reason,
                            fallback_url: f.fallback_url,
                        }
                    }
                };
            let _ = events.send(event);
        });
    }

    // --- older history and members ---------------------------------------------------------

    /// Older messages for a user scroll past the top. The store answers first; Teams is asked
    /// only when the store has nothing older, one page per request, following Graph's own next
    /// link. Never from a timer.
    async fn load_older(&mut self, chat_id: &str, before_ts: i64, before_id: &str) {
        let failed = |b: &Backend, error: String| {
            b.emit(Event::OlderFailed {
                network: NET,
                chat_id: chat_id.to_string(),
                error,
            });
        };
        match self
            .store
            .messages_before(NET, chat_id, before_ts, before_id, OLDER_PAGE)
        {
            Ok(rows) if !rows.is_empty() => {
                return self.emit(Event::OlderMessages {
                    network: NET,
                    chat_id: chat_id.to_string(),
                    messages: rows,
                    at_start: false,
                })
            }
            Ok(_) => {}
            Err(e) => return failed(self, e),
        }
        let key = format!("teams.older.{chat_id}");
        let saved = self.store.meta(&key).ok().flatten();
        if saved.as_deref() == Some("start") {
            return self.emit(Event::OlderMessages {
                network: NET,
                chat_id: chat_id.to_string(),
                messages: Vec::new(),
                at_start: true,
            });
        }
        let Some(me) = self.me_id.clone() else {
            return failed(self, NOT_SIGNED_IN.into());
        };
        let next = saved
            .as_deref()
            .and_then(|s| s.strip_prefix("next:"))
            .map(str::to_string);
        let link = next.as_deref();
        let page = match self
            .call(move |g| async move { g.messages_before(chat_id, before_ts, link).await })
            .await
        {
            Ok(page) => page,
            Err(Fail::NeedsLogin(reason)) => {
                failed(self, reason.clone());
                return self.signed_out(Some(reason));
            }
            Err(Fail::Other(message, status)) => {
                // A saved link that Graph no longer accepts is dropped: the next try starts
                // from the filter again.
                if next.is_some() && matches!(status, 400 | 404 | 410) {
                    let _ = self.store.set_meta(&key, None);
                }
                self.notice(&format!("Could not load older messages: {message}"), true);
                return failed(self, message);
            }
        };
        let stored = match store_fetched(&self.store, chat_id, &page.messages, &me) {
            Ok(s) => s,
            Err(e) => return failed(self, e),
        };
        let at_start = page.next_link.is_none();
        let saved = page
            .next_link
            .as_ref()
            .map_or("start".to_string(), |l| format!("next:{l}"));
        if let Err(e) = self.store.set_meta(&key, Some(&saved)) {
            self.notice(&e, true);
        }
        let mut messages: Vec<ChatMessage> = stored.into_iter().map(|s| s.message).collect();
        messages.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        self.emit(Event::OlderMessages {
            network: NET,
            chat_id: chat_id.to_string(),
            messages,
            at_start,
        });
    }

    /// Members for @-completion: a saved list younger than ten minutes, else one listing from
    /// Teams (saved for next time).
    async fn load_members(&mut self, chat_id: &str) {
        let answer = |b: &Backend, members: Vec<PersonRef>, error: Option<String>| {
            b.emit(Event::Members {
                network: NET,
                chat_id: chat_id.to_string(),
                members,
                error,
            });
        };
        let key = format!("teams.members.{chat_id}");
        let saved_at = self
            .store
            .meta(&key)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok());
        if saved_at.is_some_and(|t| now_ms() - t < MEMBERS_FRESH_MS) {
            if let Ok(cached) = self.store.members(NET, chat_id, 1024) {
                if !cached.is_empty() {
                    return answer(self, cached, None);
                }
            }
        }
        let Some(me) = self.me_id.clone() else {
            return answer(self, Vec::new(), Some(NOT_SIGNED_IN.into()));
        };
        let listed = match self
            .call(move |g| async move { g.members(chat_id, MEMBERS_MAX_PAGES).await })
            .await
        {
            Ok(m) => m,
            Err(Fail::NeedsLogin(reason)) => {
                answer(self, Vec::new(), Some(reason.clone()));
                return self.signed_out(Some(reason));
            }
            Err(Fail::Other(message, _)) => {
                return answer(
                    self,
                    Vec::new(),
                    Some(format!("Could not load the members: {message}")),
                )
            }
        };
        let mut people: Vec<PersonRef> = Vec::new();
        for m in &listed {
            let (Some(id), Some(name)) = (m.user_id.as_deref(), m.display_name.as_deref()) else {
                continue;
            };
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            if id == me || name.is_empty() || people.iter().any(|p| p.id == id) {
                continue;
            }
            people.push(PersonRef {
                id: id.to_string(),
                name,
            });
        }
        if let Err(e) = self.store.replace_members(NET, chat_id, &people) {
            self.notice(&e, true);
        }
        if let Err(e) = self.store.set_meta(&key, Some(&now_ms().to_string())) {
            self.notice(&e, true);
        }
        let ordered = self.store.members(NET, chat_id, 1024).unwrap_or(people);
        answer(self, ordered, None);
    }

    // --- read state and chat flags ---------------------------------------------------------

    async fn mark_read(&mut self, chat_id: &str) {
        let before = self
            .store
            .chat(NET, chat_id)
            .ok()
            .flatten()
            .map(|c| (c.unread, c.mentions));
        if let Err(e) = self.store.set_unread(NET, chat_id, 0) {
            self.notice(&e, true);
        }
        self.emit_chat(chat_id);
        let Some(me) = self.me_id.clone() else { return };
        if let Err(f) = self.mark_read_remote(chat_id, &me).await {
            // Teams still thinks the chat is unread, so chatcli shows it unread too.
            if let Some((unread, mentions)) = before {
                let _ = self.store.set_unread(NET, chat_id, unread);
                for _ in 0..mentions {
                    let _ = self.store.bump_mentions(NET, chat_id);
                }
            }
            self.emit_chat(chat_id);
            self.fail(f, "Could not mark the chat read on Teams", false);
        }
    }

    async fn mark_read_remote(&mut self, chat_id: &str, me: &str) -> Outcome<()> {
        // The tenant id comes from the access token's `tid` claim, so get a token first.
        self.token(false).await?;
        let Some(tenant) = self.tenant_id.clone() else {
            return Err(Fail::Other("the access token has no tenant id.".into(), 0));
        };
        let tenant = tenant.as_str();
        self.call(move |g| async move { g.mark_read(chat_id, me, tenant).await })
            .await
    }

    /// Pin and mute are local only. Hide is written to the store first, then to Teams; if Teams
    /// refuses, the flag goes back and the user is told.
    async fn set_chat_flag(&mut self, chat_id: &str, flag: ChatFlag, on: bool) {
        let old = match self.store.chat_flags(NET, chat_id) {
            Ok(f) => f,
            Err(e) => return self.notice(&e, true),
        };
        let mut new = old;
        match flag {
            ChatFlag::Pinned => new.pinned = on,
            ChatFlag::Muted => new.muted_until = on.then_some(MUTE_FOREVER),
            ChatFlag::Hidden => new.hidden = on,
        }
        if let Err(e) = self.store.set_chat_flags(NET, chat_id, new) {
            self.notice(&e, true);
            return self.emit_chat(chat_id);
        }
        self.emit_chat(chat_id);
        if flag != ChatFlag::Hidden {
            return;
        }
        let result = match (self.me_id.clone(), self.token(false).await) {
            (None, _) => Err(Fail::NeedsLogin(NOT_SIGNED_IN.into())),
            (_, Err(f)) => Err(f),
            (Some(me), Ok(_)) => match self.tenant_id.clone() {
                None => Err(Fail::Other("the access token has no tenant id.".into(), 0)),
                Some(tenant) => {
                    let (me, tenant) = (me.as_str(), tenant.as_str());
                    self.call(move |g| async move { g.set_hidden(chat_id, on, me, tenant).await })
                        .await
                }
            },
        };
        if let Err(f) = result {
            let _ = self.store.set_chat_flags(NET, chat_id, old);
            self.emit_chat(chat_id);
            let what = if on {
                "Could not hide the chat on Teams"
            } else {
                "Could not unhide the chat on Teams"
            };
            self.fail(f, what, false);
        } else if on {
            self.hidden.insert(chat_id.to_string());
        } else {
            self.hidden.remove(chat_id);
        }
    }

    // --- live updates: relay socket and subscription ---------------------------------------

    /// Starts the relay socket (when set up and not already running) and the subscription.
    /// Always ends by publishing the Online chip.
    async fn setup_live(&mut self) {
        self.relay_note = None;
        let (url, secrets) = match (config::load(), relay::load_secrets()) {
            (Err(e), _) | (_, Err(e)) => {
                self.relay_note = Some("relay settings unreadable".into());
                self.notice(&format!("Live updates are off: {e}"), true);
                return self.publish_online();
            }
            (Ok(cfg), Ok(secrets)) => (cfg.relay_url, secrets),
        };
        let Some(url) = url else {
            self.relay_note = Some("relay not set up".into());
            return self.publish_online();
        };
        let (Some(secrets), Some(user_id)) = (secrets, self.me_id.clone()) else {
            self.relay_note =
                Some("RELAY SECRETS MISSING: quit chatcli, then run chatcli relay setup".into());
            self.notice(
                "Live updates are off: relay secrets are missing. Quit chatcli, then run: chatcli relay setup <url>",
                true,
            );
            return self.publish_online();
        };
        self.spec = Some(Spec {
            user_id,
            relay_url: url.clone(),
            client_state: secrets.client_state,
        });
        if self.relay.as_ref().is_none_or(|c| c.is_finished()) {
            self.socket_up = false;
            self.socket_note = None;
            self.relay = Some(relay::Client::start(
                &url,
                secrets.token,
                self.relay_tx.clone(),
            ));
        }
        let now = now_ms();
        if self
            .sub
            .as_ref()
            .is_none_or(|s| !s.active(now) || now >= s.renew_at_ms)
        {
            self.ensure_subscription().await;
        }
        self.publish_online();
    }

    /// Creates or renews the subscription. A failure keeps a still-valid old one, shows
    /// Graph's message once, and schedules a retry.
    async fn ensure_subscription(&mut self) {
        let Some(spec) = self.spec.clone() else {
            return;
        };
        let store = self.store.clone();
        let now = now_ms();
        let (spec_ref, store_ref) = (&spec, &store);
        let result = self
            .call(move |g| async move { subscription::ensure(&g, store_ref, spec_ref, now).await })
            .await;
        match result {
            Ok(state) => {
                self.sub = Some(state);
                self.sub_note = None;
                self.retry_at_ms = None;
            }
            Err(Fail::NeedsLogin(reason)) => self.signed_out(Some(reason)),
            Err(Fail::Other(message, _)) => {
                self.retry_at_ms = Some(now_ms() + SUBSCRIPTION_RETRY_MS);
                let note = format!("Teams subscription failed: {message}");
                if self.sub_note.as_deref() != Some(note.as_str()) {
                    self.notice(&format!("Live updates: {note}"), true);
                }
                if self.sub.as_ref().is_none_or(|s| !s.active(now_ms())) {
                    self.sub = None;
                }
                self.sub_note = Some(note);
            }
        }
    }

    /// Deletes the subscription on Graph (best effort) and forgets its id.
    async fn remove_subscription(&mut self, why: &str) {
        let stored = self.store.meta(SUB_ID_KEY).ok().flatten();
        let id = self.sub.as_ref().map(|s| s.id.clone()).or(stored);
        if let Some(id) = id {
            if self.me_id.is_some() {
                let store = self.store.clone();
                let (id_ref, store_ref) = (id.as_str(), &store);
                let call =
                    self.call(
                        move |g| async move { subscription::remove(&g, store_ref, id_ref).await },
                    );
                match tokio::time::timeout(SHUTDOWN_CLEANUP, call).await {
                    Ok(Ok(())) => log::info("teams", &format!("subscription deleted ({why})")),
                    Ok(Err(Fail::Other(m, _) | Fail::NeedsLogin(m))) => {
                        log::error("teams", &format!("subscription not deleted ({why}): {m}"))
                    }
                    Err(_) => {
                        log::error("teams", &format!("subscription delete timed out ({why})"))
                    }
                }
            }
        }
        // Gone or not, the id must not outlive this run: an orphan is cleaned at next create.
        let _ = self.store.set_meta(SUB_ID_KEY, None);
        self.sub = None;
    }

    fn next_deadline(&self) -> Instant {
        let far = Instant::now() + Duration::from_secs(24 * 60 * 60);
        if self.spec.is_none() {
            return far;
        }
        match next_upkeep_ms(self.sub.as_ref(), self.retry_at_ms) {
            Some(at) => Instant::now() + Duration::from_millis((at - now_ms()).max(0) as u64),
            None => far,
        }
    }

    async fn upkeep(&mut self) {
        // Make sure a failure moves the deadline, so this can never spin.
        self.retry_at_ms = Some(now_ms() + SUBSCRIPTION_RETRY_MS);
        self.ensure_subscription().await;
        self.publish_online();
    }

    async fn relay_event(&mut self, event: RelayEvent) {
        // Events queued by a client that teardown_live already dropped must not revive state.
        if self.relay.is_none() {
            return;
        }
        match event {
            RelayEvent::Up => {
                self.socket_up = true;
                self.socket_note = None;
                // A laptop that slept may have outlived its subscription.
                let now = now_ms();
                if self
                    .sub
                    .as_ref()
                    .is_none_or(|s| !s.active(now) || now >= s.renew_at_ms)
                {
                    self.ensure_subscription().await;
                }
                self.publish_online();
            }
            RelayEvent::Down(reason) => {
                self.socket_up = false;
                self.socket_note = Some(format!("relay offline: {reason}"));
                self.publish_online();
            }
            RelayEvent::Frame(frame) => self.frame(frame).await,
        }
    }

    async fn frame(&mut self, frame: Frame) {
        match frame {
            Frame::Hello { dropped } => {
                log::info(
                    "relay",
                    &format!("connected, {dropped} notification(s) dropped while away"),
                );
                // Every (re)connect catches up, whether or not the Worker counted drops: the
                // socket may also have been down before the Worker noticed.
                self.catch_up().await;
            }
            Frame::Notify { items } => self.notifications(items).await,
            Frame::Lifecycle { items } => {
                for item in items {
                    self.lifecycle(item).await;
                }
            }
            Frame::Alert { text } => self.notice(&format!("Relay alert: {text}"), true),
        }
        self.publish_online();
    }

    async fn lifecycle(&mut self, item: LifecycleItem) {
        let ours = self.sub.as_ref().map(|s| s.id.clone());
        if ours.as_deref() != Some(item.subscription_id.as_str()) {
            log::info(
                "relay",
                &format!(
                    "lifecycle '{}' for a subscription that is not ours",
                    item.lifecycle_event
                ),
            );
            return;
        }
        let id = item.subscription_id;
        match item.lifecycle_event.as_str() {
            "reauthorizationRequired" => {
                let now = now_ms();
                let id_ref = id.as_str();
                match self
                    .call(move |g| async move { subscription::reauthorize(&g, id_ref, now).await })
                    .await
                {
                    Ok(state) => {
                        self.sub = Some(state);
                        self.sub_note = None;
                    }
                    Err(f) => {
                        self.retry_at_ms = Some(now_ms() + SUBSCRIPTION_RETRY_MS);
                        self.fail(f, "Could not reauthorize the Teams subscription", false);
                    }
                }
            }
            "subscriptionRemoved" => {
                log::info("relay", "subscription removed by Graph, creating a new one");
                let _ = self.store.set_meta(SUB_ID_KEY, None);
                self.sub = None;
                self.ensure_subscription().await;
            }
            "missed" => self.catch_up().await,
            other => log::info("relay", &format!("ignored lifecycle event '{other}'")),
        }
    }

    // --- change notifications --------------------------------------------------------------

    async fn notifications(&mut self, items: Vec<Notification>) {
        if self.me_id.is_none() {
            return;
        }
        let mut seen = HashSet::new();
        let mut relisted = false;
        for item in items {
            let Some(r) = relay::parse_resource(&item.resource) else {
                log::error("relay", "notification with a resource chatcli cannot read");
                continue;
            };
            if !seen.insert((
                r.chat_id.clone(),
                r.message_id.clone(),
                item.change_type.clone(),
            )) {
                continue;
            }
            if self.not_shown.contains(&r.chat_id) {
                continue;
            }
            if self.hidden.contains(&r.chat_id) && item.change_type != "created" {
                continue;
            }
            if item.change_type == "deleted" {
                self.mark_deleted(&r.chat_id, &r.message_id);
            } else {
                let created = item.change_type == "created";
                self.live_message(&r.chat_id, &r.message_id, created, &mut relisted)
                    .await;
            }
        }
    }

    async fn live_message(
        &mut self,
        chat_id: &str,
        message_id: &str,
        created: bool,
        relisted: &mut bool,
    ) {
        let mut counted_by_list = false;
        // A chat chatcli has not seen, or one the user hid that a new message just unhid on
        // Teams: list again once, so the flag and the chat are current before the fetch.
        let stale = !matches!(self.store.chat(NET, chat_id), Ok(Some(_)))
            || (created && self.hidden.contains(chat_id));
        if stale {
            if !*relisted {
                *relisted = true;
                if let Err(f) = self.refresh_chats().await {
                    return self.fail(f, "Could not load chats", false);
                }
                counted_by_list = true;
            }
            if !matches!(self.store.chat(NET, chat_id), Ok(Some(_))) {
                // Still unknown after the re-list: remember it, so its messages do not trigger
                // a list each.
                self.not_shown.insert(chat_id.to_string());
                return;
            }
        }
        let me = self.me_id.clone().unwrap_or_default();
        let fetched = match self
            .call(move |g| async move { g.message(chat_id, message_id).await })
            .await
        {
            Ok(m) => m,
            // Deleted again before we looked: nothing to show.
            Err(Fail::Other(m, 404)) => {
                return log::info("teams", &format!("new message already gone: {m}"))
            }
            Err(f) => return self.fail(f, "Could not load a new message", false),
        };
        let stored = match store_fetched(&self.store, chat_id, std::slice::from_ref(&fetched), &me)
        {
            Ok(mut s) if !s.is_empty() => s.remove(0),
            Ok(_) => return log::error("teams", "a new message had no timestamp and was skipped"),
            Err(e) => return self.notice(&e, true),
        };
        let message = stored.message;
        // Message is only for one that just arrived; an edit, delete or reaction is a change
        // and never raises a notification.
        let is_new = created && !stored.existed;
        if is_new {
            self.emit(Event::Message(message.clone()));
        } else {
            self.emit(Event::MessageChanged(message.clone()));
        }
        if is_new && !message.from_me && message.mentions_me {
            if let Err(e) = self.store.bump_mentions(NET, chat_id) {
                self.notice(&e, true);
            }
        }
        let unread = (!counted_by_list).then(|| {
            let current = self
                .store
                .chat(NET, chat_id)
                .ok()
                .flatten()
                .map_or(0, |c| c.unread);
            map::unread_after(current, message.from_me, is_new)
        });
        touch_chat(&self.store, &self.events, chat_id, &message, unread);
    }

    fn mark_deleted(&self, chat_id: &str, message_id: &str) {
        match apply_deleted(&self.store, chat_id, message_id) {
            Ok(Some(m)) => self.emit(Event::MessageChanged(m)),
            Ok(None) => log::info("teams", "delete notice for a message chatcli never stored"),
            Err(e) => self.notice(&e, true),
        }
    }

    /// After a reconnect or a `missed` event: list chats again and fetch the messages of every
    /// chat that has stored history and either a newer last message on Teams or a last message
    /// that was edited or deleted while chatcli was away.
    async fn catch_up(&mut self) {
        if self.me_id.is_none() {
            return;
        }
        let fresh = match self.refresh_chats().await {
            Ok(f) => f,
            Err(f) => return self.fail(f, "Could not catch up on Teams", false),
        };
        let store = self.store.clone();
        let (ids, capped) = map::catch_up_chats(
            &fresh.shown,
            &fresh.last,
            |id, last_id| Local {
                newest_ts: store.newest_synced_ts(NET, id).ok().flatten(),
                last: last_id.and_then(|m| store.message(NET, id, m).ok().flatten()),
            },
            CATCH_UP_MAX_CHATS,
        );
        let mut filled = 0;
        let mut gaps = 0;
        for id in &ids {
            match self.fetch_history(id).await {
                Ok(h) => {
                    filled += h.new.len();
                    gaps += usize::from(h.gap);
                    // Missed messages arrive like live ones (the TUI never toasts an old one);
                    // edits, deletes and reactions are changes.
                    for m in h.new {
                        self.emit(Event::Message(m));
                    }
                    for m in h.changed {
                        self.emit(Event::MessageChanged(m));
                    }
                    self.emit_chat(id);
                }
                Err(f) => return self.fail(f, "Could not catch up on Teams", false),
            }
        }
        log::info(
            "teams",
            &format!(
                "catch-up: gaps filled {filled} message(s) in {} chat(s)",
                ids.len()
            ),
        );
        if gaps > 0 {
            self.notice(
                &format!("{gaps} chat(s) had {FETCH_MESSAGES} or more new messages: the oldest of them may be missing from chatcli."),
                true,
            );
        }
        if capped {
            self.notice(
                &format!("Catch-up covered {CATCH_UP_MAX_CHATS} chats; open the others to load their latest messages."),
                false,
            );
        }
    }

    async fn shutdown(&mut self) {
        self.relay = None;
        // Uploads and downloads stop at once; a half-written cache file is removed on drop.
        self.tasks.abort_all();
        let _ = tokio::time::timeout(SHUTDOWN_CLEANUP, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        self.remove_subscription("shutdown").await;
        self.teardown_live();
    }
}

/// What a catch-up or an opened chat found.
struct History {
    /// Messages that arrived since the newest stored one.
    new: Vec<ChatMessage>,
    /// Stored messages that changed (edit, delete, reaction) or were missing from the middle.
    changed: Vec<ChatMessage>,
    /// Every fetched message was new and the fetch was full: older ones may be missing.
    gap: bool,
}

/// The Graph calls that make `emoji` the user's one reaction (or remove it): every other
/// reaction of theirs is unset first, then the emoji is set unless it already is. `true` = set.
fn reaction_steps(mine: &[String], emoji: &str, remove: bool) -> Vec<(String, bool)> {
    if remove {
        return vec![(emoji.to_string(), false)];
    }
    let mut steps: Vec<(String, bool)> = mine
        .iter()
        .filter(|e| e.as_str() != emoji)
        .map(|e| (e.clone(), false))
        .collect();
    if !mine.iter().any(|e| e == emoji) {
        steps.push((emoji.to_string(), true));
    }
    steps
}

#[cfg(test)]
mod tests;
