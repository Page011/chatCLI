//! Everyday features that sit on top of the chat list and the conversation: desktop toasts for
//! new messages (24), older history on scroll-up (29, 30), jump to the next unread and mark all
//! read (31), pin, mute and hide (32), search (33) and typing indicators (52).
//!
//! `App` stays pure: a toast is queued for the run loop to hand to the notifier, a request for
//! the backend is returned as an `Out`, and the clock is the one `tick` sets.

use chatcli_core::model::{ChatFlag, ChatMessage, Command, Event, Network};
use chatcli_core::runtime::Toast;
use chatcli_core::store::SearchHit;

use super::{App, Focus, Out, Prompt};
use crate::chats;
use crate::keymap::Action;
use crate::notify::{self, Admit, Arrived, Verdict};
use crate::older::{self, Older};
use crate::search::Search;
use crate::text::{clean_line, clean_msg};
use crate::theme::Theme;
use crate::typing;
use crate::ui;

impl App {
    // ---- look: theme and layout ----

    pub(crate) fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Replaces the theme (the run loop applies `NO_COLOR` this way; tests pick a built-in).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// What the drawing code needs besides the app: styles, the clock and the time format.
    pub(crate) fn look(&self) -> ui::Look<'_> {
        ui::Look {
            theme: &self.theme,
            clock: &self.clock,
            fmt: self.fmt,
            avatars: self.layout.avatars,
        }
    }

    /// Rows one chat takes in the sidebar: a title row, and a preview row unless the density is
    /// compact or previews are off.
    pub(crate) fn row_h(&self) -> u16 {
        use chatcli_core::settings::Density;
        if self.layout.density == Density::Compact || !self.layout.show_previews {
            1
        } else {
            2
        }
    }

    // ---- notifications (24) ----

    /// The terminal window gained or lost focus (a focus event from the terminal). Until the
    /// first event arrives the window counts as not focused, so a terminal that never reports
    /// focus still gets its toasts.
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Toasts queued since the last call; the run loop hands each to the notifier.
    pub fn take_toasts(&mut self) -> Vec<Toast> {
        std::mem::take(&mut self.toasts)
    }

    fn queue_toast(&mut self, title: String, body: String) {
        self.toasts.push(Toast {
            title,
            body,
            sound: self.notif.sound,
        });
    }

    /// A message that just arrived live (`Event::Message`, the only source of toasts).
    pub(super) fn notify_arrival(&mut self, m: &ChatMessage) {
        let n = m.network;
        let now = self.clock.now_ms;
        let chat = self.ns(n).chats.iter().find(|c| c.id == m.chat_id);
        let muted = chat.is_some_and(|c| chats::is_muted(c, now));
        let title = chat.map_or_else(|| "Chat".to_string(), |c| c.title.clone());
        let on_screen =
            self.ns(n).open.as_deref() == Some(m.chat_id.as_str()) && self.convo_visible(n);
        let verdict = notify::decide(&Arrived {
            from_me: m.from_me,
            ts: m.ts,
            ui_start_ms: self.started_ms,
            enabled: self.notif.enabled,
            muted,
            focused: self.focused,
            chat_on_screen: on_screen,
        });
        if verdict != Verdict::Toast {
            return;
        }
        let key = (n, m.chat_id.clone());
        let mut slot = self.folds.remove(&key);
        let admitted = notify::admit(&mut slot, &title, now);
        self.folds.extend(slot.map(|f| (key, f)));
        if admitted == Admit::Now {
            let (title, body) =
                notify::toast_text(self.notif.preview, &title, &m.sender, &clean_msg(&m.text));
            self.queue_toast(title, body);
        }
    }

    /// Earliest time a folded toast is due, for `next_deadline`.
    pub(super) fn toast_deadline(&self) -> Option<i64> {
        self.folds
            .values()
            .filter(|f| f.pending > 0)
            .map(|f| f.last_ms + notify::FOLD_MS)
            .min()
    }

    /// Shows the folded `N new messages` toasts whose window ended, and forgets quiet chats.
    pub(super) fn fire_toasts(&mut self) {
        let now = self.clock.now_ms;
        let mut due: Vec<(String, String)> = Vec::new();
        for f in self.folds.values_mut() {
            due.extend(notify::take_due(f, now));
        }
        self.folds
            .retain(|_, f| f.pending > 0 || now - f.last_ms < notify::FOLD_MS);
        for (title, body) in due {
            self.queue_toast(title, body);
        }
    }

    // ---- older history (29, 30) ----

    /// The user scrolled past the top (or pressed gg on the first message): ask the backend for
    /// the page before the oldest message held. One request at a time, never after the start of
    /// the conversation, never without a user action calling this.
    pub(super) fn request_older(&mut self, n: Network) -> Out {
        let ns = self.ns_mut(n);
        let Some(chat_id) = ns.open.clone() else {
            return Vec::new();
        };
        if !ns
            .older
            .get(&chat_id)
            .copied()
            .unwrap_or_default()
            .may_ask()
        {
            return Vec::new();
        }
        let Some((before_ts, before_id)) = ns
            .messages
            .get(&chat_id)
            .and_then(|l| older::request_point(l))
        else {
            return Vec::new();
        };
        ns.older.insert(chat_id.clone(), Older::Loading);
        // The user is at the top: scroll to the new top line, so `loading older...` is seen.
        let max = self.convo_lines(n).saturating_sub(self.convo_size(n).1);
        self.ns_mut(n).scroll = max;
        vec![(
            n,
            Command::LoadOlder {
                chat_id,
                before_ts,
                before_id,
            },
        )]
    }

    /// True when the conversation is already scrolled to its oldest line.
    pub(super) fn at_top(&self, n: Network) -> bool {
        let max = self.convo_lines(n).saturating_sub(self.convo_size(n).1);
        self.ns(n).scroll >= max
    }

    fn older_arrived(
        &mut self,
        n: Network,
        chat_id: String,
        mut messages: Vec<ChatMessage>,
        at_start: bool,
    ) {
        messages.iter_mut().for_each(super::clean_message);
        let ns = self.ns_mut(n);
        ns.older.insert(
            chat_id.clone(),
            if at_start {
                Older::AtStart
            } else {
                Older::Idle
            },
        );
        // A chat that was never loaded is skipped: opening it brings the whole history. The
        // scroll offset counts from the newest line, so lines added above move nothing.
        if let Some(held) = ns.messages.get_mut(&chat_id) {
            let (merged, _) = older::merge_older(std::mem::take(held), messages);
            *held = merged;
            self.keep_selection(n, &chat_id);
        }
    }

    // ---- unread (31) ----

    /// `next_unread` / `prev_unread`: select the next chat of this tab with unread messages,
    /// wrapping round. Muted chats are skipped: you muted them to stop being told.
    pub(super) fn jump_unread(&mut self, n: Network, dir: isize) {
        let now = self.clock.now_ms;
        let ns = self.ns(n);
        let vis = ns.visible();
        let unread: Vec<bool> = vis.iter().map(|&i| ns.chats[i].unread > 0).collect();
        let eligible: Vec<bool> = vis
            .iter()
            .zip(&unread)
            .map(|(&i, &u)| u && !chats::is_muted(&ns.chats[i], now))
            .collect();
        match chats::next_eligible(&eligible, ns.sel_pos(&vis), dir) {
            Some(pos) => {
                let id = ns.chats[vis[pos]].id.clone();
                self.ns_mut(n).selected = Some(id.clone());
                self.show(n, id);
                self.ensure_visible(n);
            }
            None if unread.iter().any(|&u| u) => {
                self.push_notice(n, "Only muted chats have unread messages.", false);
            }
            None => self.push_notice(n, "No unread chats.", false),
        }
    }

    /// `mark_all_read`: one line to confirm, then one `MarkRead` per unread chat of the tab.
    pub(super) fn ask_mark_all_read(&mut self, n: Network) {
        let count = self.ns(n).chats.iter().filter(|c| c.unread > 0).count();
        if count == 0 {
            self.push_notice(n, "Nothing unread.", false);
        } else {
            self.prompt = Some(Prompt::MarkAllRead { count });
        }
    }

    pub(super) fn mark_all_read(&mut self, n: Network) -> Out {
        let ns = self.ns_mut(n);
        let ids: Vec<String> = ns
            .chats
            .iter()
            .filter(|c| c.unread > 0)
            .map(|c| c.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in ids {
            ns.mark_read(&id, &mut out);
        }
        out
    }

    // ---- pin, mute, hide (32) ----

    /// `pin`, `mute`, `hide`: ask the backend to flip the flag on the selected chat. The list
    /// changes when the backend answers with the chat, so what is shown is what was stored.
    pub(super) fn flag_action(&mut self, n: Network, flag: ChatFlag) -> Out {
        let now = self.clock.now_ms;
        let ns = self.ns(n);
        let vis = ns.visible();
        let Some(&i) = vis.get(ns.sel_pos(&vis)) else {
            return Vec::new();
        };
        let c = &ns.chats[i];
        let on = match flag {
            ChatFlag::Pinned => !c.flags.pinned,
            ChatFlag::Muted => !chats::is_muted(c, now),
            ChatFlag::Hidden => !c.flags.hidden,
        };
        vec![(
            n,
            Command::SetChatFlag {
                chat_id: c.id.clone(),
                flag,
                on,
            },
        )]
    }

    pub(super) fn toggle_show_hidden(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        ns.show_hidden = !ns.show_hidden;
        self.fix_selection(n, true);
    }

    // ---- typing (52) ----

    pub(super) fn typing_event(&mut self, n: Network, chat_id: String, sender: &str, active: bool) {
        // Teams has no typing indicator; the contract never sends one.
        if n != Network::WhatsApp {
            return;
        }
        let now = self.clock.now_ms;
        let sender = clean_line(sender);
        let ns = self.ns_mut(n);
        let t = ns.typing.entry(chat_id.clone()).or_default();
        if active {
            t.start(&sender, now);
        } else {
            t.stop(&sender);
        }
        if t.is_empty() {
            ns.typing.remove(&chat_id);
        }
    }

    /// A message from `sender` ends their typing indicator.
    pub(super) fn typing_stops(&mut self, n: Network, chat_id: &str, sender: &str) {
        let ns = self.ns_mut(n);
        if let Some(t) = ns.typing.get_mut(chat_id) {
            t.stop(sender);
            if t.is_empty() {
                ns.typing.remove(chat_id);
            }
        }
    }

    /// Drops typing indicators that ran out. True when one did: the screen needs a redraw.
    pub(super) fn prune_typing(&mut self) -> bool {
        let now = self.clock.now_ms;
        let mut changed = false;
        for ns in &mut self.nets {
            ns.typing.retain(|_, t| {
                changed |= t.prune(now);
                !t.is_empty()
            });
        }
        changed
    }

    /// Your own typing: after a key or paste that took the draft from empty to something,
    /// tell WhatsApp once (at most every 5 s per chat) when `[whatsapp] send_typing` is on.
    pub(super) fn composing_after(
        &mut self,
        n: Network,
        chat: Option<String>,
        was_empty: bool,
    ) -> Out {
        let now = self.clock.now_ms;
        let enabled = self.send_typing && n == Network::WhatsApp && was_empty;
        let ns = self.ns_mut(n);
        let Some(chat_id) = chat.filter(|c| ns.open.as_ref() == Some(c)) else {
            return Vec::new();
        };
        if !enabled || ns.draft().is_empty() {
            return Vec::new();
        }
        if !typing::may_send_composing(ns.typed_at.get(&chat_id).copied(), now) {
            return Vec::new();
        }
        ns.typed_at.insert(chat_id.clone(), now);
        ns.composing.insert(chat_id.clone());
        vec![(
            n,
            Command::Typing {
                chat_id,
                active: true,
            },
        )]
    }

    /// Send pressed: when composing went out for this chat, `paused` follows the message.
    pub(super) fn send_and_pause(&mut self, n: Network) -> Out {
        let chat = self.ns(n).open.clone();
        let mut out = self.send(n);
        if let Some(chat_id) = chat {
            if !out.is_empty() && self.ns_mut(n).composing.remove(&chat_id) {
                out.push((
                    n,
                    Command::Typing {
                        chat_id,
                        active: false,
                    },
                ));
            }
        }
        out
    }

    // ---- search (33) ----

    pub(crate) fn search(&self) -> Option<&Search> {
        self.search.as_ref()
    }

    pub(super) fn open_search(&mut self) {
        if self.store.is_none() {
            self.push_global_notice("Search needs the local store: restart chatcli.", true);
            return;
        }
        self.popup = None;
        self.search = Some(Search::default());
    }

    /// Runs the query again after the text changed. A failure is shown in the box and as a notice.
    fn search_rerun(&mut self) {
        let rows = ui::search_geometry(self.layout_in(self.size).body).map_or(1, |g| g.rows);
        let (Some(s), Some(store)) = (self.search.as_mut(), self.store.as_ref()) else {
            return;
        };
        s.run(store);
        s.keep_visible(rows);
        if let Some(e) = s.error.clone() {
            self.push_global_notice(&e, true);
        }
    }

    pub(super) fn search_type(&mut self, text: &str) {
        if let Some(s) = self.search.as_mut() {
            s.query.push_str(text);
            self.search_rerun();
        }
    }

    pub(super) fn search_backspace(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.query.pop();
            self.search_rerun();
        }
    }

    pub(super) fn search_move(&mut self, delta: isize) {
        let rows = ui::search_geometry(self.layout_in(self.size).body).map_or(1, |g| g.rows);
        if let Some(s) = self.search.as_mut() {
            s.step(delta);
            s.keep_visible(rows);
        }
    }

    /// Page keys in the hit list: about a screenful.
    pub(super) fn search_page(&mut self, up: bool) {
        let rows = ui::search_geometry(self.layout_in(self.size).body).map_or(1, |g| g.rows);
        let step = (rows as isize - 1).max(1);
        self.search_move(if up { -step } else { step });
    }

    pub(super) fn search_action(&mut self, action: Action) -> Out {
        match action {
            Action::PopupUp => self.search_move(-1),
            Action::PopupDown => self.search_move(1),
            Action::PopupAccept => return self.search_open_hit(),
            Action::PopupClose => self.search = None,
            _ => {}
        }
        Vec::new()
    }

    /// Enter on a hit: open its chat, like choosing it in the list. The conversation shows the
    /// chat as the backend gives it (jumping to a far-back hit is out of scope).
    fn search_open_hit(&mut self) -> Out {
        let Some(hit) = self.search.as_ref().and_then(|s| s.chosen().cloned()) else {
            return Vec::new();
        };
        self.search = None;
        self.open_hit(&hit)
    }

    fn open_hit(&mut self, hit: &SearchHit) -> Out {
        let n = hit.network;
        self.set_active(n);
        let ns = self.ns(n);
        if ns.panel() {
            self.push_notice(n, "Sign in to open that chat.", false);
            return Vec::new();
        }
        let Some(hidden) = ns
            .chats
            .iter()
            .find(|c| c.id == hit.chat_id)
            .map(|c| c.flags.hidden)
        else {
            self.push_notice(n, "That chat is no longer in your list.", false);
            return Vec::new();
        };
        let ns = self.ns_mut(n);
        ns.filter.clear();
        // A hidden chat has to be on the list to be selected.
        ns.show_hidden |= hidden;
        ns.selected = Some(hit.chat_id.clone());
        ns.focus = Focus::List;
        self.fix_selection(n, true);
        self.show(n, hit.chat_id.clone());
        self.commit(n)
    }

    // ---- events of this stage ----

    /// The events this stage owns. `None` when `ev` is not one of them.
    pub(super) fn handle_everyday(&mut self, ev: Event) -> Option<Event> {
        match ev {
            Event::OlderMessages {
                network,
                chat_id,
                messages,
                at_start,
            } => self.older_arrived(network, chat_id, messages, at_start),
            Event::OlderFailed {
                network,
                chat_id,
                error,
            } => {
                self.ns_mut(network).older.insert(chat_id, Older::Idle);
                self.push_notice(network, &error, true);
            }
            Event::Typing {
                network,
                chat_id,
                sender,
                active,
            } => self.typing_event(network, chat_id, &sender, active),
            other => return Some(other),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use chatcli_core::media::MediaCache;
    use chatcli_core::model::{
        ChatFlags, ChatKind, ChatSummary, MessageState, Status, MUTE_FOREVER,
    };
    use chatcli_core::runtime::UiCtx;
    use chatcli_core::settings::{Density, MediaSettings, NotifyPreview, Settings};
    use chatcli_core::store::Store;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use ratatui::layout::Rect;

    use super::*;
    use crate::keymap::Context;
    use crate::notify::FOLD_MS;
    use crate::text::Clock;

    const T0: i64 = 1_800_000_000_000;

    fn clock() -> Clock {
        Clock {
            now_ms: T0,
            tz: chrono::FixedOffset::east_opt(0),
        }
    }

    fn key_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Out {
        app.handle_key(key_with(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut App, c: char) -> Out {
        press(app, KeyCode::Char(c))
    }

    fn typed(app: &mut App, s: &str) {
        s.chars().for_each(|c| {
            ch(app, c);
        });
    }

    fn chat(net: Network, id: &str, title: &str, ts: i64, unread: u32) -> ChatSummary {
        ChatSummary {
            id: id.into(),
            title: title.into(),
            kind: ChatKind::Direct,
            last_ts: Some(ts),
            unread,
            ..ChatSummary::blank(net)
        }
    }

    fn flagged(mut c: ChatSummary, f: ChatFlags) -> ChatSummary {
        c.flags = f;
        c
    }

    fn online_status(app: &mut App, net: Network) {
        app.handle_event(Event::Status(
            net,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
    }

    /// Teams online with chats a (2 unread), b (0), c (1), newest first.
    fn app_with(settings: &Settings) -> App {
        let mut app = App::with_settings(clock(), settings, None);
        online_status(&mut app, Network::Teams);
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![
                chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 2),
                chat(Network::Teams, "b", "Sam Sample", T0 - 2_000, 0),
                chat(Network::Teams, "c", "Jordan Placeholder", T0 - 3_000, 1),
            ],
        ));
        app
    }

    fn app() -> App {
        app_with(&Settings::default())
    }

    fn message(
        net: Network,
        chat_id: &str,
        id: &str,
        ts: i64,
        sender: &str,
        from_me: bool,
    ) -> ChatMessage {
        ChatMessage {
            chat_id: chat_id.into(),
            id: id.into(),
            ts,
            sender: sender.into(),
            from_me,
            text: "secret plan".into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(net)
        }
    }

    fn live(chat_id: &str, id: &str, ts: i64, from_me: bool) -> Event {
        Event::Message(message(
            Network::Teams,
            chat_id,
            id,
            ts,
            "Sam Sample",
            from_me,
        ))
    }

    fn names(out: &Out) -> Vec<String> {
        out.iter().map(|(_, c)| format!("{c:?}")).collect()
    }

    fn is_load_older(c: &(Network, Command)) -> bool {
        matches!(c.1, Command::LoadOlder { .. })
    }

    // ---- toasts ----

    #[test]
    fn a_background_chat_while_unfocused_gives_one_toast_without_message_text() {
        let mut app = app();
        app.handle_event(live("b", "m1", T0, false));
        let toasts = app.take_toasts();
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Sam Sample");
        assert_eq!(toasts[0].body, "New message", "preview is off by default");
        assert!(toasts[0].sound);
        assert!(app.take_toasts().is_empty(), "taken once");
    }

    #[test]
    fn preview_settings_decide_how_much_text_leaves() {
        for (preview, body) in [
            (NotifyPreview::Off, "New message"),
            (NotifyPreview::Sender, "Sam Sample: new message"),
            (NotifyPreview::Full, "Sam Sample: secret plan"),
        ] {
            let mut s = Settings::default();
            s.notifications.preview = preview;
            let mut app = app_with(&s);
            app.handle_event(live("b", "m1", T0, false));
            assert_eq!(app.take_toasts()[0].body, body, "{preview:?}");
        }
        let mut s = Settings::default();
        s.notifications.sound = false;
        let mut app = app_with(&s);
        app.handle_event(live("b", "m1", T0, false));
        assert!(!app.take_toasts()[0].sound);
    }

    #[test]
    fn no_toast_for_your_own_the_chat_you_watch_stale_muted_or_disabled() {
        let mut app = app();
        app.handle_event(live("b", "own", T0, true));
        assert!(app.take_toasts().is_empty(), "own message");

        // Open chat a and focus the window: a message there is seen, one in b is not.
        press(&mut app, KeyCode::Enter);
        app.set_focus(true);
        app.handle_event(live("a", "m1", T0, false));
        assert!(app.take_toasts().is_empty(), "open chat while focused");
        app.handle_event(live("b", "m2", T0, false));
        assert_eq!(app.take_toasts().len(), 1, "another chat, window focused");
        // The window goes to the background: now chat a gets a toast too.
        app.set_focus(false);
        app.handle_event(live("a", "m3", T0, false));
        assert_eq!(
            app.take_toasts().len(),
            1,
            "open chat in a background window"
        );

        let mut app = self::app();
        app.handle_event(live("b", "old", T0 - 61_000, false));
        assert!(app.take_toasts().is_empty(), "catch-up older than a minute");
        app.handle_event(live("b", "recent", T0 - 59_000, false));
        assert_eq!(app.take_toasts().len(), 1);

        let mut app = self::app();
        let muted = ChatFlags {
            muted_until: Some(MUTE_FOREVER),
            ..ChatFlags::default()
        };
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "b", "Sam Sample", T0 - 2_000, 0),
            muted,
        )));
        app.handle_event(live("b", "m1", T0, false));
        assert!(app.take_toasts().is_empty(), "muted chat");

        let mut s = Settings::default();
        s.notifications.enabled = false;
        let mut app = app_with(&s);
        app.handle_event(live("b", "m1", T0, false));
        assert!(app.take_toasts().is_empty(), "notifications off");
    }

    #[test]
    fn history_and_changes_never_toast() {
        let mut app = app();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "b".into(),
            messages: vec![message(Network::Teams, "b", "h1", T0, "Sam Sample", false)],
        });
        app.handle_event(Event::OlderMessages {
            network: Network::Teams,
            chat_id: "b".into(),
            messages: vec![message(
                Network::Teams,
                "b",
                "h0",
                T0 - 1,
                "Sam Sample",
                false,
            )],
            at_start: false,
        });
        app.handle_event(Event::MessageChanged(message(
            Network::Teams,
            "b",
            "h1",
            T0,
            "Sam Sample",
            false,
        )));
        assert!(app.take_toasts().is_empty());
    }

    #[test]
    fn messages_in_ten_seconds_fold_into_one_toast_when_the_window_ends() {
        let mut app = app();
        app.handle_event(live("b", "m1", T0, false));
        app.handle_event(live("b", "m2", T0, false));
        app.handle_event(live("b", "m3", T0, false));
        assert_eq!(app.take_toasts().len(), 1, "the first is shown at once");
        assert_eq!(app.next_deadline(), Some(T0 + FOLD_MS));
        app.tick(T0 + FOLD_MS - 1);
        app.fire_timers();
        assert!(app.take_toasts().is_empty(), "not before the window ends");
        app.tick(T0 + FOLD_MS);
        app.fire_timers();
        let toasts = app.take_toasts();
        assert_eq!(toasts.len(), 1);
        assert_eq!(
            (toasts[0].title.as_str(), toasts[0].body.as_str()),
            ("Sam Sample", "2 new messages")
        );
        app.tick(T0 + 5 * FOLD_MS);
        app.fire_timers();
        assert!(app.take_toasts().is_empty());
        assert_eq!(app.next_deadline(), None, "nothing left to wake for");
    }

    #[test]
    fn each_chat_has_its_own_ten_seconds() {
        let mut app = app();
        app.handle_event(live("a", "m1", T0, false));
        app.handle_event(live("b", "m2", T0, false));
        assert_eq!(app.take_toasts().len(), 2);
    }

    // ---- older history ----

    /// Chat "a" open with `count` messages one minute apart, focus in the message box.
    fn app_with_history(count: usize) -> App {
        let mut app = app();
        press(&mut app, KeyCode::Enter);
        let msgs = (0..count)
            .map(|i| {
                message(
                    Network::Teams,
                    "a",
                    &format!("m{i:02}"),
                    10_000_000 + i as i64 * 60_000,
                    "Alex Example",
                    false,
                )
            })
            .collect();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: msgs,
        });
        app
    }

    fn older_batch(count: usize) -> Vec<ChatMessage> {
        (0..count)
            .map(|i| {
                message(
                    Network::Teams,
                    "a",
                    &format!("o{i:02}"),
                    10_000_000 - (count - i) as i64 * 60_000,
                    "Alex Example",
                    false,
                )
            })
            .collect()
    }

    fn visible_text(app: &App) -> Vec<String> {
        let (w, h) = app.convo_size(Network::Teams);
        let lines = ui::message_lines(app.ns(Network::Teams), &app.look(), w);
        let scroll = app
            .ns(Network::Teams)
            .scroll
            .min(lines.len().saturating_sub(h));
        let end = lines.len() - scroll;
        lines[end.saturating_sub(h)..end]
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn page_up_until_top(app: &mut App) -> Vec<(Network, Command)> {
        let mut found = Vec::new();
        for _ in 0..15 {
            found.extend(
                press(app, KeyCode::PageUp)
                    .into_iter()
                    .filter(is_load_older),
            );
        }
        found
    }

    #[test]
    fn page_up_at_the_top_asks_for_older_once_and_not_before() {
        let mut app = app_with_history(60);
        // The first page ups only scroll: the top is a few screens away.
        assert!(press(&mut app, KeyCode::PageUp).is_empty());
        assert!(!app.at_top(Network::Teams));
        let found = page_up_until_top(&mut app);
        assert_eq!(
            found,
            vec![(
                Network::Teams,
                Command::LoadOlder {
                    chat_id: "a".into(),
                    before_ts: 10_000_000,
                    before_id: "m00".into(),
                }
            )],
            "one request, for the oldest message held; later page ups wait for the answer"
        );
        assert_eq!(app.ns(Network::Teams).older["a"], Older::Loading);
        let first_row = visible_text(&app)[0].clone();
        assert!(first_row.contains("loading older..."), "{first_row:?}");
    }

    #[test]
    fn two_scrolls_load_two_pages_with_no_duplicates_and_no_jump() {
        let mut app = app_with_history(60);
        page_up_until_top(&mut app);
        let loading = visible_text(&app);
        app.handle_event(Event::OlderMessages {
            network: Network::Teams,
            chat_id: "a".into(),
            // The page overlaps what is held: m00 comes again.
            messages: {
                let mut b = older_batch(30);
                b.push(message(
                    Network::Teams,
                    "a",
                    "m00",
                    10_000_000,
                    "Alex Example",
                    false,
                ));
                b
            },
            at_start: false,
        });
        let held = &app.ns(Network::Teams).messages["a"];
        assert_eq!(held.len(), 90, "60 + 30 new, the overlap added nothing");
        assert!(
            held.windows(2).all(|w| w[0].ts <= w[1].ts),
            "still in time order"
        );
        let mut seen: Vec<&str> = held.iter().map(|m| m.id.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 90);
        // Every message that was on screen stays on the same row. Only the rows above them
        // changed: the note, the day separator that the older page's own separator replaces,
        // and the name row of the oldest held message, which now continues the older page's
        // run from the same person.
        let after = visible_text(&app);
        assert_eq!(loading[2], "Alex Example 02:46", "{loading:?}");
        assert_eq!(&after[3..], &loading[3..], "no jump");
        assert_eq!(app.ns(Network::Teams).older["a"], Older::Idle);

        // Scroll to the top again: the second page is asked for from the new oldest message.
        let again = page_up_until_top(&mut app);
        assert_eq!(
            again,
            vec![(
                Network::Teams,
                Command::LoadOlder {
                    chat_id: "a".into(),
                    before_ts: 10_000_000 - 30 * 60_000,
                    before_id: "o00".into(),
                }
            )]
        );
    }

    #[test]
    fn the_start_of_the_conversation_is_shown_once_and_never_asked_again() {
        let mut app = app_with_history(60);
        page_up_until_top(&mut app);
        app.handle_event(Event::OlderMessages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: older_batch(5),
            at_start: true,
        });
        assert_eq!(app.ns(Network::Teams).older["a"], Older::AtStart);
        assert!(page_up_until_top(&mut app).is_empty(), "no more requests");
        let top = visible_text(&app)[0].clone();
        assert!(top.contains("start of conversation"), "{top:?}");
        let (w, _) = app.convo_size(Network::Teams);
        let all = ui::message_lines(app.ns(Network::Teams), &app.look(), w);
        let notes = all
            .iter()
            .filter(|l| l.to_string().contains("start of conversation"))
            .count();
        assert_eq!(notes, 1);
        // The wheel does not ask either.
        let lay = app.layout_in(app.size);
        let wheel = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: lay.convo.x + 2,
            row: lay.convo.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(wheel.is_empty());
    }

    #[test]
    fn a_failure_is_a_notice_and_the_next_scroll_tries_again() {
        let mut app = app_with_history(60);
        page_up_until_top(&mut app);
        app.handle_event(Event::OlderFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            error: "Teams is busy: wait a moment and scroll up again.".into(),
        });
        let n = app.notices().back().unwrap();
        assert!(n.is_error && n.text.contains("Teams is busy"), "{n:?}");
        assert_eq!(app.ns(Network::Teams).older["a"], Older::Idle);
        assert_eq!(
            page_up_until_top(&mut app).len(),
            1,
            "a retry is a new scroll"
        );
    }

    #[test]
    fn nothing_asks_for_older_without_a_user_scroll() {
        let mut app = app_with_history(5);
        let mut out: Out = Vec::new();
        for i in 0..20 {
            out.extend(app.handle_event(live("a", &format!("n{i}"), T0 + i, false)));
            app.tick(T0 + 5_000 * i);
            out.extend(app.fire_timers());
        }
        out.extend(app.handle_event(Event::Chats(
            Network::Teams,
            vec![chat(Network::Teams, "a", "Alex Example", T0, 0)],
        )));
        app.resize(100, 30);
        assert!(!out.iter().any(is_load_older), "{:?}", names(&out));
        assert!(app.ns(Network::Teams).older.is_empty());
    }

    #[test]
    fn wheel_up_at_the_top_asks_and_selecting_the_first_message_asks() {
        let mut app = app_with_history(5);
        let lay = app.layout_in(app.size);
        let wheel = |app: &mut App| {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: lay.convo.x + 2,
                row: lay.convo.y + 2,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(
            wheel(&mut app).iter().filter(|c| is_load_older(c)).count(),
            1
        );
        assert!(wheel(&mut app).is_empty(), "one at a time");

        let mut app = app_with_history(5);
        app.handle_key(key_with(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.active().focus, Focus::Select);
        let out = press(&mut app, KeyCode::Home);
        assert_eq!(
            out.iter().filter(|c| is_load_older(c)).count(),
            1,
            "{:?}",
            names(&out)
        );
        assert_eq!(app.ns(Network::Teams).sel_msg.as_deref(), Some("m00"));
    }

    // ---- unread ----

    fn selected(app: &App) -> &str {
        app.active().selected.as_deref().unwrap()
    }

    #[test]
    fn n_and_shift_n_walk_the_unread_chats_and_wrap() {
        let mut app = app();
        assert_eq!(selected(&app), "a");
        ch(&mut app, 'n');
        assert_eq!(selected(&app), "c", "b has nothing unread");
        ch(&mut app, 'n');
        assert_eq!(selected(&app), "a", "wraps past the end");
        ch(&mut app, 'N');
        assert_eq!(selected(&app), "c", "and backwards");
        assert_eq!(
            app.active().open.as_deref(),
            Some("c"),
            "it previews like a cursor move"
        );
    }

    #[test]
    fn with_nothing_unread_n_says_so() {
        let mut app = app();
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![chat(Network::Teams, "a", "Alex Example", T0, 0)],
        ));
        ch(&mut app, 'n');
        assert_eq!(app.notices().back().unwrap().text, "No unread chats.");
        assert_eq!(selected(&app), "a");
    }

    #[test]
    fn muted_chats_are_skipped_by_n_and_named_when_they_are_all_there_is() {
        let muted = ChatFlags {
            muted_until: Some(MUTE_FOREVER),
            ..ChatFlags::default()
        };
        let mut app = app();
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![
                chat(Network::Teams, "a", "Alex Example", T0 - 1, 0),
                flagged(chat(Network::Teams, "b", "Sam Sample", T0 - 2, 4), muted),
            ],
        ));
        ch(&mut app, 'n');
        assert_eq!(
            app.notices().back().unwrap().text,
            "Only muted chats have unread messages."
        );
        assert_eq!(selected(&app), "a");
    }

    #[test]
    fn mark_all_read_asks_then_sends_one_mark_read_per_unread_chat() {
        let mut app = app();
        assert!(ch(&mut app, 'M').is_empty());
        assert_eq!(app.prompt(), Some(&Prompt::MarkAllRead { count: 2 }));
        let out = ch(&mut app, 'y');
        assert_eq!(
            out,
            vec![
                (
                    Network::Teams,
                    Command::MarkRead {
                        chat_id: "a".into()
                    }
                ),
                (
                    Network::Teams,
                    Command::MarkRead {
                        chat_id: "c".into()
                    }
                ),
            ]
        );
        assert_eq!(
            app.active().unread(),
            0,
            "sidebar counts and the tab total are cleared"
        );
        assert!(app.active().chats.iter().all(|c| c.unread == 0));
        // Nothing left: a notice, no question.
        ch(&mut app, 'M');
        assert!(app.prompt().is_none());
        assert_eq!(app.notices().back().unwrap().text, "Nothing unread.");
    }

    #[test]
    fn declining_mark_all_read_changes_nothing() {
        let mut app = app();
        ch(&mut app, 'M');
        assert!(ch(&mut app, 'n').is_empty());
        assert!(app.prompt().is_none());
        assert_eq!(app.active().unread(), 3);
        ch(&mut app, 'M');
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert_eq!(app.active().unread(), 3);
    }

    #[test]
    fn mark_all_read_includes_muted_chats() {
        let muted = ChatFlags {
            muted_until: Some(MUTE_FOREVER),
            ..ChatFlags::default()
        };
        let mut app = app();
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![flagged(
                chat(Network::Teams, "m", "Muted Chat", T0, 7),
                muted,
            )],
        ));
        ch(&mut app, 'M');
        let out = ch(&mut app, 'y');
        assert_eq!(out.len(), 1, "every unread chat of the tab, muted too");
    }

    // ---- pin, mute, hide ----

    fn flag_cmd(id: &str, flag: ChatFlag, on: bool) -> Vec<(Network, Command)> {
        vec![(
            Network::Teams,
            Command::SetChatFlag {
                chat_id: id.into(),
                flag,
                on,
            },
        )]
    }

    #[test]
    fn p_m_x_ask_the_backend_to_flip_the_flag_of_the_selected_chat() {
        let mut app = app();
        assert_eq!(ch(&mut app, 'p'), flag_cmd("a", ChatFlag::Pinned, true));
        assert_eq!(ch(&mut app, 'm'), flag_cmd("a", ChatFlag::Muted, true));
        assert_eq!(ch(&mut app, 'x'), flag_cmd("a", ChatFlag::Hidden, true));
        // The list changes only when the backend answers.
        assert!(app
            .active()
            .chats
            .iter()
            .all(|c| c.flags == ChatFlags::default()));
        // Once the chat says it is pinned, p takes the pin off.
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 2),
            ChatFlags {
                pinned: true,
                muted_until: Some(MUTE_FOREVER),
                hidden: false,
            },
        )));
        assert_eq!(ch(&mut app, 'p'), flag_cmd("a", ChatFlag::Pinned, false));
        assert_eq!(ch(&mut app, 'm'), flag_cmd("a", ChatFlag::Muted, false));
    }

    #[test]
    fn pinned_chats_sort_first_and_keep_their_order_inside_each_group() {
        let mut app = app();
        let pin = ChatFlags {
            pinned: true,
            ..ChatFlags::default()
        };
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "c", "Jordan Placeholder", T0 - 3_000, 1),
            pin,
        )));
        let order: Vec<&str> = app.active().chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["c", "a", "b"]);
        // A newer message in b does not lift it over the pinned chat.
        app.handle_event(Event::ChatUpdated(chat(
            Network::Teams,
            "b",
            "Sam Sample",
            T0,
            1,
        )));
        let order: Vec<&str> = app.active().chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["c", "b", "a"]);
    }

    #[test]
    fn muted_unread_is_left_out_of_the_tab_total_and_ends_with_the_mute() {
        let mut app = app();
        assert_eq!(app.active().unread(), 3);
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 2),
            ChatFlags {
                muted_until: Some(MUTE_FOREVER),
                ..ChatFlags::default()
            },
        )));
        assert_eq!(app.active().unread(), 1);
        // A mute with an end time stops counting when the end passes.
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 2),
            ChatFlags {
                muted_until: Some(T0 + 60_000),
                ..ChatFlags::default()
            },
        )));
        assert_eq!(app.active().unread(), 1);
        app.tick(T0 + 60_000);
        assert_eq!(app.active().unread(), 3);
    }

    #[test]
    fn hidden_chats_leave_the_list_and_h_brings_them_back() {
        let mut app = app();
        let hidden = ChatFlags {
            hidden: true,
            ..ChatFlags::default()
        };
        app.handle_event(Event::ChatUpdated(flagged(
            chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 2),
            hidden,
        )));
        let listed = |app: &App| -> Vec<String> {
            let ns = app.active();
            ns.visible()
                .iter()
                .map(|&i| ns.chats[i].id.clone())
                .collect()
        };
        assert_eq!(listed(&app), ["b", "c"]);
        assert_eq!(
            selected(&app),
            "b",
            "the selection moved off the hidden chat"
        );
        assert_eq!(
            app.active().unread(),
            1,
            "hidden chats are not in the total"
        );
        ch(&mut app, 'H');
        assert_eq!(listed(&app), ["a", "b", "c"]);
        // On a hidden chat x un-hides.
        ch(&mut app, 'k');
        assert_eq!(selected(&app), "a");
        assert_eq!(ch(&mut app, 'x'), flag_cmd("a", ChatFlag::Hidden, false));
        ch(&mut app, 'H');
        assert_eq!(listed(&app), ["b", "c"]);
    }

    #[test]
    fn flags_do_nothing_on_a_sign_in_panel() {
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        for c in ['p', 'm', 'x', 'n', 'M', 'H'] {
            assert!(ch(&mut app, c).is_empty(), "{c}");
        }
    }

    // ---- search ----

    fn tmp_media() -> MediaCache {
        let dir =
            std::env::temp_dir().join(format!("chatcli-everyday-test-{}", std::process::id()));
        MediaCache::open(&dir, &MediaSettings::default()).unwrap()
    }

    fn search_app() -> App {
        let store = Store::open_in_memory().unwrap();
        let chats = vec![
            chat(Network::Teams, "a", "Alex Example", T0 - 1_000, 0),
            chat(Network::Teams, "b", "Sam Sample", T0 - 2_000, 0),
            flagged(
                chat(Network::Teams, "h", "Hidden Chat", T0 - 3_000, 0),
                ChatFlags {
                    hidden: true,
                    ..ChatFlags::default()
                },
            ),
        ];
        let wa = chat(Network::WhatsApp, "w", "Riley Sample", T0 - 4_000, 0);
        store.upsert_chats(&chats).unwrap();
        store.upsert_chats(std::slice::from_ref(&wa)).unwrap();
        let mut msgs = vec![
            message(Network::Teams, "a", "a1", 1_000, "Alex Example", false),
            message(Network::Teams, "b", "b1", 3_000, "Sam Sample", false),
            message(Network::Teams, "h", "h1", 2_000, "Sam Sample", false),
            message(Network::WhatsApp, "w", "w1", 4_000, "Riley Sample", false),
        ];
        msgs[0].text = "the budget draft".into();
        msgs[1].text = "budget approved".into();
        msgs[2].text = "budget in the hidden chat".into();
        msgs[3].text = "whatsapp budget note".into();
        store.upsert_messages(&msgs).unwrap();
        let ctx = UiCtx::new(store, tmp_media());
        let mut app = App::with_ctx(clock(), &ctx);
        online_status(&mut app, Network::Teams);
        online_status(&mut app, Network::WhatsApp);
        app.handle_event(Event::Chats(Network::Teams, chats));
        app.handle_event(Event::Chats(Network::WhatsApp, vec![wa]));
        app
    }

    fn ctrl_f(app: &mut App) -> Out {
        app.handle_key(key_with(KeyCode::Char('f'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_f_opens_search_and_typing_finds_newest_first_across_both_networks() {
        let mut app = search_app();
        ctrl_f(&mut app);
        assert!(app.search().is_some());
        assert_eq!(app.context(), Context::Popup);
        typed(&mut app, "budget");
        let hits: Vec<(Network, &str)> = app
            .search()
            .unwrap()
            .hits
            .iter()
            .map(|h| (h.network, h.chat_id.as_str()))
            .collect();
        assert_eq!(
            hits,
            [
                (Network::WhatsApp, "w"),
                (Network::Teams, "b"),
                (Network::Teams, "h"),
                (Network::Teams, "a")
            ]
        );
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.search().unwrap().query, "budge");
        press(&mut app, KeyCode::Esc);
        assert!(app.search().is_none());
        assert_eq!(app.context(), Context::List);
    }

    #[test]
    fn enter_on_a_hit_opens_its_chat_on_its_network() {
        let mut app = search_app();
        ctrl_f(&mut app);
        typed(&mut app, "budget");
        press(&mut app, KeyCode::Down);
        let out = press(&mut app, KeyCode::Enter);
        assert!(app.search().is_none());
        assert_eq!(app.active, Network::Teams);
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        assert_eq!(app.active().open.as_deref(), Some("b"));
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::OpenChat {
                    chat_id: "b".into()
                }
            )]
        );

        let mut app = search_app();
        ctrl_f(&mut app);
        typed(&mut app, "budget");
        let out = press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.active,
            Network::WhatsApp,
            "the first hit is on WhatsApp"
        );
        assert_eq!(
            out,
            vec![(
                Network::WhatsApp,
                Command::OpenChat {
                    chat_id: "w".into()
                }
            )]
        );
    }

    #[test]
    fn a_hit_in_a_hidden_chat_shows_hidden_chats_and_opens_it() {
        let mut app = search_app();
        ctrl_f(&mut app);
        typed(&mut app, "hidden chat");
        press(&mut app, KeyCode::Enter);
        assert!(app.ns(Network::Teams).show_hidden);
        assert_eq!(app.active().open.as_deref(), Some("h"));
    }

    #[test]
    fn hostile_queries_never_break_search_or_raise_a_notice() {
        let mut app = search_app();
        ctrl_f(&mut app);
        for q in ["\"", "*", "NEAR(", "a\"b", "-x", "()", "  "] {
            app.handle_paste(q);
            assert!(app.notices().is_empty(), "{q:?}");
            while !app.search().unwrap().query.is_empty() {
                press(&mut app, KeyCode::Backspace);
            }
        }
        // An empty query shows no hits and a hint.
        assert!(app.search().unwrap().hits.is_empty());
    }

    #[test]
    fn search_without_a_store_says_so() {
        let mut app = app();
        ctrl_f(&mut app);
        assert!(app.search().is_none());
        assert!(app.notices().back().unwrap().is_error);
    }

    #[test]
    fn search_covers_the_screen_so_the_mouse_does_nothing_behind_it() {
        let mut app = search_app();
        ctrl_f(&mut app);
        let before = app.active().selected.clone();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.active().selected, before);
        assert!(app.search().is_some());
    }

    // ---- typing ----

    fn wa_app(settings: &Settings) -> App {
        let mut app = App::with_settings(clock(), settings, None);
        online_status(&mut app, Network::WhatsApp);
        app.handle_event(Event::Chats(
            Network::WhatsApp,
            vec![chat(Network::WhatsApp, "w", "Riley Sample", T0 - 1_000, 0)],
        ));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, Network::WhatsApp);
        app
    }

    fn typing_event(net: Network, who: &str, active: bool) -> Event {
        Event::Typing {
            network: net,
            chat_id: "w".into(),
            sender: who.into(),
            active,
        }
    }

    #[test]
    fn someone_typing_shows_and_clears_on_pause_message_or_ten_seconds() {
        let mut app = wa_app(&Settings::default());
        app.handle_event(typing_event(Network::WhatsApp, "Riley Sample", true));
        let text = |app: &App| {
            app.ns(Network::WhatsApp)
                .typing
                .get("w")
                .and_then(|t| t.text())
        };
        assert_eq!(text(&app).as_deref(), Some("Riley Sample is typing..."));
        app.tick(T0 + 9_999);
        assert!(text(&app).is_some());
        assert!(app.tick(T0 + 10_000), "the redraw that clears it");
        assert_eq!(text(&app), None);

        app.handle_event(typing_event(Network::WhatsApp, "Riley Sample", true));
        app.handle_event(typing_event(Network::WhatsApp, "Riley Sample", false));
        assert_eq!(text(&app), None, "paused");

        app.handle_event(typing_event(Network::WhatsApp, "Riley Sample", true));
        app.handle_event(Event::Message(message(
            Network::WhatsApp,
            "w",
            "m1",
            T0 + 20_000,
            "Riley Sample",
            false,
        )));
        assert_eq!(text(&app), None, "their message ends it");
    }

    #[test]
    fn groups_name_each_sender_and_teams_shows_nothing() {
        let mut app = wa_app(&Settings::default());
        app.handle_event(typing_event(Network::WhatsApp, "Riley Sample", true));
        app.handle_event(typing_event(Network::WhatsApp, "Sam Sample", true));
        assert_eq!(
            app.ns(Network::WhatsApp).typing["w"].text().as_deref(),
            Some("Riley Sample and Sam Sample are typing...")
        );
        app.handle_event(typing_event(Network::Teams, "Alex Example", true));
        assert!(app.ns(Network::Teams).typing.is_empty());
    }

    fn typing_cmd(active: bool) -> (Network, Command) {
        (
            Network::WhatsApp,
            Command::Typing {
                chat_id: "w".into(),
                active,
            },
        )
    }

    #[test]
    fn your_typing_goes_out_on_the_first_key_at_most_every_five_seconds_and_pauses_on_send() {
        let mut s = Settings::default();
        s.whatsapp.send_typing = true;
        let mut app = wa_app(&s);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            ch(&mut app, 'h'),
            vec![typing_cmd(true)],
            "first key of a draft"
        );
        assert!(ch(&mut app, 'i').is_empty(), "later keys send nothing");
        let out = press(&mut app, KeyCode::Enter);
        assert!(matches!(out[0].1, Command::Send { .. }));
        assert_eq!(out[1], typing_cmd(false), "paused follows the message");
        // A new draft inside the 5 s window: no second composing.
        assert!(ch(&mut app, 'a').is_empty());
        app.tick(T0 + 5_000);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            ch(&mut app, 'b'),
            vec![typing_cmd(true)],
            "five seconds later it may"
        );
    }

    #[test]
    fn a_pasted_first_draft_counts_as_typing_too() {
        let mut s = Settings::default();
        s.whatsapp.send_typing = true;
        let mut app = wa_app(&s);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.handle_paste("hello"), vec![typing_cmd(true)]);
    }

    #[test]
    fn with_send_typing_off_or_on_teams_nothing_is_sent() {
        let mut app = wa_app(&Settings::default());
        press(&mut app, KeyCode::Enter);
        assert!(ch(&mut app, 'h').is_empty());
        let out = press(&mut app, KeyCode::Enter);
        assert!(!out.iter().any(|(_, c)| matches!(c, Command::Typing { .. })));

        let mut s = Settings::default();
        s.whatsapp.send_typing = true;
        let mut teams = app_with(&s);
        press(&mut teams, KeyCode::Enter);
        assert!(ch(&mut teams, 'h').is_empty(), "never to Teams");
    }

    // ---- layout ----

    fn click(app: &mut App, col: u16, row: u16) -> Out {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn with_settings(f: impl FnOnce(&mut Settings)) -> Settings {
        let mut s = Settings::default();
        f(&mut s);
        s
    }

    #[test]
    fn row_height_follows_density_and_previews() {
        assert_eq!(app().row_h(), 2);
        assert_eq!(
            app_with(&with_settings(|s| s.layout.density = Density::Compact)).row_h(),
            1
        );
        assert_eq!(
            app_with(&with_settings(|s| s.layout.show_previews = false)).row_h(),
            1
        );
    }

    #[test]
    fn clicking_a_chat_still_works_in_compact_density_and_with_previews_off() {
        for settings in [
            with_settings(|s| s.layout.density = Density::Compact),
            with_settings(|s| s.layout.show_previews = false),
        ] {
            let mut app = app_with(&settings);
            app.resize(100, 30);
            let list = app.layout_in(app.size).list;
            // One row per chat: the third row is the third chat.
            click(&mut app, list.x + 3, list.y + 2);
            assert_eq!(selected(&app), "c");
            click(&mut app, list.x + 3, list.y + 1);
            assert_eq!(selected(&app), "b");
            // Tabs follow the geometry too.
            let tabs = ui::tabs(&app, 100);
            click(&mut app, tabs[1].x + 1, 0);
            assert_eq!(app.active, Network::WhatsApp);
        }
        // Cozy rows are two high.
        let mut app = app();
        app.resize(100, 30);
        let list = app.layout_in(app.size).list;
        click(&mut app, list.x + 3, list.y + 2);
        assert_eq!(selected(&app), "b");
    }

    #[test]
    fn a_compact_sidebar_fits_more_chats_in_the_same_height() {
        let cozy = app();
        let compact = app_with(&with_settings(|s| s.layout.density = Density::Compact));
        assert!(compact.list_rows(Network::Teams) >= 2 * cozy.list_rows(Network::Teams) - 1);
    }

    #[test]
    fn sidebar_width_is_clamped_so_the_message_pane_stays_usable() {
        use chatcli_core::settings::SidebarWidth::{Columns, Percent};
        assert_eq!(ui::sidebar_cols(100, None), 30);
        assert_eq!(ui::sidebar_cols(200, None), 40);
        assert_eq!(ui::sidebar_cols(100, Some(Columns(20))), 20);
        assert_eq!(ui::sidebar_cols(100, Some(Percent(50))), 50);
        assert_eq!(
            ui::sidebar_cols(100, Some(Columns(120))),
            70,
            "30 left for messages"
        );
        assert_eq!(ui::sidebar_cols(100, Some(Percent(70))), 70);
        assert_eq!(
            ui::sidebar_cols(100, Some(Percent(10))),
            16,
            "never under 16"
        );
        assert_eq!(ui::sidebar_cols(60, Some(Columns(120))), 30);
        let app = app_with(&with_settings(|s| {
            s.layout.sidebar_width = Some(Columns(24));
        }));
        assert_eq!(app.layout_in(Rect::new(0, 0, 100, 30)).sidebar.width, 24);
    }

    #[test]
    fn a_request_that_never_left_or_was_lost_in_a_reconnect_can_be_asked_again() {
        let mut app = app_with_history(60);
        let found = page_up_until_top(&mut app);
        assert_eq!(found.len(), 1);
        // The channel to the backend was closed: the run loop hands the command back.
        app.command_undelivered(Network::Teams, &found[0].1);
        assert!(app.ns(Network::Teams).older.is_empty());
        assert_eq!(page_up_until_top(&mut app).len(), 1);
        // A connection that dropped and came back may have lost the answer.
        assert_eq!(app.ns(Network::Teams).older["a"], Older::Loading);
        online_status(&mut app, Network::Teams);
        assert!(app.ns(Network::Teams).older.is_empty());
        assert_eq!(page_up_until_top(&mut app).len(), 1);
    }

    #[test]
    fn the_new_actions_are_listed_in_the_help() {
        let app = app();
        let names: Vec<String> = ui::help_rows(&app)
            .iter()
            .filter_map(|r| match r {
                ui::HelpRow::Entry { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        for want in [
            "Next unread",
            "Prev unread",
            "Mark all read",
            "Pin",
            "Mute",
            "Hide",
            "Show hidden",
            "Search",
        ] {
            assert!(names.iter().any(|n| n == want), "{want}");
        }
    }
}
