//! The Teams backend task: one tokio task that owns sign-in, Graph, the relay socket and the
//! Graph subscription, and talks to the TUI only through `Command` and `Event`.
//!
//! No timers call Graph to poll Teams (Microsoft allows at most one poll a day). Chats are
//! listed at start, on the user's Refresh, and in a catch-up after a relay reconnect; messages
//! are fetched when the user opens a chat or a change notification names one. The only timer
//! is subscription upkeep (renew at half-life, retry after a failure), which is not polling.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use chatcli_core::model::{
    now_ms, ChatMessage, ChatSummary, Command, CommandRx, Event, EventTx, MessageState, Network,
    Status,
};
use chatcli_core::store::Store;
use chatcli_core::{config, log};

use crate::auth::{self, AuthFailure};
use crate::graph::{ApiResult, Graph};
use crate::map::{self, TOKEN_MARGIN_MS};
use crate::relay::{self, Frame, LifecycleItem, Notification, RelayEvent};
use crate::subscription::{self, Spec, SubState, SUB_ID_KEY};

const NET: Network = Network::Teams;
/// 20 pages of 50 = 1000 chats. More is reported, never dropped silently.
const MAX_CHAT_PAGES: u32 = 20;
/// Messages fetched from Graph when a chat opens or a gap is filled.
const FETCH_MESSAGES: u32 = 50;
/// Messages shown from the local store.
const STORE_MESSAGES: u32 = 200;
/// Chats fetched in one catch-up, so a long absence cannot trigger a request storm.
const CATCH_UP_MAX_CHATS: usize = 25;
/// After a failed subscription create or renew, try again after this long.
const SUBSCRIPTION_RETRY_MS: i64 = 10 * 60 * 1000;
const SHUTDOWN_CLEANUP: Duration = Duration::from_secs(5);

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
    events: EventTx,
    http: reqwest::Client,
    relay_tx: mpsc::UnboundedSender<RelayEvent>,
    token: Option<CachedToken>,
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
    /// notification for one is followed up (re-list, then fetch) instead of dropped.
    hidden: HashSet<String>,
    last_status: Option<Status>,
}

pub async fn run(store: Store, mut commands: CommandRx, events: EventTx) {
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
    let mut b = Backend::new(store, events, http, relay_tx);
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
            _ = tokio::time::sleep_until(deadline) => b.upkeep().await,
        }
    }
    b.shutdown().await;
}

impl Backend {
    fn new(
        store: Store,
        events: EventTx,
        http: reqwest::Client,
        relay_tx: mpsc::UnboundedSender<RelayEvent>,
    ) -> Self {
        Backend {
            store,
            events,
            http,
            relay_tx,
            token: None,
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

    // --- tokens and Graph calls ------------------------------------------------------------

    fn set_token(&mut self, token: auth::Token) {
        if let Some(tid) = map::tid_from_jwt(&token.access) {
            self.tenant_id = Some(tid);
        }
        let expires_at_ms =
            now_ms() + i64::try_from(token.expires_in_secs).unwrap_or(i64::MAX / 2_000) * 1000;
        self.token = Some(CachedToken {
            access: token.access,
            expires_at_ms,
        });
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
        match auth::refresh(&self.http).await {
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
        match f(Graph::new(self.http.clone(), token)).await {
            Ok(v) => Ok(v),
            Err(e) if e.status == 401 => {
                log::info("teams", "Graph answered 401, refreshing the token once");
                let token = self.token(true).await?;
                f(Graph::new(self.http.clone(), token)).await.map_err(|e| {
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

    /// Back to the sign-in panel, with Microsoft's reason when there is one.
    fn signed_out(&mut self, reason: Option<String>) {
        self.teardown_live();
        self.token = None;
        self.me_id = None;
        self.tenant_id = None;
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
        self.me_id = Some(me.id);
        if let Err(f) = self.refresh_chats().await {
            return self.fail(f, "Could not load chats", true);
        }
        self.setup_live().await;
    }

    /// Lists every chat, keeps 1:1 and group chats that are not hidden, replaces the store's
    /// copy and sends the sidebar. Returns the fresh summaries.
    async fn refresh_chats(&mut self) -> Outcome<Vec<ChatSummary>> {
        let page = self
            .call(|g| async move { g.chats_all(MAX_CHAT_PAGES).await })
            .await?;
        let me = self.me_id.clone().unwrap_or_default();
        let mut shown = Vec::new();
        self.not_shown.clear();
        self.hidden.clear();
        for chat in &page.chats {
            match map::chat_summary(chat, &me) {
                Some(summary) => shown.push(summary),
                None if map::is_hidden_chat(chat) => {
                    self.hidden.insert(chat.id.clone());
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
        self.emit_chats();
        Ok(shown)
    }

    async fn login(&mut self, commands: &mut CommandRx) -> bool {
        self.teardown_live();
        self.emit_status(Status::LoggingIn);
        let events = self.events.clone();
        let http = self.http.clone();
        let sign_in = auth::sign_in_token(&http, move |url| {
            let _ = events.send(Event::Notice {
                network: NET,
                text: format!("If no browser opened, sign in at: {url}"),
                is_error: false,
            });
        });
        tokio::pin!(sign_in);
        let result = loop {
            tokio::select! {
                result = &mut sign_in => break result,
                cmd = commands.recv() => {
                    // Quit stays possible while the browser is open; anything else waits.
                    if matches!(cmd, None | Some(Command::Shutdown)) {
                        return true;
                    }
                }
            }
        };
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
        self.token = None;
        self.me_id = None;
        self.tenant_id = None;
        self.emit_status(Status::NeedsLogin { reason: None });
    }

    // --- commands --------------------------------------------------------------------------

    /// Returns true when the task should end.
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
            } => self.send(&chat_id, &text, &client_id).await,
            Command::MarkRead { chat_id } => self.mark_read(&chat_id).await,
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

    /// Stores the newest messages of a chat. Returns how many are newer than what was stored.
    /// The flag is true when every fetched message is newer than the stored ones and the
    /// fetch was full, so older messages in between may be missing.
    async fn fetch_history(&mut self, chat_id: &str) -> Outcome<(usize, bool)> {
        let newest = self.newest_stored(chat_id);
        let me = self.me_id.clone().unwrap_or_default();
        let fetched = self
            .call(move |g| async move { g.messages(chat_id, FETCH_MESSAGES).await })
            .await?;
        let mapped: Vec<ChatMessage> = fetched
            .iter()
            .filter_map(|m| map::map_message(m, chat_id, &me))
            .collect();
        if mapped.len() < fetched.len() {
            log::error("teams", "some messages had no timestamp and were skipped");
        }
        if let Err(e) = self.store.upsert_messages(&mapped) {
            self.notice(&e, true);
        }
        let newer = mapped
            .iter()
            .filter(|m| newest.is_none_or(|n| m.ts > n))
            .count();
        let gap =
            newest.is_some() && fetched.len() >= FETCH_MESSAGES as usize && newer == mapped.len();
        Ok((newer, gap))
    }

    /// Newest message Teams itself reported: local failed sends are left out, see the store.
    fn newest_stored(&self, chat_id: &str) -> Option<i64> {
        self.store.newest_synced_ts(NET, chat_id).ok().flatten()
    }

    async fn send(&mut self, chat_id: &str, text: &str, client_id: &str) {
        let me = self.me_id.clone().unwrap_or_default();
        let result = if self.me_id.is_none() {
            Err(Fail::NeedsLogin(
                "Not signed in to Teams. Sign in first.".into(),
            ))
        } else {
            self.call(move |g| async move { g.send_text(chat_id, text).await })
                .await
        };
        match result {
            Ok(sent) => {
                // Graph's answer has the real id and time; without a time, use ours.
                let mut message =
                    map::map_message(&sent, chat_id, &me).unwrap_or_else(|| ChatMessage {
                        network: NET,
                        chat_id: chat_id.to_string(),
                        id: sent.id.clone(),
                        ts: now_ms(),
                        sender: "You".into(),
                        from_me: true,
                        text: text.to_string(),
                        state: MessageState::Sent,
                    });
                message.from_me = true;
                if let Err(e) = self.store.confirm_message(client_id, &message) {
                    self.notice(&e, true);
                }
                self.emit(Event::MessageConfirmed {
                    client_id: client_id.to_string(),
                    message: message.clone(),
                });
                self.touch_chat(chat_id, &message, None);
            }
            Err(f) => {
                let (error, status_failure) = match f {
                    Fail::NeedsLogin(reason) => (reason.clone(), Some(Fail::NeedsLogin(reason))),
                    Fail::Other(message, _) => (message, None),
                };
                log::error("teams-send", &format!("send failed: {error}"));
                // Keep the failed message so it still shows as NOT SENT after reopening.
                let failed = ChatMessage {
                    network: NET,
                    chat_id: chat_id.to_string(),
                    id: client_id.to_string(),
                    ts: now_ms(),
                    sender: "You".into(),
                    from_me: true,
                    text: text.to_string(),
                    state: MessageState::Failed(error.clone()),
                };
                if let Err(e) = self.store.upsert_messages(&[failed]) {
                    self.notice(&e, true);
                }
                self.emit(Event::MessageFailed {
                    network: NET,
                    chat_id: chat_id.to_string(),
                    client_id: client_id.to_string(),
                    error,
                });
                if let Some(f) = status_failure {
                    self.fail(f, "Could not send", false);
                }
            }
        }
    }

    async fn mark_read(&mut self, chat_id: &str) {
        if let Err(e) = self.store.set_unread(NET, chat_id, 0) {
            self.notice(&e, true);
        }
        self.emit_chat(chat_id);
        let Some(me) = self.me_id.clone() else { return };
        // The tenant id comes from the access token's `tid` claim, so get a token first.
        if let Err(f) = self.token(false).await {
            return self.fail(f, "Could not mark the chat read on Teams", false);
        }
        let Some(tenant) = self.tenant_id.clone() else {
            return self.notice(
                "Could not mark the chat read on Teams: the access token has no tenant id.",
                true,
            );
        };
        let (me, tenant) = (me.as_str(), tenant.as_str());
        if let Err(f) = self
            .call(move |g| async move { g.mark_read(chat_id, me, tenant).await })
            .await
        {
            self.fail(f, "Could not mark the chat read on Teams", false);
        }
    }

    /// Moves a chat's last-message fields forward for one message and sends ChatUpdated.
    /// `unread`: the new count, or None to leave it.
    fn touch_chat(&self, chat_id: &str, message: &ChatMessage, unread: Option<u32>) {
        let Ok(Some(mut chat)) = self.store.chat(NET, chat_id) else {
            return;
        };
        if message.ts >= chat.last_ts.unwrap_or(i64::MIN) {
            chat.last_ts = Some(message.ts);
            chat.last_preview = map::preview_line(&message.sender, message.from_me, &message.text);
        }
        if let Some(n) = unread {
            chat.unread = n;
        }
        if let Err(e) = self.store.upsert_chats(std::slice::from_ref(&chat)) {
            self.notice(&e, true);
        }
        self.emit(Event::ChatUpdated(chat));
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
            self.relay_note = Some("relay secrets missing: run chatcli relay setup".into());
            self.notice(
                "Live updates are off: relay secrets are missing. Run: chatcli relay setup <url>",
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
        if !matches!(self.store.chat(NET, chat_id), Ok(Some(_))) {
            // A chat chatcli has not seen: list again once, and remember ids that stay
            // unknown so their messages do not trigger a list each.
            if !*relisted {
                *relisted = true;
                if let Err(f) = self.refresh_chats().await {
                    return self.fail(f, "Could not load chats", false);
                }
                counted_by_list = true;
            }
            if !matches!(self.store.chat(NET, chat_id), Ok(Some(_))) {
                // Still hidden after the re-list: leave it in `hidden`, so the next "created"
                // notification looks again.
                if !self.hidden.contains(chat_id) {
                    self.not_shown.insert(chat_id.to_string());
                }
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
        let Some(message) = map::map_message(&fetched, chat_id, &me) else {
            return log::error("teams", "a new message had no timestamp and was skipped");
        };
        let existed = self
            .store
            .messages(NET, chat_id, STORE_MESSAGES)
            .is_ok_and(|all| all.iter().any(|m| m.id == message.id));
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&message)) {
            self.notice(&e, true);
        }
        self.emit(Event::Message(message.clone()));
        let unread = (!counted_by_list).then(|| {
            let current = self
                .store
                .chat(NET, chat_id)
                .ok()
                .flatten()
                .map_or(0, |c| c.unread);
            map::unread_after(current, message.from_me, created && !existed)
        });
        self.touch_chat(chat_id, &message, unread);
    }

    fn mark_deleted(&self, chat_id: &str, message_id: &str) {
        let found = self
            .store
            .messages(NET, chat_id, STORE_MESSAGES)
            .ok()
            .and_then(|all| all.into_iter().find(|m| m.id == message_id));
        let Some(mut message) = found else {
            return log::info("teams", "delete notice for a message chatcli never stored");
        };
        message.state = MessageState::Deleted;
        message.text = "[deleted]".into();
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&message)) {
            return self.notice(&e, true);
        }
        self.emit(Event::Message(message));
    }

    /// After a reconnect or a `missed` event: list chats again and fetch the messages of
    /// every chat that has stored history and a newer last message on Teams.
    async fn catch_up(&mut self) {
        if self.me_id.is_none() {
            return;
        }
        let fresh = match self.refresh_chats().await {
            Ok(f) => f,
            Err(f) => return self.fail(f, "Could not catch up on Teams", false),
        };
        let (ids, capped) =
            map::catch_up_chats(&fresh, |id| self.newest_stored(id), CATCH_UP_MAX_CHATS);
        let mut filled = 0;
        let mut gaps = 0;
        for id in &ids {
            match self.fetch_history(id).await {
                Ok((n, gap)) => {
                    filled += n;
                    gaps += usize::from(gap);
                    self.emit_history(id);
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
        self.remove_subscription("shutdown").await;
        self.teardown_live();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_only_when_relay_socket_and_subscription_are_all_up() {
        assert_eq!(live_state(None, true, None, true, None), (true, None));
        let not_live = |args: (Option<&str>, bool, Option<&str>, bool, Option<&str>)| {
            live_state(args.0, args.1, args.2, args.3, args.4)
        };
        assert_eq!(
            not_live((Some("relay not set up"), true, None, true, None)),
            (false, Some("relay not set up".into()))
        );
        assert_eq!(
            not_live((None, false, Some("relay offline: timed out"), true, None)),
            (false, Some("relay offline: timed out".into()))
        );
        assert_eq!(
            not_live((None, false, None, true, None)),
            (false, Some("connecting to relay".into()))
        );
        assert_eq!(
            not_live((
                None,
                true,
                None,
                false,
                Some("Teams subscription failed: x")
            )),
            (false, Some("Teams subscription failed: x".into()))
        );
        assert_eq!(
            not_live((None, true, None, false, None)),
            (false, Some("no Teams subscription yet".into()))
        );
    }

    /// An unconfigured install: the task announces itself, shows the cached chats, asks for
    /// setup (never touching the credential store) and ends on Shutdown.
    #[tokio::test]
    async fn unconfigured_start_asks_for_setup_and_shuts_down() {
        let dir = std::env::temp_dir().join(format!("chatcli-backend-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CHATCLI_DATA_DIR", &dir);
        let store = Store::open_in_memory().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(store, cmd_rx, ev_tx));
        let mut seen = Vec::new();
        while seen.len() < 3 {
            seen.push(
                tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(seen[0], Event::Status(Network::Teams, Status::Starting));
        assert_eq!(seen[1], Event::Chats(Network::Teams, vec![]));
        let Event::Status(
            _,
            Status::NeedsLogin {
                reason: Some(reason),
            },
        ) = &seen[2]
        else {
            panic!("expected NeedsLogin with a reason, got {:?}", seen[2]);
        };
        assert!(reason.contains(auth::NOT_SET_UP), "{reason}");
        cmd_tx.send(Command::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_backend() -> (Backend, mpsc::UnboundedReceiver<Event>) {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (relay_tx, _relay_rx) = mpsc::unbounded_channel();
        let http = reqwest::Client::new();
        (
            Backend::new(Store::open_in_memory().unwrap(), ev_tx, http, relay_tx),
            ev_rx,
        )
    }

    #[tokio::test]
    async fn sign_in_needed_is_not_overwritten_by_online_status() {
        let (mut b, mut ev) = test_backend();
        b.me_id = Some("u".into());
        b.signed_out(Some("password changed".into()));
        while ev.try_recv().is_ok() {}
        // What upkeep and frame do after a rejected refresh, and a stale relay event.
        b.upkeep().await;
        b.relay_event(RelayEvent::Up).await;
        b.publish_online();
        assert!(ev.try_recv().is_err(), "nothing may follow NeedsLogin");
        assert!(matches!(b.last_status, Some(Status::NeedsLogin { .. })));
    }

    #[test]
    fn upkeep_runs_at_renew_time_unless_a_retry_is_pending() {
        let sub = SubState {
            id: "s".into(),
            expires_ms: 1000,
            renew_at_ms: 500,
        };
        assert_eq!(next_upkeep_ms(Some(&sub), None), Some(500));
        assert_eq!(next_upkeep_ms(Some(&sub), Some(900)), Some(900));
        assert_eq!(next_upkeep_ms(None, Some(900)), Some(900));
        assert_eq!(next_upkeep_ms(None, None), None);
    }
}
