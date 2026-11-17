//! The 0.2 command handling: react, edit, delete, media (fetch and send), older history from the
//! phone, typing, members, chat flags, and the changes the network reports about stored messages.
//!
//! A child module of `backend`, so it works on the same `Backend` state. The rule for anything
//! slow: the network call runs on its own task and answers with a `Done` message; the store write
//! and the event happen back on the one task that owns the store.

use std::future::Future;
use std::io::Write;
use std::sync::Mutex;

use chatcli_core::limits;
use chatcli_core::media::{kind_for_path, prepare_image, safe_extension, AvatarLookup};
use chatcli_core::model::{MediaKind, MUTE_FOREVER};
use chatcli_core::store::REACTOR_ME;
use sha2::{Digest, Sha256};
use whatsapp_rust::wacore::types::events::ChatPresenceUpdate;
use whatsapp_rust::wacore::types::presence::{ChatPresence, ChatPresenceMedia};

use super::*;
use crate::wa::{FetchError, Member, Progress, UploadInput};

const MIB: u64 = 1024 * 1024;
/// One page of older messages, from the store or from the phone.
const OLDER_PAGE: u32 = 50;
/// How long the phone gets to answer an older-history request. One shot per user request.
const OLDER_WAIT: Duration = Duration::from_secs(30);
/// A whole profile photo lookup (the request and the download) gets this long.
const AVATAR_WAIT: Duration = Duration::from_secs(40);
/// At most one "typing" stanza per chat in this time.
const TYPING_EVERY: Duration = Duration::from_secs(5);
/// A typing stanza is one socket write: give up quickly rather than hold the command loop.
const TYPING_TIMEOUT: Duration = Duration::from_secs(5);
/// A big upload or download may take minutes; a dead one must not hang for ever.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Progress lines reach the TUI at most this often (4 a second).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

pub(super) const MEDIA_EXPIRED: &str = "MEDIA EXPIRED: ask them to send it again.";
const NO_ADDRESS: &str = "This chat has no valid WhatsApp address. Press r to refresh.";

impl Backend {
    // -- plumbing ---------------------------------------------------------------------------

    pub(super) fn wa(&self) -> Option<Arc<dyn Wa>> {
        self.link.as_ref().map(|l| l.wa.clone())
    }

    /// Runs `fut` on its own task and delivers its answer to `on_done`.
    pub(super) fn spawn_done(&mut self, fut: impl Future<Output = Done> + Send + 'static) {
        let tx = self.tx.clone();
        self.tasks.spawn(async move {
            let _ = tx.send(Msg::Done(fut.await));
        });
    }

    /// Frees the results of tasks that finished.
    pub(super) fn reap(&mut self) {
        while self.tasks.try_join_next().is_some() {}
    }

    /// Shutdown: aborts uploads, downloads and timers and waits (at most 5 s) for them to end.
    pub(super) async fn stop_tasks(&mut self) {
        self.tasks.abort_all();
        let tasks = &mut self.tasks;
        if tokio::time::timeout(TASK_STOP, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            log::error(
                AREA,
                "background transfers did not stop in 5 s; dropped them",
            );
        }
    }

    fn chat_updated(&self, chat_id: &str) {
        if let Ok(Some(c)) = self.store.chat(NET, chat_id) {
            self.emit(Out::ChatUpdated(c));
        }
    }

    /// The stored id of a chat the network names by `jid`: the phone-number form when the hidden
    /// (LID) address has a known one, as everywhere else.
    pub(super) fn canonical_id(&self, jid: &Jid) -> String {
        let jid = jid.to_non_ad();
        if jid.is_lid() {
            if let Some(pn) = self.names.linked(&jid.user) {
                return Jid::pn(pn).to_string();
            }
        }
        jid.to_string()
    }

    /// Reads the full message, lets `change` edit it (it returns false for "nothing to do"),
    /// writes it back and emits `MessageChanged` with the message as the store now reads it.
    /// Never builds a message from scratch: that would wipe its attachments, quote and mentions.
    /// The chat's sidebar line follows when the changed message is the newest one (by id, not by
    /// time: two messages can share a second).
    pub(super) fn change_message(
        &mut self,
        chat_id: &str,
        id: &str,
        change: impl FnOnce(&mut ChatMessage) -> bool,
    ) -> Option<ChatMessage> {
        let mut m = match self.store.message(NET, chat_id, id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                log::info(AREA, "a change to a message that is not stored was ignored");
                return None;
            }
            Err(e) => {
                self.store_failed("read message", &e);
                return None;
            }
        };
        if !change(&mut m) {
            return None;
        }
        if let Err(e) = self.store.upsert_messages(std::slice::from_ref(&m)) {
            self.store_failed("update message", &e);
            return None;
        }
        let newest = self
            .store
            .messages(NET, chat_id, 1)
            .ok()
            .and_then(|mut v| v.pop());
        if newest.is_some_and(|n| n.id == m.id) {
            if let Ok(Some(mut c)) = self.store.chat(NET, chat_id) {
                let group = chat_id.parse::<Jid>().is_ok_and(|j| j.is_group());
                c.last_preview = map::preview(&m.sender, map::preview_text(&m), group || m.from_me);
                if self.store.upsert_chats(std::slice::from_ref(&c)).is_ok() {
                    self.emit(Out::ChatUpdated(c));
                }
            }
        }
        let fresh = match self.store.message(NET, chat_id, id) {
            Ok(Some(f)) => f,
            _ => m,
        };
        self.emit(Out::MessageChanged(fresh.clone()));
        Some(fresh)
    }

    // -- capabilities and account -----------------------------------------------------------

    pub(super) fn caps(&self) -> Capabilities {
        Capabilities {
            reply: true,
            react: true,
            edit: true,
            delete: true,
            mentions: true,
            send_image: true,
            send_file: true,
            fetch_media: true,
            // Teams files only.
            fetch_files: false,
            load_older: true,
            // The setting gates what the TUI sends; the library can always do it.
            typing: true,
            pin: true,
            mute: true,
            hide: true,
        }
    }

    pub(super) fn emit_caps(&self) {
        self.emit(Out::Capabilities {
            network: NET,
            caps: self.caps(),
        });
    }

    /// Ties the cached chats to the linked account. A different account on the same cache would
    /// show the old account's messages as "You" and its names on new contacts.
    pub(super) fn bind_account(&mut self) {
        let Some(pn) = self.names.own_pn().map(str::to_string) else {
            return;
        };
        match self.store.bind_account(NET, &pn) {
            Ok(true) => {
                log::info(
                    AREA,
                    "a different WhatsApp account is linked: cached chats cleared",
                );
                let own = self.link.as_ref().map(|l| l.wa.own_users());
                self.names = Names::default();
                if let Some(own) = own {
                    self.names.set_own(own);
                }
                self.older.clear();
                self.emit_chats();
            }
            Ok(false) => {}
            Err(e) => self.store_failed("check linked account", &e),
        }
    }

    // -- results of spawned tasks -----------------------------------------------------------

    pub(super) async fn on_done(&mut self, d: Done) {
        match d {
            Done::Reacted {
                chat_id,
                message_id,
                emoji,
                result,
            } => match result {
                Ok(()) => self.apply_reaction(
                    &chat_id,
                    &message_id,
                    REACTOR_ME,
                    true,
                    emoji.as_deref().unwrap_or(""),
                    now_ms(),
                ),
                Err(e) => self.notice(
                    &format!("WhatsApp did not accept the reaction ({e}). Try again."),
                    true,
                ),
            },
            Done::Edited {
                chat_id,
                message_id,
                text,
                mentions,
                result,
            } => match result {
                Ok(()) => {
                    self.change_message(&chat_id, &message_id, |m| {
                        m.text = text;
                        m.mentions = mentions;
                        m.edited = true;
                        true
                    });
                }
                // The store is untouched, so the pane and the phone agree on the old text.
                Err(e) => self.notice(
                    &format!(
                        "WhatsApp did not accept the edit ({e}). The text is unchanged: edits only work shortly after sending."
                    ),
                    true,
                ),
            },
            Done::Deleted {
                chat_id,
                message_id,
                result,
            } => match result {
                Ok(()) => self.apply_deleted(&chat_id, &message_id),
                Err(e) => self.notice(
                    &format!(
                        "WhatsApp did not delete the message ({e}). It stays: deleting only works shortly after sending."
                    ),
                    true,
                ),
            },
            Done::Fetched {
                chat_id,
                message_id,
                index,
                result,
            } => match result {
                Ok(path) => self.emit(Out::MediaReady {
                    network: NET,
                    chat_id,
                    message_id,
                    index,
                    path,
                }),
                Err(reason) => self.media_failed(chat_id, message_id, index, reason),
            },
            Done::Members { chat_id, result } => self.on_members(chat_id, result),
            Done::ReadMembers { chat_id, result } => self.on_read_members(chat_id, result),
            Done::Avatar {
                gen,
                person_id,
                path,
            } => {
                // An answer from before a logout is not shown: its photo is gone or not ours.
                if gen == self.avatar_gen.load(std::sync::atomic::Ordering::SeqCst) {
                    self.emit(Out::Avatar {
                        network: NET,
                        person_id,
                        path,
                    });
                }
                self.schedule_avatar();
            }
            Done::OlderAsked {
                chat_id,
                token,
                result,
            } => {
                let current = self.older.get(&chat_id).is_some_and(|p| p.token == token);
                match result {
                    Ok(id) if current => {
                        if let Some(p) = self.older.get_mut(&chat_id) {
                            p.request_id = Some(id);
                        }
                    }
                    Err(e) if current => {
                        self.older.remove(&chat_id);
                        self.older_failed(
                            chat_id,
                            format!("Could not ask the phone for older messages ({e}). Try again."),
                        );
                    }
                    _ => {}
                }
            }
            Done::OlderDeadline { chat_id, token } => {
                if self.older.get(&chat_id).is_some_and(|p| p.token == token) {
                    self.older.remove(&chat_id);
                    self.older_failed(
                        chat_id,
                        "PHONE DID NOT ANSWER: try again when the phone is online.".to_string(),
                    );
                }
            }
            Done::MarkedRead {
                chat_id,
                unread,
                mentions,
                result,
            } => {
                if let Err(e) = result {
                    log::error(AREA, &format!("read mark refused: {}", wa_log(&e)));
                    self.restore_unread(&chat_id, unread, mentions);
                    self.notice(
                        &format!(
                            "Could not mark the chat read on WhatsApp ({e}). It is unread again here: open it to retry."
                        ),
                        true,
                    );
                }
            }
            Done::Flagged {
                chat_id,
                prior,
                result,
            } => {
                if let Err(e) = result {
                    if let Err(w) = self.store.set_chat_flags(NET, &chat_id, prior) {
                        self.store_failed("restore chat flags", &w);
                    }
                    self.chat_updated(&chat_id);
                    self.notice(
                        &format!("WhatsApp did not accept the change ({e}). It is back as it was."),
                        true,
                    );
                }
            }
        }
    }

    /// Puts the unread count back after the phone refused the read mark. Messages that arrived
    /// meanwhile keep their count on top.
    fn restore_unread(&mut self, chat_id: &str, unread: u32, mentions: u32) {
        let now = match self.store.chat(NET, chat_id) {
            Ok(Some(c)) => c,
            _ => return,
        };
        if let Err(e) = self.store.set_unread(NET, chat_id, now.unread + unread) {
            return self.store_failed("restore unread", &e);
        }
        for _ in 0..mentions {
            if let Err(e) = self.store.bump_mentions(NET, chat_id) {
                log::error(AREA, &format!("restore mentions: {e}"));
                break;
            }
        }
        self.chat_updated(chat_id);
    }

    // -- reactions --------------------------------------------------------------------------

    /// Folds one reaction into the stored message and re-sends it. A reaction to a message that
    /// is not stored is logged and dropped.
    pub(super) fn apply_reaction(
        &mut self,
        chat_id: &str,
        target: &str,
        reactor: &str,
        from_me: bool,
        emoji: &str,
        ts: i64,
    ) {
        let emoji = (!emoji.is_empty()).then_some(emoji);
        match self
            .store
            .set_reaction(NET, chat_id, target, reactor, from_me, emoji, ts)
        {
            Ok(true) => match self.store.message(NET, chat_id, target) {
                Ok(Some(m)) => self.emit(Out::MessageChanged(m)),
                Ok(None) => {}
                Err(e) => self.store_failed("read message", &e),
            },
            Ok(false) => log::info(
                AREA,
                "a reaction to a message that is not stored was dropped",
            ),
            Err(e) => self.store_failed("save reaction", &e),
        }
    }

    pub(super) fn react(
        &mut self,
        chat_id: String,
        message_id: String,
        emoji: String,
        remove: bool,
    ) {
        let Some(wa) = self.wa() else {
            return self.notice(NOT_CONNECTED, true);
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return self.notice(NO_ADDRESS, true);
        };
        let msg = match self.store.message(NET, &chat_id, &message_id) {
            Ok(Some(m)) if m.state != MessageState::Deleted => m,
            Ok(Some(_)) => return self.notice("Cannot react to a deleted message.", true),
            Ok(None) => {
                return self.notice("Cannot react: that message is no longer stored.", true)
            }
            Err(e) => return self.store_failed("read message", &e),
        };
        // In a group the key names the author of the message; our own messages need none.
        let participant = if jid.is_group() && !msg.from_me {
            let known = if msg.sender_id.is_empty() {
                self.sender_of(&chat_id, &message_id).map(|j| j.to_string())
            } else {
                Some(msg.sender_id.clone())
            };
            match known {
                Some(p) => Some(p),
                None => {
                    return self.notice(
                        "Cannot react to this older message: its sender is unknown.",
                        true,
                    )
                }
            }
        } else {
            None
        };
        let key = wa::MessageKey {
            remote_jid: Some(chat_id.clone()),
            from_me: Some(msg.from_me),
            id: Some(message_id.clone()),
            participant,
        };
        // An empty emoji is how the protocol removes a reaction.
        let remove = remove || emoji.is_empty();
        let wire = if remove { String::new() } else { emoji.clone() };
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.react(jid, key, wire)).await;
            Done::Reacted {
                chat_id,
                message_id,
                emoji: (!remove).then_some(emoji),
                result,
            }
        });
    }

    // -- edit and delete --------------------------------------------------------------------

    /// An own message that is still there to change: sent (not pending, failed or deleted).
    fn own_sent(&self, chat_id: &str, message_id: &str, what: &str) -> Option<ChatMessage> {
        match self.store.message(NET, chat_id, message_id) {
            Ok(Some(m)) if m.from_me && m.state == MessageState::Sent => Some(m),
            Ok(_) => {
                self.notice(&format!("Only your own sent messages can be {what}."), true);
                None
            }
            Err(e) => {
                self.store_failed("read message", &e);
                None
            }
        }
    }

    pub(super) fn edit(&mut self, chat_id: String, message_id: String, text: String) {
        let Some(wa) = self.wa() else {
            return self.notice(NOT_CONNECTED, true);
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return self.notice(NO_ADDRESS, true);
        };
        let Some(msg) = self.own_sent(&chat_id, &message_id, "edited") else {
            return;
        };
        if text.trim().is_empty() {
            return self.notice(
                "An edited message cannot be empty: delete it instead.",
                true,
            );
        }
        if !limits::text_fits(NET, &text) {
            return self.notice(
                "MESSAGE TOO LONG: shorten the edit, WhatsApp takes up to 60000 characters.",
                true,
            );
        }
        // An edit keeps the reply it belongs to and the people it mentions (the TUI sends only the
        // text, as "@name" tokens).
        let quote = match &msg.reply_to {
            Some(q) => match self.quote_context(&jid, q) {
                Ok(ctx) => Some(ctx),
                Err(e) => return self.notice(&e, true),
            },
            None => None,
        };
        let wanted: &[PersonRef] = if jid.is_group() { &msg.mentions } else { &[] };
        let (message, applied) = rich::text_message(&text, quote, wanted);
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.edit(jid, message_id.clone(), message)).await;
            Done::Edited {
                chat_id,
                message_id,
                text,
                mentions: applied,
                result,
            }
        });
    }

    pub(super) fn delete(&mut self, chat_id: String, message_id: String) {
        let Some(wa) = self.wa() else {
            return self.notice(NOT_CONNECTED, true);
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return self.notice(NO_ADDRESS, true);
        };
        if self.own_sent(&chat_id, &message_id, "deleted").is_none() {
            return;
        }
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.revoke(jid, message_id.clone())).await;
            Done::Deleted {
                chat_id,
                message_id,
                result,
            }
        });
    }

    /// True when `sender` (who sent a revoke or edit) is the author of the stored message
    /// `target`. WhatsApp traffic is end-to-end encrypted, so the server cannot enforce this:
    /// without the check any group member could blank or rewrite anyone's message. A target that
    /// is not stored passes (the change is then ignored anyway). Group admin revokes are dropped
    /// too: the library does not mark them.
    pub(super) fn authored_by(
        &self,
        chat_id: &str,
        target: &str,
        from_me: bool,
        sender: &Jid,
        group: bool,
    ) -> bool {
        let stored = match self.store.message(NET, chat_id, target) {
            Ok(Some(m)) => m,
            // Not stored (or unreadable): nothing to protect, `change_message` ignores it.
            Ok(None) | Err(_) => return true,
        };
        let ok = if stored.from_me != from_me {
            false
        } else if from_me || !group {
            // Our own other device, or the one peer of a direct chat.
            true
        } else {
            let author = stored
                .sender_id
                .parse::<Jid>()
                .ok()
                .or_else(|| self.sender_of(chat_id, target));
            author.is_some_and(|a| {
                let (a, s) = (a.to_non_ad().user, sender.to_non_ad().user);
                a == s || self.names.linked(&a).is_some_and(|l| l == s)
            })
        };
        if !ok {
            log::info(AREA, "a revoke or edit from a different sender was dropped");
        }
        ok
    }

    /// A message was deleted for everyone (by us or by the sender): it keeps its place as a
    /// deleted message, with no text, attachments or reactions.
    pub(super) fn apply_deleted(&mut self, chat_id: &str, target: &str) {
        if let Err(e) = self.store.replace_reactions(NET, chat_id, target, &[]) {
            return self.store_failed("clear reactions", &e);
        }
        self.change_message(chat_id, target, |m| {
            if m.state == MessageState::Deleted {
                return false;
            }
            m.state = MessageState::Deleted;
            m.text.clear();
            m.attachments.clear();
            m.reactions.clear();
            m.mentions.clear();
            m.mentions_me = false;
            m.reply_to = None;
            true
        });
    }

    /// The sender edited a message: keep the latest text only.
    pub(super) fn apply_edited(
        &mut self,
        chat_id: &str,
        target: &str,
        text: &str,
        mentioned: &[String],
    ) {
        let (text, mentions) = rich::normalise_mentions(text, mentioned, &self.names);
        let me = rich::mentions_me(mentioned, &self.names);
        self.change_message(chat_id, target, |m| {
            if m.state == MessageState::Deleted {
                return false;
            }
            m.text = text;
            m.mentions = mentions;
            m.mentions_me = me && !m.from_me;
            m.edited = true;
            true
        });
    }

    // -- media: download --------------------------------------------------------------------

    fn media_failed(&self, chat_id: String, message_id: String, index: u32, reason: String) {
        log::error(AREA, &format!("media fetch failed: {}", wa_log(&reason)));
        self.emit(Out::MediaFailed {
            network: NET,
            chat_id,
            message_id,
            index,
            reason,
            fallback_url: None,
        });
    }

    pub(super) fn fetch_media(&mut self, chat_id: String, message_id: String, index: u32) {
        let att = match self.store.message(NET, &chat_id, &message_id) {
            Ok(Some(m)) => m.attachments.get(index as usize).cloned(),
            Ok(None) => None,
            Err(e) => {
                self.store_failed("read message", &e);
                // Answer the TUI too, or its item stays Downloading and holds an auto slot.
                let reason = "Could not read the message locally.".to_string();
                return self.media_failed(chat_id, message_id, index, reason);
            }
        };
        let Some(att) = att else {
            let reason = "That attachment is no longer available.".to_string();
            return self.media_failed(chat_id, message_id, index, reason);
        };
        let Some(fetch) = rich::parse_fetch_ref(&att.fetch_ref) else {
            let reason =
                "MEDIA EXPIRED: this file cannot be downloaded, ask them to send it again."
                    .to_string();
            return self.media_failed(chat_id, message_id, index, reason);
        };
        let ext = safe_extension(att.name.as_deref(), att.mime.as_deref());
        if let Some(path) = self.media.lookup(NET, &att.fetch_ref, &ext) {
            return self.emit(Out::MediaReady {
                network: NET,
                chat_id,
                message_id,
                index,
                path,
            });
        }
        // Before any network call: a file over the limit is refused, not downloaded and dropped.
        let declared = att.size.or(Some(fetch.length)).filter(|n| *n > 0);
        if let Err(reason) = self.media.check_size(declared) {
            return self.media_failed(chat_id, message_id, index, reason);
        }
        let Some(wa) = self.wa() else {
            return self.media_failed(chat_id, message_id, index, NOT_CONNECTED.to_string());
        };
        let cache = self.media.clone();
        let fetch_ref = att.fetch_ref;
        self.spawn_done(async move {
            let result = fetch_into_cache(wa, cache, fetch, fetch_ref, ext).await;
            Done::Fetched {
                chat_id,
                message_id,
                index,
                result,
            }
        });
    }

    // -- media: send ------------------------------------------------------------------------

    pub(super) fn send_media(
        &mut self,
        chat_id: String,
        client_id: String,
        path: PathBuf,
        caption: String,
    ) {
        let name = rich::display_name(&path);
        // The backend decides by content: the TUI's guess is not trusted.
        let kind = kind_for_path(&path);
        let draft = self.draft(
            &chat_id,
            &client_id,
            &rich::media_marker(kind, &name, &caption),
        );
        let Some(wa) = self.wa() else {
            return self.send_failed(draft, NOT_CONNECTED.into());
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return self.send_failed(draft, NO_ADDRESS.into());
        };
        if !limits::text_fits(NET, &caption) {
            let error = "MESSAGE TOO LONG: shorten the caption.".to_string();
            return self.send_failed(draft, error);
        }
        let limit = limits::send_limit_bytes(NET, kind);
        if let Err(error) = check_file(&path, kind, limit) {
            return self.send_failed(draft, error);
        }
        let events = self.events.clone();
        let tx = self.tx.clone();
        self.tasks.spawn(async move {
            let (draft, result) =
                media_job(wa, events, jid, path, kind, name, caption, draft, limit).await;
            let _ = tx.send(Msg::Sent(Box::new(Sent {
                chat_id: draft.chat_id.clone(),
                client_id,
                draft,
                result,
            })));
        });
    }

    // -- older history from the phone -------------------------------------------------------

    fn older_failed(&self, chat_id: String, error: String) {
        log::error(AREA, &format!("older messages failed: {}", wa_log(&error)));
        self.emit(Out::OlderFailed {
            network: NET,
            chat_id,
            error,
        });
    }

    fn older_key(chat_id: &str) -> String {
        format!("whatsapp.older.{chat_id}")
    }

    pub(super) fn load_older(&mut self, chat_id: String, before_ts: i64, before_id: String) {
        // 1. What is stored comes first and costs nothing.
        match self
            .store
            .messages_before(NET, &chat_id, before_ts, &before_id, OLDER_PAGE)
        {
            Ok(messages) if !messages.is_empty() => {
                return self.emit(Out::OlderMessages {
                    network: NET,
                    chat_id,
                    messages,
                    at_start: false,
                });
            }
            Ok(_) => {}
            Err(e) => {
                log::error(AREA, &format!("read older messages: {e}"));
                return self.older_failed(
                    chat_id,
                    "Could not read older messages from the local copy. See chatcli.log."
                        .to_string(),
                );
            }
        }
        // 2. The phone already said there is nothing older.
        if self
            .store
            .meta(&Self::older_key(&chat_id))
            .ok()
            .flatten()
            .is_some()
        {
            return self.emit(Out::OlderMessages {
                network: NET,
                chat_id,
                messages: Vec::new(),
                at_start: true,
            });
        }
        // 3. One request to the phone, one 30 s wait. A second scroll while it waits is the same
        // request, not another one.
        if self.older.contains_key(&chat_id) {
            return;
        }
        let Some(wa) = self.wa() else {
            return self.older_failed(chat_id, NOT_CONNECTED.to_string());
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return self.older_failed(chat_id, NO_ADDRESS.to_string());
        };
        let oldest = match self.store.oldest_message(NET, &chat_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return self.older_failed(
                    chat_id,
                    "Nothing is stored for this chat yet: wait for WhatsApp to sync it."
                        .to_string(),
                )
            }
            Err(e) => {
                log::error(AREA, &format!("read oldest message: {e}"));
                return self.older_failed(
                    chat_id,
                    "Could not read the local copy. See chatcli.log.".to_string(),
                );
            }
        };
        self.older_token += 1;
        let token = self.older_token;
        self.older.insert(
            chat_id.clone(),
            OlderPending {
                token,
                request_id: None,
                before_ts: oldest.ts,
                before_id: oldest.id.clone(),
            },
        );
        let (ask_chat, wait_chat) = (chat_id.clone(), chat_id);
        self.spawn_done(async move {
            let result = within(
                NETWORK_TIMEOUT,
                wa.request_history(jid, oldest.id, oldest.from_me, oldest.ts),
            )
            .await;
            Done::OlderAsked {
                chat_id: ask_chat,
                token,
                result,
            }
        });
        self.spawn_done(async move {
            tokio::time::sleep(OLDER_WAIT).await;
            Done::OlderDeadline {
                chat_id: wait_chat,
                token,
            }
        });
    }

    /// After a history chunk was stored: an on-demand answer finishes the matching request; any
    /// other chunk refreshes the chat the user has open (the sidebar alone would leave its
    /// messages incomplete until it is reopened).
    pub(super) fn after_history(
        &mut self,
        o: &HistoryOutcome,
        on_demand: bool,
        session_id: Option<&str>,
    ) {
        if on_demand {
            return self.resolve_older(o, session_id);
        }
        let Some(open) = self.open_chat.clone() else {
            return;
        };
        if !o.touched.contains(&open) {
            return;
        }
        match self.store.messages(NET, &open, OPEN_LIMIT) {
            Ok(messages) => self.emit(Out::Messages {
                network: NET,
                chat_id: open,
                messages,
            }),
            Err(e) => self.store_failed("read messages", &e),
        }
    }

    fn resolve_older(&mut self, o: &HistoryOutcome, session_id: Option<&str>) {
        let matched: Vec<String> = self
            .older
            .iter()
            .filter(|(chat, p)| {
                session_id.is_some_and(|s| p.request_id.as_deref() == Some(s))
                    || o.touched.contains(chat)
            })
            .map(|(chat, _)| chat.clone())
            .collect();
        for chat_id in matched {
            let Some(p) = self.older.remove(&chat_id) else {
                continue;
            };
            let rows =
                self.store
                    .messages_before(NET, &chat_id, p.before_ts, &p.before_id, OLDER_PAGE);
            match rows {
                Ok(messages) if !messages.is_empty() => self.emit(Out::OlderMessages {
                    network: NET,
                    chat_id,
                    messages,
                    at_start: false,
                }),
                Ok(_) => {
                    // The phone answered with nothing older: this chat starts here.
                    if let Err(e) = self
                        .store
                        .set_meta(&Self::older_key(&chat_id), Some("start"))
                    {
                        log::error(AREA, &format!("save start of chat: {e}"));
                    }
                    self.emit(Out::OlderMessages {
                        network: NET,
                        chat_id,
                        messages: Vec::new(),
                        at_start: true,
                    });
                }
                Err(e) => {
                    log::error(AREA, &format!("read older messages: {e}"));
                    self.older_failed(
                        chat_id,
                        "Could not read older messages from the local copy. See chatcli.log."
                            .to_string(),
                    );
                }
            }
        }
    }

    // -- typing -----------------------------------------------------------------------------

    /// Our own typing state. On by default (`send_typing`): with it off no chat-state stanza
    /// is ever sent. On, "composing" goes out at most once per 5 s per chat, and "paused" only
    /// after a "composing".
    pub(super) async fn typing(&mut self, chat_id: String, active: bool) {
        if !self.settings.whatsapp.send_typing {
            return;
        }
        let Some(wa) = self.wa() else { return };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return;
        };
        if active {
            let now = Instant::now();
            if self
                .typing_sent
                .get(&chat_id)
                .is_some_and(|t| now.duration_since(*t) < TYPING_EVERY)
            {
                return;
            }
            self.typing_sent.insert(chat_id.clone(), now);
            self.typing_open.insert(chat_id);
        } else if !self.typing_open.remove(&chat_id) {
            return;
        }
        match tokio::time::timeout(TYPING_TIMEOUT, wa.typing(jid, active)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::error(AREA, &format!("typing state not sent: {}", wa_log(&e))),
            Err(_) => log::error(AREA, "typing state not sent: no answer in 5 s"),
        }
    }

    /// Someone in a chat started or stopped typing.
    pub(super) fn on_chat_presence(&mut self, p: &ChatPresenceUpdate) {
        let src = &p.source;
        if src.is_from_me {
            return;
        }
        let raw = src.chat.to_non_ad();
        if !map::is_supported_chat(&raw) {
            return;
        }
        let chat_id = self.canonical_id(&raw);
        // Recording a voice message is a different state; only text typing is shown.
        let active = p.state == ChatPresence::Composing && p.media == ChatPresenceMedia::Text;
        let sender = self.names.label_with(&src.sender.to_non_ad(), None);
        if active {
            self.typing_in.insert((chat_id.clone(), sender.clone()));
        } else {
            self.typing_in.remove(&(chat_id.clone(), sender.clone()));
        }
        self.emit(Out::Typing {
            network: NET,
            chat_id,
            sender,
            active,
        });
    }

    /// Their message arrived, so they are no longer typing.
    pub(super) fn typing_ended(&mut self, chat_id: &str, sender: &str) {
        if self
            .typing_in
            .remove(&(chat_id.to_string(), sender.to_string()))
        {
            self.emit(Out::Typing {
                network: NET,
                chat_id: chat_id.to_string(),
                sender: sender.to_string(),
                active: false,
            });
        }
    }

    // -- profile photos ---------------------------------------------------------------------

    fn avatar_none(&self, person_id: String, path: Option<PathBuf>) {
        self.emit(Out::Avatar {
            network: NET,
            person_id,
            path,
        });
    }

    /// Answers every person: from the cache when it is fresh, otherwise by queueing one lookup
    /// each. Only what the TUI asked for is ever looked up (never a loop over contacts), each id
    /// once a session, behind the rate limit in `schedule_avatar`.
    pub(super) fn fetch_avatars(&mut self, people: Vec<String>) {
        let now = now_ms();
        for id in people {
            let usable = id.parse::<Jid>().is_ok_and(|j| j.is_pn() || j.is_lid());
            if !usable {
                self.avatar_none(id, None);
                continue;
            }
            let stale = match self.media.avatar_lookup(NET, &id, now) {
                AvatarLookup::Fresh(p) => {
                    self.avatar_none(id, Some(p));
                    continue;
                }
                AvatarLookup::NoPhoto => {
                    self.avatar_none(id, None);
                    continue;
                }
                AvatarLookup::Stale(p) => Some(p),
                AvatarLookup::Missing => None,
            };
            if self.avatar_seen.contains(&id)
                || self.link.is_none()
                || self.avatar_queue.len() >= AVATAR_QUEUE_MAX
            {
                self.avatar_none(id, stale);
                continue;
            }
            self.avatar_seen.insert(id.clone());
            self.avatar_queue.push_back(id);
        }
        self.schedule_avatar();
    }

    /// Starts the next queued lookup when the limits allow, else sets `avatar_at` to when one
    /// will. One lookup per call: the answer calls this again.
    pub(super) fn schedule_avatar(&mut self) {
        self.avatar_at = None;
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        while self
            .avatar_sent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= minute)
        {
            self.avatar_sent.pop_front();
        }
        if self.avatar_queue.is_empty() {
            return;
        }
        if self.avatar_total >= AVATAR_SESSION_MAX {
            if self.avatar_total == AVATAR_SESSION_MAX {
                self.avatar_total += 1;
                log::info(AREA, "profile photo lookup limit for this run reached");
            }
            return self.drop_avatar_queue();
        }
        let mut at = now;
        if self.avatar_sent.len() >= AVATAR_PER_MIN {
            if let Some(first) = self.avatar_sent.front() {
                at = at.max(*first + minute);
            }
        }
        if let Some(last) = self.avatar_sent.back() {
            at = at.max(*last + AVATAR_GAP);
        }
        if at > now {
            self.avatar_at = Some(at);
            return;
        }
        let Some(id) = self.avatar_queue.pop_front() else {
            return;
        };
        let (Some(wa), Ok(jid)) = (self.wa(), id.parse::<Jid>()) else {
            return self.avatar_none(id, None);
        };
        self.avatar_sent.push_back(now);
        self.avatar_total += 1;
        let cache = self.media.clone();
        let live = self.avatar_gen.clone();
        let gen = live.load(std::sync::atomic::Ordering::SeqCst);
        self.spawn_done(async move {
            let path = avatar_job(wa, &cache, &id, jid, &live, gen).await;
            Done::Avatar {
                gen,
                person_id: id,
                path,
            }
        });
    }

    /// Logout or stop: every waiting person is answered with no photo.
    pub(super) fn drop_avatar_queue(&mut self) {
        self.avatar_at = None;
        while let Some(id) = self.avatar_queue.pop_front() {
            self.avatar_none(id, None);
        }
    }

    // -- members ----------------------------------------------------------------------------

    pub(super) fn load_members(&mut self, chat_id: String) {
        let members_failed = |this: &Self, chat_id: String, error: String| {
            this.emit(Out::Members {
                network: NET,
                chat_id,
                members: Vec::new(),
                error: Some(error),
            });
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return members_failed(self, chat_id, NO_ADDRESS.to_string());
        };
        if !jid.is_group() {
            return members_failed(
                self,
                chat_id,
                "Mentions work in group chats only.".to_string(),
            );
        }
        let Some(wa) = self.wa() else {
            return self.on_members(chat_id, Err(NOT_CONNECTED.to_string()));
        };
        let names = self.names.clone();
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.members(jid))
                .await
                .map(|m| member_people(m, &names));
            Done::Members { chat_id, result }
        });
    }

    fn on_members(&mut self, chat_id: String, result: Result<Vec<PersonRef>, String>) {
        match result {
            Ok(members) => {
                if let Err(e) = self.store.replace_members(NET, &chat_id, &members) {
                    log::error(AREA, &format!("save members: {e}"));
                }
                self.emit(Out::Members {
                    network: NET,
                    chat_id,
                    members,
                    error: None,
                });
            }
            Err(e) => {
                // The last known list still lets the user mention people while offline.
                let cached = self.store.members(NET, &chat_id, 1024).unwrap_or_default();
                if cached.is_empty() {
                    self.emit(Out::Members {
                        network: NET,
                        chat_id,
                        members: Vec::new(),
                        error: Some(format!(
                            "Could not read the group's members: {}",
                            wa_log(&e)
                        )),
                    });
                } else {
                    log::info(
                        AREA,
                        "members come from the saved list: the group could not be read",
                    );
                    self.emit(Out::Members {
                        network: NET,
                        chat_id,
                        members: cached,
                        error: None,
                    });
                }
            }
        }
    }

    // -- chat flags -------------------------------------------------------------------------

    pub(super) fn set_chat_flag(&mut self, chat_id: String, flag: ChatFlag, on: bool) {
        // The stored flags stay as they were: the user is told, and the row is shown again so the
        // TUI undoes its own guess.
        let refuse = |this: &Self, chat_id: &str, text: &str| {
            this.notice(text, true);
            this.chat_updated(chat_id);
        };
        let Some(wa) = self.wa() else {
            return refuse(self, &chat_id, NOT_CONNECTED);
        };
        let Ok(jid) = chat_id.parse::<Jid>() else {
            return refuse(self, &chat_id, NO_ADDRESS);
        };
        let prior = match self.store.chat_flags(NET, &chat_id) {
            Ok(f) => f,
            Err(e) => return self.store_failed("read chat flags", &e),
        };
        let mut next = prior;
        match flag {
            ChatFlag::Pinned => next.pinned = on,
            ChatFlag::Muted => next.muted_until = on.then_some(MUTE_FOREVER),
            ChatFlag::Hidden => next.hidden = on,
        }
        // Local first, so the sidebar answers at once; the phone follows.
        if let Err(e) = self.store.set_chat_flags(NET, &chat_id, next) {
            return self.store_failed("save chat flags", &e);
        }
        self.chat_updated(&chat_id);
        self.spawn_done(async move {
            let result = within(NETWORK_TIMEOUT, wa.set_flag(jid, flag, on)).await;
            Done::Flagged {
                chat_id,
                prior,
                result,
            }
        });
    }

    /// The phone pinned, muted or archived a chat: the network wins over what is stored.
    fn update_flags(&mut self, jid: &Jid, change: impl FnOnce(&mut ChatFlags)) {
        let id = self.canonical_id(jid);
        let before = match self.store.chat_flags(NET, &id) {
            Ok(f) => f,
            Err(e) => return self.store_failed("read chat flags", &e),
        };
        let mut after = before;
        change(&mut after);
        if after == before {
            return;
        }
        if let Err(e) = self.store.set_chat_flags(NET, &id, after) {
            return self.store_failed("save chat flags", &e);
        }
        self.chat_updated(&id);
    }

    pub(super) fn on_pin_update(&mut self, jid: &Jid, pinned: bool) {
        self.update_flags(jid, |f| f.pinned = pinned);
    }

    pub(super) fn on_mute_update(&mut self, jid: &Jid, until: Option<i64>) {
        self.update_flags(jid, |f| f.muted_until = until);
    }

    pub(super) fn on_archive_update(&mut self, jid: &Jid, archived: bool) {
        self.update_flags(jid, |f| f.hidden = archived);
    }
}

// ---------------------------------------------------------------------------------------------
// Free functions: pure or running on a spawned task, so they are tested on their own.
// ---------------------------------------------------------------------------------------------

/// Library error text with URLs removed, for logs and notices.
pub(super) fn wa_log(text: &str) -> String {
    crate::wa::redact_urls(text)
}

/// One profile photo lookup, stored in the cache. No photo, a picture over the cap or one that
/// will not decode all write the "no photo" marker; a network failure writes nothing and keeps
/// an older photo. When `live` no longer reads `gen` (the session stopped) nothing is written.
async fn avatar_job(
    wa: Arc<dyn Wa>,
    cache: &MediaCache,
    id: &str,
    jid: Jid,
    live: &Arc<std::sync::atomic::AtomicU64>,
    gen: u64,
) -> Option<PathBuf> {
    use std::sync::atomic::Ordering::SeqCst;
    let current = || live.load(SeqCst) == gen;
    let stale = || match cache.avatar_lookup(NET, id, now_ms()) {
        AvatarLookup::Stale(p) => Some(p),
        _ => None,
    };
    let no_photo = || {
        if current() {
            let _ = cache.store_no_avatar(NET, id);
        }
        None
    };
    let answer = within(AVATAR_WAIT, wa.profile_picture(jid)).await;
    if !current() {
        return None;
    }
    match answer {
        Ok(Some(bytes)) => {
            let c = cache.clone();
            let i = id.to_string();
            let live = live.clone();
            // Lookups are paced (AVATAR_GAP apart), so one cache walk per photo is cheap here.
            let stored = tokio::task::spawn_blocking(move || {
                if live.load(SeqCst) != gen {
                    return None;
                }
                let r = c.store_avatar(NET, &i, &bytes);
                if r.is_ok() {
                    let _ = c.evict(now_ms());
                }
                Some(r)
            })
            .await;
            match stored {
                Ok(Some(Ok(p))) => Some(p),
                Ok(None) => None,
                _ => no_photo(),
            }
        }
        Ok(None) => no_photo(),
        Err(e) if e == crate::wa::AVATAR_TOO_BIG => no_photo(),
        Err(_) => {
            // Fixed text only: library errors can carry server nodes.
            log::info(AREA, "profile photo lookup failed");
            stale()
        }
    }
}

/// `fut` or a one-sentence timeout, so one dead call never hangs a task for ever.
pub(super) async fn within<T>(
    limit: Duration,
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err("WhatsApp did not answer in time".to_string()),
    }
}

/// Downloads one attachment into the cache. The bytes are checked against the sender's file
/// hash before they are written, so a damaged or swapped file is never cached.
pub(super) async fn fetch_into_cache(
    wa: Arc<dyn Wa>,
    cache: MediaCache,
    fetch: rich::Fetch,
    fetch_ref: chatcli_core::model::FetchRef,
    ext: String,
) -> Result<PathBuf, String> {
    let sha = fetch.sha256.clone();
    let limit = cache.max_file_bytes();
    let bytes = match tokio::time::timeout(TRANSFER_TIMEOUT, wa.download(fetch, limit)).await {
        Ok(Ok(b)) => b,
        Ok(Err(FetchError::Expired)) => return Err(MEDIA_EXPIRED.to_string()),
        // The declared size was small but the real file is not: same sentence as the up-front check.
        Ok(Err(FetchError::TooBig)) => {
            return Err(cache
                .check_size(Some(limit.saturating_add(1)))
                .err()
                .unwrap_or_else(|| "FILE OVER the size limit.".to_string()))
        }
        Ok(Err(FetchError::Other(e))) => {
            return Err(format!(
                "Could not download the file ({}). Try again.",
                wa_log(&e)
            ))
        }
        Err(_) => return Err("The download took too long. Try again.".to_string()),
    };
    tokio::task::spawn_blocking(move || write_verified(&cache, &fetch_ref, &ext, &bytes, &sha))
        .await
        .map_err(|e| format!("Saving the file failed ({e}). Try again."))?
}

/// Writes `bytes` to the cache when they match `sha256` (empty = the sender gave none).
pub(super) fn write_verified(
    cache: &MediaCache,
    fetch_ref: &chatcli_core::model::FetchRef,
    ext: &str,
    bytes: &[u8],
    sha256: &[u8],
) -> Result<PathBuf, String> {
    if !sha256.is_empty() && Sha256::digest(bytes).as_slice() != sha256 {
        return Err(
            "The downloaded file failed its integrity check and was discarded. Try again."
                .to_string(),
        );
    }
    let mut part = cache.begin(NET, fetch_ref, ext)?;
    part.write_all(bytes).map_err(|e| {
        let text = e.to_string();
        if text.starts_with("FILE OVER") {
            text
        } else {
            format!("Could not write the media cache ({text}): check the disk has room.")
        }
    })?;
    cache.commit(part)
}

/// Checks a file before any network call: it exists, is a file, is not empty and is within the
/// send limit for its kind (images are checked again after re-encoding).
pub(super) fn check_file(
    path: &std::path::Path,
    kind: MediaKind,
    limit: u64,
) -> Result<(), String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("FILE NOT FOUND: check the path.".to_string())
        }
        Err(e) => {
            return Err(format!(
                "Could not read the file ({e}): check you can open it."
            ))
        }
    };
    if !meta.is_file() {
        return Err("That is a folder, not a file: pick a file.".to_string());
    }
    if meta.len() == 0 {
        return Err("FILE IS EMPTY: pick another file.".to_string());
    }
    // A picture is re-encoded, so its limit applies to the result; the raw file only needs to
    // be something `prepare_image` is willing to open.
    if kind != MediaKind::Image && meta.len() > limit {
        return Err(format!(
            "FILE OVER {} MB: WhatsApp takes files up to that size.",
            limit / MIB
        ));
    }
    Ok(())
}

/// Progress that reaches the TUI at most 4 times a second, plus always the last step.
pub(super) fn progress_to(events: EventTx, chat_id: String, client_id: String) -> Progress {
    let last: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    Arc::new(move |done, total| {
        let now = std::time::Instant::now();
        if let Ok(mut at) = last.lock() {
            if done < total && at.is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY) {
                return;
            }
            *at = Some(now);
        }
        let _ = events.send(Out::SendProgress {
            network: NET,
            chat_id: chat_id.clone(),
            client_id: client_id.clone(),
            done,
            total,
        });
    })
}

/// Prepares, uploads and sends one image or document. Returns the message as it will be stored
/// and the network id, or the reason it failed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn media_job(
    wa: Arc<dyn Wa>,
    events: EventTx,
    jid: Jid,
    path: PathBuf,
    kind: MediaKind,
    name: String,
    caption: String,
    mut draft: ChatMessage,
    limit: u64,
) -> (ChatMessage, Result<String, String>) {
    let result = async {
        // A picture is decoded and encoded again: that drops EXIF (it can hold a location).
        let (input, mime, size, shown_name) = if kind == MediaKind::Image {
            let p = path.clone();
            let image = tokio::task::spawn_blocking(move || prepare_image(&p, limit))
                .await
                .map_err(|e| format!("Preparing the picture failed ({e})."))??;
            (
                UploadInput::Bytes(image.bytes),
                image.mime.to_string(),
                Some((image.width, image.height)),
                None,
            )
        } else {
            (
                UploadInput::File(path),
                rich::mime_for_name(&name).to_string(),
                None,
                Some(name.clone()),
            )
        };
        let progress = progress_to(events.clone(), draft.chat_id.clone(), draft.id.clone());
        let up =
            match tokio::time::timeout(TRANSFER_TIMEOUT, wa.upload(input, kind, progress)).await {
                Ok(r) => r?,
                Err(_) => return Err("The upload took too long. Try again.".to_string()),
            };
        let message = match size {
            Some((w, h)) => rich::image_message(&up, &mime, w, h, &caption),
            None => rich::document_message(&up, &name, &mime, &caption),
        };
        draft.attachments = vec![rich::sent_attachment(
            kind,
            shown_name.as_deref(),
            &mime,
            &up,
        )];
        within(NETWORK_TIMEOUT, wa.send(jid, message)).await
    }
    .await;
    (draft, result)
}

/// The people of a group for @-completion: every participant except us, named from the saved
/// names (a hidden number falls back to its phone number's name), without duplicates.
pub(super) fn member_people(members: Vec<Member>, names: &Names) -> Vec<PersonRef> {
    let mut out: Vec<PersonRef> = Vec::new();
    for m in members {
        if names.is_own(&m.jid.user) || m.pn.as_ref().is_some_and(|p| names.is_own(&p.user)) {
            continue;
        }
        let mut name = names.label_with(&m.jid, None);
        if map::is_fallback_title(&name) {
            if let Some(pn) = &m.pn {
                name = names.label_with(pn, None);
            }
        }
        let id = m.jid.to_string();
        if !out.iter().any(|p| p.id == id) {
            out.push(PersonRef { id, name });
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}
