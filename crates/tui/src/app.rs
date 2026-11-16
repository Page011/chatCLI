//! All screen state and input handling. Pure: no terminal, no network. Every handler returns
//! the commands the caller must deliver to the backends, so tests drive the whole app by
//! calling methods and reading what comes back.

use std::collections::{HashMap, VecDeque};

use chatcli_core::model::{
    ChatMessage, ChatSummary, Command, Event, MessageState, Network, Status,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::text::{clean_line, clean_msg, Clock};
use crate::ui;

/// Commands to deliver, each to the backend of the named network.
pub type Out = Vec<(Network, Command)>;

const MAX_NOTICES: usize = 50;
/// How long the newest notice stays in the footer. A fixed error must not stay red for hours
/// under a tab that is LIVE again; the log keeps the full history.
const NOTICE_TTL_MS: i64 = 30_000;
/// Width under which only one pane (list or conversation) fits on screen.
pub(crate) const NARROW: u16 = 60;
const WHEEL_LINES: isize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Filter,
    Composer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub network: Network,
    pub text: String,
    pub is_error: bool,
    /// Clock time when it arrived.
    pub at_ms: i64,
}

pub(crate) struct NetState {
    pub network: Network,
    pub status: Status,
    /// Newest first.
    pub chats: Vec<ChatSummary>,
    /// Per chat id, oldest first.
    pub messages: HashMap<String, Vec<ChatMessage>>,
    pub selected: Option<String>,
    pub open: Option<String>,
    pub focus: Focus,
    pub filter: String,
    /// Unsent text per chat id, so switching chats never sends it to the wrong place.
    pub drafts: HashMap<String, String>,
    /// Message pane lines hidden below the view (0 = newest visible).
    pub scroll: usize,
    /// First chat shown in the sidebar.
    pub list_top: usize,
}

impl NetState {
    fn new(network: Network) -> NetState {
        NetState {
            network,
            status: Status::Starting,
            chats: Vec::new(),
            messages: HashMap::new(),
            selected: None,
            open: None,
            focus: Focus::List,
            filter: String::new(),
            drafts: HashMap::new(),
            scroll: 0,
            list_top: 0,
        }
    }

    pub fn unread(&self) -> u32 {
        self.chats.iter().map(|c| c.unread).sum()
    }

    /// The sign-in panel replaces the chat view while the network is not signed in.
    pub fn panel(&self) -> bool {
        matches!(
            self.status,
            Status::NeedsLogin { .. } | Status::LoggingIn | Status::Qr { .. }
        )
    }

    /// Indexes into `chats` that pass the filter.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.chats.len())
            .filter(|&i| needle.is_empty() || self.chats[i].title.to_lowercase().contains(&needle))
            .collect()
    }

    /// Position of the selected chat within `visible()`; 0 when nothing valid is selected.
    fn sel_pos(&self, vis: &[usize]) -> usize {
        self.selected
            .as_ref()
            .and_then(|id| vis.iter().position(|&i| &self.chats[i].id == id))
            .unwrap_or(0)
    }

    pub fn draft(&self) -> &str {
        self.open
            .as_ref()
            .and_then(|id| self.drafts.get(id))
            .map_or("", |s| s.as_str())
    }

    fn draft_mut(&mut self) -> Option<&mut String> {
        let id = self.open.clone()?;
        Some(self.drafts.entry(id).or_default())
    }
}

pub struct App {
    nets: [NetState; 2],
    active: Network,
    notices: VecDeque<Notice>,
    size: Rect,
    clock: Clock,
    next_client: u64,
    quit: bool,
}

fn idx(n: Network) -> usize {
    match n {
        Network::Teams => 0,
        Network::WhatsApp => 1,
    }
}

fn sort_chats(chats: &mut [ChatSummary]) {
    // Newest first; stable, so chats without messages keep the backend's order at the end.
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_ts));
}

fn clean_chat(c: &mut ChatSummary) {
    c.title = clean_line(&c.title);
    c.last_preview = clean_line(&c.last_preview);
}

fn clean_message(m: &mut ChatMessage) {
    m.sender = clean_line(&m.sender);
    m.text = clean_msg(&m.text);
    if let MessageState::Failed(reason) = &m.state {
        m.state = MessageState::Failed(clean_line(reason));
    }
}

/// Insert or replace by id, keeping the list ordered by time.
fn upsert_message(list: &mut Vec<ChatMessage>, m: ChatMessage) {
    match list.iter_mut().find(|x| x.id == m.id) {
        Some(slot) => *slot = m,
        None => list.push(m),
    }
    list.sort_by_key(|x| x.ts);
}

impl App {
    pub fn new(clock: Clock) -> App {
        App {
            nets: [
                NetState::new(Network::Teams),
                NetState::new(Network::WhatsApp),
            ],
            active: Network::Teams,
            notices: VecDeque::new(),
            size: Rect::new(0, 0, 80, 24),
            clock,
            next_client: 0,
            quit: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.size = Rect::new(0, 0, width, height);
        for n in Network::ALL {
            self.ensure_visible(n);
        }
    }

    /// Advances the clock. True when the minute changed, so times on screen need a redraw.
    pub fn tick(&mut self, now_ms: i64) -> bool {
        let before = self.clock.minute();
        let notice_before = self.current_notice().map(|n| n.at_ms);
        self.clock.now_ms = now_ms;
        let notice_changed = self.current_notice().map(|n| n.at_ms) != notice_before;
        if notice_changed {
            self.relayout();
        }
        self.clock.minute() != before || notice_changed
    }

    /// The footer's height depends on what it shows, so the visible chat rows can change.
    fn relayout(&mut self) {
        for n in Network::ALL {
            self.ensure_visible(n);
        }
    }

    /// The newest notice while it is fresh, unless the status already says the same thing
    /// (a start-up failure sends both, and showing both prints the sentence twice).
    pub(crate) fn current_notice(&self) -> Option<&Notice> {
        let n = self.notices.back()?;
        if self.clock.now_ms - n.at_ms >= NOTICE_TTL_MS {
            return None;
        }
        let said = match &self.ns(n.network).status {
            Status::Error { message } => Some(message),
            Status::NeedsLogin { reason } => reason.as_ref(),
            _ => None,
        };
        (said != Some(&n.text)).then_some(n)
    }

    pub fn push_notice(&mut self, network: Network, text: &str, is_error: bool) {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(Notice {
            network,
            text: clean_line(text),
            is_error,
            at_ms: self.clock.now_ms,
        });
        self.relayout();
    }

    pub(crate) fn ns(&self, n: Network) -> &NetState {
        &self.nets[idx(n)]
    }

    fn ns_mut(&mut self, n: Network) -> &mut NetState {
        &mut self.nets[idx(n)]
    }

    pub(crate) fn active(&self) -> &NetState {
        self.ns(self.active)
    }

    #[cfg(test)]
    pub(crate) fn notices(&self) -> &VecDeque<Notice> {
        &self.notices
    }

    pub(crate) fn clock(&self) -> &Clock {
        &self.clock
    }

    // ---- geometry shared with drawing ----

    /// On a narrow terminal the conversation replaces the list while the composer has focus.
    fn wants_convo(&self, n: Network) -> bool {
        let ns = self.ns(n);
        ns.focus == Focus::Composer && ns.open.is_some()
    }

    fn layout_for(&self, n: Network) -> ui::Layout {
        self.layout_in_for(n, self.size)
    }

    fn layout_in_for(&self, n: Network, area: Rect) -> ui::Layout {
        let ns = self.ns(n);
        ui::layout(
            area,
            ui::footer_height(self, area),
            self.wants_convo(n),
            ns.focus == Focus::Filter || !ns.filter.is_empty(),
        )
    }

    /// Layout of the active tab for the frame being drawn.
    pub(crate) fn layout_in(&self, area: Rect) -> ui::Layout {
        self.layout_in_for(self.active, area)
    }

    /// Message pane (width, height) as it is when the conversation is on screen.
    fn convo_size(&self) -> (usize, usize) {
        let m = ui::layout(self.size, ui::footer_height(self, self.size), true, false).messages;
        (m.width as usize, m.height as usize)
    }

    fn convo_lines(&self, n: Network) -> usize {
        ui::message_lines(self.ns(n), &self.clock, self.convo_size().0).len()
    }

    fn scroll_by(&mut self, n: Network, delta: isize) {
        let max = self.convo_lines(n).saturating_sub(self.convo_size().1) as isize;
        let ns = self.ns_mut(n);
        ns.scroll = (ns.scroll as isize + delta).clamp(0, max) as usize;
    }

    fn list_rows(&self, n: Network) -> usize {
        (self.layout_for(n).list.height as usize / 2).max(1)
    }

    /// Scrolls the sidebar just enough to show the selected chat.
    fn ensure_visible(&mut self, n: Network) {
        let rows = self.list_rows(n);
        let ns = self.ns_mut(n);
        let vis = ns.visible();
        let pos = ns.sel_pos(&vis);
        if pos < ns.list_top {
            ns.list_top = pos;
        } else if pos >= ns.list_top + rows {
            ns.list_top = pos + 1 - rows;
        }
        ns.list_top = ns.list_top.min(vis.len().saturating_sub(rows));
    }

    /// Keeps `selected` pointing at a visible chat.
    fn fix_selection(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        let vis = ns.visible();
        let valid = ns
            .selected
            .as_ref()
            .is_some_and(|id| vis.iter().any(|&i| &ns.chats[i].id == id));
        if !valid {
            ns.selected = vis.first().map(|&i| ns.chats[i].id.clone());
        }
        self.ensure_visible(n);
    }

    // ---- backend events ----

    pub fn handle_event(&mut self, ev: Event) -> Out {
        let mut out = Vec::new();
        match ev {
            Event::Status(n, mut status) => {
                match &mut status {
                    Status::NeedsLogin { reason: Some(r) } => *r = clean_line(r),
                    Status::Online {
                        detail: Some(d), ..
                    } => *d = clean_line(d),
                    Status::Error { message } => *message = clean_line(message),
                    _ => {}
                }
                self.ns_mut(n).status = status;
            }
            Event::Chats(n, mut chats) => {
                chats.iter_mut().for_each(clean_chat);
                sort_chats(&mut chats);
                self.ns_mut(n).chats = chats;
                self.read_open_chat(n, &mut out);
                self.fix_selection(n);
            }
            Event::ChatUpdated(mut chat) => {
                clean_chat(&mut chat);
                let n = chat.network;
                let ns = self.ns_mut(n);
                match ns.chats.iter_mut().find(|c| c.id == chat.id) {
                    Some(slot) => *slot = chat,
                    None => ns.chats.push(chat),
                }
                sort_chats(&mut ns.chats);
                self.read_open_chat(n, &mut out);
                self.fix_selection(n);
            }
            Event::Messages {
                network,
                chat_id,
                mut messages,
            } => {
                messages.iter_mut().for_each(clean_message);
                messages.sort_by_key(|m| m.ts);
                self.ns_mut(network).messages.insert(chat_id, messages);
            }
            Event::Message(mut m) => {
                clean_message(&mut m);
                let n = m.network;
                let chat_id = m.chat_id.clone();
                let from_me = m.from_me;
                let is_open = self.ns(n).open.as_ref() == Some(&chat_id);
                // A reader scrolled up keeps their place: grow the offset by the lines added.
                let before = (is_open && self.ns(n).scroll > 0).then(|| self.convo_lines(n));
                upsert_message(self.ns_mut(n).messages.entry(chat_id).or_default(), m);
                if let Some(before) = before {
                    let added = self.convo_lines(n).saturating_sub(before);
                    self.ns_mut(n).scroll += added;
                }
                if is_open && !from_me {
                    let ns = self.ns_mut(n);
                    if let Some(c) = ns
                        .chats
                        .iter_mut()
                        .find(|c| Some(&c.id) == ns.open.as_ref())
                    {
                        c.unread = 0;
                    }
                    if let Some(chat_id) = ns.open.clone() {
                        out.push((n, Command::MarkRead { chat_id }));
                    }
                }
            }
            Event::MessageConfirmed {
                client_id,
                mut message,
            } => {
                clean_message(&mut message);
                let list = self
                    .ns_mut(message.network)
                    .messages
                    .entry(message.chat_id.clone())
                    .or_default();
                list.retain(|m| m.id != client_id);
                upsert_message(list, message);
            }
            Event::MessageFailed {
                network,
                chat_id,
                client_id,
                error,
            } => {
                let reason = clean_line(&error);
                let found = self
                    .ns_mut(network)
                    .messages
                    .get_mut(&chat_id)
                    .and_then(|l| l.iter_mut().find(|m| m.id == client_id));
                if let Some(m) = found {
                    m.state = MessageState::Failed(reason);
                }
            }
            Event::Notice {
                network,
                text,
                is_error,
            } => self.push_notice(network, &text, is_error),
        }
        self.relayout();
        out
    }

    /// The open chat is on screen, so anything unread in it is read: zero the badge and tell
    /// the backend once per update that carried an unread count.
    fn read_open_chat(&mut self, n: Network, out: &mut Out) {
        let ns = self.ns_mut(n);
        let Some(open) = ns.open.clone() else { return };
        if let Some(c) = ns.chats.iter_mut().find(|c| c.id == open && c.unread > 0) {
            c.unread = 0;
            out.push((n, Command::MarkRead { chat_id: open }));
        }
    }

    // ---- keyboard ----

    pub fn handle_key(&mut self, key: KeyEvent) -> Out {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.quit = true;
                return Vec::new();
            }
            KeyCode::Tab => return self.switch_by(1),
            KeyCode::BackTab => return self.switch_by(-1),
            KeyCode::Char(c @ '1'..='9') if alt => {
                if let Some(&n) = Network::ALL.get(c as usize - '1' as usize) {
                    self.active = n;
                }
                return Vec::new();
            }
            _ => {}
        }
        let n = self.active;
        let plain = !ctrl && !alt;
        if self.ns(n).panel() {
            return self.key_panel(n, key.code, plain);
        }
        match self.ns(n).focus {
            Focus::List => self.key_list(n, key.code, plain),
            Focus::Filter => self.key_filter(n, key.code, plain),
            Focus::Composer => self.key_composer(n, key.code, plain),
        }
    }

    fn switch_by(&mut self, step: isize) -> Out {
        let len = Network::ALL.len() as isize;
        self.active = Network::ALL[(idx(self.active) as isize + step).rem_euclid(len) as usize];
        Vec::new()
    }

    fn key_panel(&mut self, n: Network, code: KeyCode, plain: bool) -> Out {
        match code {
            KeyCode::Enter if matches!(self.ns(n).status, Status::NeedsLogin { .. }) => {
                vec![(n, Command::Login)]
            }
            KeyCode::Char('q') if plain => {
                self.quit = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn key_list(&mut self, n: Network, code: KeyCode, plain: bool) -> Out {
        match code {
            KeyCode::Up => self.move_sel(n, -1),
            KeyCode::Down => self.move_sel(n, 1),
            KeyCode::Char('k') if plain => self.move_sel(n, -1),
            KeyCode::Char('j') if plain => self.move_sel(n, 1),
            KeyCode::Enter => {
                let ns = self.ns(n);
                let vis = ns.visible();
                let Some(&i) = vis.get(ns.sel_pos(&vis)) else {
                    return Vec::new();
                };
                let id = ns.chats[i].id.clone();
                return self.open_chat(n, id);
            }
            KeyCode::Char('/') if plain => self.ns_mut(n).focus = Focus::Filter,
            KeyCode::Char('r') if plain => return vec![(n, Command::Refresh)],
            KeyCode::Char('q') if plain => self.quit = true,
            KeyCode::Esc => {
                if !self.ns(n).filter.is_empty() {
                    self.ns_mut(n).filter.clear();
                    self.fix_selection(n);
                }
            }
            KeyCode::PageUp => self.page(n, 1),
            KeyCode::PageDown => self.page(n, -1),
            _ => {}
        }
        Vec::new()
    }

    fn key_filter(&mut self, n: Network, code: KeyCode, plain: bool) -> Out {
        match code {
            KeyCode::Up => self.move_sel(n, -1),
            KeyCode::Down => self.move_sel(n, 1),
            KeyCode::Enter => self.ns_mut(n).focus = Focus::List,
            KeyCode::Esc => {
                let ns = self.ns_mut(n);
                ns.filter.clear();
                ns.focus = Focus::List;
                self.fix_selection(n);
            }
            KeyCode::Backspace => {
                self.ns_mut(n).filter.pop();
                self.fix_selection(n);
            }
            KeyCode::Char(c) if plain => {
                self.ns_mut(n).filter.push(c);
                self.fix_selection(n);
            }
            _ => {}
        }
        Vec::new()
    }

    fn key_composer(&mut self, n: Network, code: KeyCode, plain: bool) -> Out {
        match code {
            KeyCode::Enter => return self.send(n),
            KeyCode::Esc => self.ns_mut(n).focus = Focus::List,
            KeyCode::Backspace => {
                if let Some(d) = self.ns_mut(n).draft_mut() {
                    d.pop();
                }
            }
            KeyCode::Char(c) if plain => {
                if let Some(d) = self.ns_mut(n).draft_mut() {
                    d.push(c);
                }
            }
            KeyCode::PageUp => self.page(n, 1),
            KeyCode::PageDown => self.page(n, -1),
            _ => {}
        }
        Vec::new()
    }

    /// Pasted text goes into the composer or filter with newlines flattened: a pasted
    /// multi-line block must never turn into several sent messages.
    pub fn handle_paste(&mut self, text: &str) {
        let n = self.active;
        let flat = clean_line(text);
        match self.ns(n).focus {
            Focus::Composer => {
                if let Some(d) = self.ns_mut(n).draft_mut() {
                    d.push_str(&flat);
                }
            }
            Focus::Filter => {
                self.ns_mut(n).filter.push_str(&flat);
                self.fix_selection(n);
            }
            Focus::List => {}
        }
    }

    fn page(&mut self, n: Network, dir: isize) {
        let step = self.convo_size().1.saturating_sub(1).max(1) as isize;
        self.scroll_by(n, dir * step);
    }

    fn move_sel(&mut self, n: Network, delta: isize) {
        let ns = self.ns_mut(n);
        let vis = ns.visible();
        if vis.is_empty() {
            return;
        }
        let pos = (ns.sel_pos(&vis) as isize + delta).clamp(0, vis.len() as isize - 1) as usize;
        ns.selected = Some(ns.chats[vis[pos]].id.clone());
        self.ensure_visible(n);
    }

    fn open_chat(&mut self, n: Network, chat_id: String) -> Out {
        let ns = self.ns_mut(n);
        ns.open = Some(chat_id.clone());
        ns.selected = Some(chat_id.clone());
        ns.focus = Focus::Composer;
        ns.scroll = 0;
        if let Some(c) = ns.chats.iter_mut().find(|c| c.id == chat_id) {
            c.unread = 0;
        }
        self.ensure_visible(n);
        vec![
            (
                n,
                Command::OpenChat {
                    chat_id: chat_id.clone(),
                },
            ),
            (n, Command::MarkRead { chat_id }),
        ]
    }

    fn send(&mut self, n: Network) -> Out {
        let text = self.ns(n).draft().trim().to_string();
        let Some(chat_id) = self.ns(n).open.clone() else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }
        self.next_client += 1;
        // The clock makes ids unique across runs: Teams stores a failed send under this id, and
        // a repeat of "local-1" would overwrite or delete the earlier run's NOT SENT message.
        let client_id = format!("local-{}-{}", self.clock.now_ms, self.next_client);
        let msg = ChatMessage {
            network: n,
            chat_id: chat_id.clone(),
            id: client_id.clone(),
            ts: self.clock.now_ms,
            sender: "You".to_string(),
            from_me: true,
            text: text.clone(),
            state: MessageState::Pending,
        };
        let ns = self.ns_mut(n);
        ns.drafts.remove(&chat_id);
        ns.scroll = 0;
        upsert_message(ns.messages.entry(chat_id.clone()).or_default(), msg);
        vec![(
            n,
            Command::Send {
                chat_id,
                text,
                client_id,
            },
        )]
    }

    // ---- mouse ----

    pub fn handle_mouse(&mut self, m: MouseEvent) -> Out {
        let pos = Position::new(m.column, m.row);
        let n = self.active;
        let lay = self.layout_for(n);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if lay.header.contains(pos) {
                    let hit = ui::tabs(self, self.size.width)
                        .into_iter()
                        .find(|t| pos.x >= t.x && pos.x < t.x + t.w);
                    if let Some(t) = hit {
                        self.active = t.net;
                    }
                } else if !self.ns(n).panel() && lay.list.contains(pos) {
                    let ns = self.ns(n);
                    let row = ns.list_top + (pos.y - lay.list.y) as usize / 2;
                    if let Some(&i) = ns.visible().get(row) {
                        let id = ns.chats[i].id.clone();
                        return self.open_chat(n, id);
                    }
                } else if !self.ns(n).panel()
                    && lay.composer.contains(pos)
                    && self.ns(n).open.is_some()
                {
                    self.ns_mut(n).focus = Focus::Composer;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if !self.ns(n).panel() => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if lay.list.contains(pos) {
                    let rows = self.list_rows(n);
                    let ns = self.ns_mut(n);
                    let max = ns.visible().len().saturating_sub(rows);
                    ns.list_top = if up {
                        ns.list_top.saturating_sub(1)
                    } else {
                        (ns.list_top + 1).min(max)
                    };
                } else if lay.convo.contains(pos) {
                    self.scroll_by(n, if up { WHEEL_LINES } else { -WHEEL_LINES });
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::model::ChatKind;
    use crossterm::event::KeyEventState;

    fn clock() -> Clock {
        Clock {
            now_ms: 1_800_000_000_000,
            tz: chrono::FixedOffset::east_opt(0),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn chat(n: Network, id: &str, title: &str, ts: i64, unread: u32) -> ChatSummary {
        ChatSummary {
            network: n,
            id: id.into(),
            title: title.into(),
            kind: ChatKind::Direct,
            last_ts: Some(ts),
            last_preview: "hi".into(),
            unread,
        }
    }

    fn online_app() -> App {
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![
                chat(Network::Teams, "a", "Alex Example", 2000, 2),
                chat(Network::Teams, "b", "Sam Sample", 1000, 0),
            ],
        ));
        app
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn open_chat_sends_open_and_mark_read_and_clears_badge() {
        let mut app = online_app();
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            out,
            vec![
                (
                    Network::Teams,
                    Command::OpenChat {
                        chat_id: "a".into()
                    }
                ),
                (
                    Network::Teams,
                    Command::MarkRead {
                        chat_id: "a".into()
                    }
                ),
            ]
        );
        assert_eq!(app.active().unread(), 0);
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn send_confirm_and_fail_flow() {
        let mut app = online_app();
        let first_id = format!("local-{}-1", app.clock().now_ms);
        let second_id = format!("local-{}-2", app.clock().now_ms);
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hello there");
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::Send {
                    chat_id: "a".into(),
                    text: "hello there".into(),
                    client_id: first_id.clone()
                }
            )]
        );
        let list = &app.ns(Network::Teams).messages["a"];
        assert_eq!(list[0].state, MessageState::Pending);
        assert_eq!(app.active().draft(), "");

        let mut real = list[0].clone();
        real.id = "real-1".into();
        real.state = MessageState::Sent;
        app.handle_event(Event::MessageConfirmed {
            client_id: first_id,
            message: real,
        });
        let list = &app.ns(Network::Teams).messages["a"];
        assert_eq!(
            (list.len(), list[0].id.as_str(), &list[0].state),
            (1, "real-1", &MessageState::Sent)
        );

        type_text(&mut app, "second");
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(Event::MessageFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            client_id: second_id,
            error: "boom".into(),
        });
        let list = &app.ns(Network::Teams).messages["a"];
        assert_eq!(list[1].state, MessageState::Failed("boom".into()));
    }

    #[test]
    fn client_ids_differ_between_runs() {
        let first_run = {
            let mut app = online_app();
            app.handle_key(key(KeyCode::Enter));
            type_text(&mut app, "x");
            app.handle_key(key(KeyCode::Enter))
        };
        let mut later = online_app();
        later.tick(later.clock().now_ms + 60_000);
        later.handle_key(key(KeyCode::Enter));
        type_text(&mut later, "x");
        let second_run = later.handle_key(key(KeyCode::Enter));
        let id = |out: &Vec<(Network, Command)>| match &out[0].1 {
            Command::Send { client_id, .. } => client_id.clone(),
            other => panic!("{other:?}"),
        };
        assert_ne!(id(&first_run), id(&second_run));
    }

    #[test]
    fn empty_draft_sends_nothing() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "   ");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn incoming_message_in_open_chat_marks_read() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        let m = ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: "m1".into(),
            ts: 3000,
            sender: "Alex Example".into(),
            from_me: false,
            text: "ping".into(),
            state: MessageState::Sent,
        };
        let out = app.handle_event(Event::Message(m));
        assert!(out.contains(&(
            Network::Teams,
            Command::MarkRead {
                chat_id: "a".into()
            }
        )));
    }

    #[test]
    fn sign_in_panel_enter_logs_in() {
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![(Network::Teams, Command::Login)]
        );
        app.handle_event(Event::Status(Network::Teams, Status::LoggingIn));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn refresh_and_tab_switch() {
        let mut app = online_app();
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            vec![(Network::Teams, Command::Refresh)]
        );
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active, Network::WhatsApp);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.active, Network::Teams);
    }

    #[test]
    fn filter_selects_first_match_and_clears() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "sam");
        assert_eq!(app.active().visible().len(), 1);
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.active().visible().len(), 2);
    }

    #[test]
    fn quit_keys() {
        let mut app = online_app();
        app.handle_key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(app.should_quit());
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit());
        // q while typing is text, not quit.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit());
    }

    #[test]
    fn key_release_is_ignored() {
        let mut app = online_app();
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        app.handle_key(k);
        assert!(!app.should_quit());
    }

    #[test]
    fn notices_keep_last_fifty() {
        let mut app = App::new(clock());
        for i in 0..60 {
            app.handle_event(Event::Notice {
                network: Network::Teams,
                text: format!("n{i}"),
                is_error: false,
            });
        }
        assert_eq!(app.notices().len(), 50);
        assert_eq!(app.notices().back().unwrap().text, "n59");
        assert_eq!(app.notices().front().unwrap().text, "n10");
    }

    #[test]
    fn notice_expires_and_status_duplicates_are_hidden() {
        let mut app = App::new(clock());
        let t0 = app.clock().now_ms;
        app.push_notice(Network::Teams, "relay is down", true);
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("relay is down")
        );
        assert!(!app.tick(t0 + NOTICE_TTL_MS - 1000) || app.current_notice().is_some());
        assert!(app.tick(t0 + NOTICE_TTL_MS), "redraw when the notice goes");
        assert!(app.current_notice().is_none());
        // A start-up failure sends the same sentence as notice and status: show it once.
        app.push_notice(Network::Teams, "Could not reach Teams: offline", true);
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Error {
                message: "Could not reach Teams: offline".into(),
            },
        ));
        assert!(app.current_notice().is_none());
    }

    #[test]
    fn network_text_is_sanitised() {
        let mut app = App::new(clock());
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![chat(Network::Teams, "a", "Evil\u{1b}[2JName", 1, 0)],
        ));
        assert!(!app.ns(Network::Teams).chats[0].title.contains('\u{1b}'));
    }

    #[test]
    fn paste_is_flattened_into_the_draft() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        app.handle_paste("one\ntwo");
        assert_eq!(app.active().draft(), "one two");
    }

    #[test]
    fn chat_updates_resort_and_keep_selection() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        app.handle_event(Event::ChatUpdated(chat(
            Network::Teams,
            "b",
            "Sam Sample",
            5000,
            1,
        )));
        assert_eq!(app.active().chats[0].id, "b");
        assert_eq!(app.active().selected.as_deref(), Some("b"));
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_click_tab_and_chat() {
        let mut app = online_app();
        let tabs = ui::tabs(&app, 80);
        app.handle_mouse(click(tabs[1].x + 1, 0));
        assert_eq!(app.active, Network::WhatsApp);
        app.handle_mouse(click(tabs[0].x + 1, 0));
        assert_eq!(app.active, Network::Teams);
        // Sidebar rows are two lines tall and start below the header and the border.
        let out = app.handle_mouse(click(5, 4));
        assert_eq!(
            out[0],
            (
                Network::Teams,
                Command::OpenChat {
                    chat_id: "b".into()
                }
            )
        );
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn mouse_wheel_scrolls_the_conversation() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        let msgs: Vec<ChatMessage> = (0..80)
            .map(|i| ChatMessage {
                network: Network::Teams,
                chat_id: "a".into(),
                id: format!("m{i}"),
                ts: 1000 + i,
                sender: "Alex Example".into(),
                from_me: false,
                text: "line".into(),
                state: MessageState::Sent,
            })
            .collect();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: msgs,
        });
        let wheel = |kind| MouseEvent {
            kind,
            column: 50,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.active().scroll, 3);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.active().scroll, 0);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.active().scroll, 0);
        // A message arriving while scrolled up must not move the view.
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        let before = app.active().scroll;
        let mut late = app.ns(Network::Teams).messages["a"][0].clone();
        late.id = "new".into();
        late.ts = 99_999;
        app.handle_event(Event::Message(late));
        assert!(app.active().scroll > before);
    }

    #[test]
    fn tick_reports_minute_change_only() {
        let mut app = App::new(clock());
        let now = app.clock().now_ms;
        assert!(!app.tick(now + 1000));
        assert!(app.tick(now + 120_000));
    }
}
