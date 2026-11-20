//! Selecting one message anywhere in a chat (issue 77): a search hit, or a place `jump_back`
//! (Ctrl+O) returns to.
//!
//! The TUI holds a chat as one run of messages up to the newest, and the message pane lays all of
//! them out on every draw. A hit far back is therefore not loaded together with everything after
//! it: a window around it comes from the store, and the stretch between that window and the
//! newer messages held stays out (`NetState::gaps`). Moving down to the gap, or up to it from the
//! newer side, reads the next page of it from the store, so the user sees one unbroken
//! conversation. Store reads are local and indexed: a page costs a millisecond or two, so it is
//! done on the UI thread.

use chatcli_core::model::{ChatMessage, Network};

use super::{clean_message, App, Focus};
use crate::older::{self, unconfirmed, Older};
use crate::ui;

/// Messages read per store read: around a hit, and per step into a gap.
pub(crate) const PAGE: u32 = 50;
/// Pages a scroll reads at most to get the gap note off the screen (a tall pane of short lines).
const SCROLL_PAGES: usize = 4;

/// `(ts, id)` of a message, the order the store and the gap use.
fn key(m: &ChatMessage) -> (i64, &str) {
    (m.ts, m.id.as_str())
}

/// Adds store messages to the held ones (a held copy wins), in `(ts, id)` order.
fn merge_in(held: Vec<ChatMessage>, batch: Vec<ChatMessage>) -> Vec<ChatMessage> {
    older::merge_older(held, batch).0
}

/// The first confirmed message of `list` after `from`: where a stretch read from the store
/// ends. Unconfirmed sends have no place in the store's order.
fn first_after<'a>(
    list: impl Iterator<Item = &'a ChatMessage>,
    from: (i64, &str),
) -> Option<(i64, String)> {
    list.filter(|m| !unconfirmed(m) && key(m) > from)
        .map(key)
        .min()
        .map(|(t, i)| (t, i.to_string()))
}

/// What one read of the gap did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fill {
    /// No gap there, so nothing to read.
    Nothing,
    /// A page went in (or the gap closed).
    Read,
    /// The store could not be read: the gap stays, with a notice, and the next key tries again.
    Failed,
}

/// What `load_window` found for the message to select.
enum Window {
    Loaded,
    /// Not in the store (moved or deleted since the search).
    Missing,
    /// In the store but not a message selection can stop on (a failed or pending send).
    Unselectable,
}

/// Where the gap note is in the open chat's laid-out conversation, from one layout.
struct GapView {
    /// The note's line.
    line: usize,
    /// Every line of the conversation.
    total: usize,
}

impl App {
    /// Selects message `id` (sent at `ts`) of the open chat, reading it and the messages around
    /// it from the store when it is not held. With `remember`, the current place goes on the
    /// `jump_back` list first. False, with a notice, when the store does not have it.
    pub(super) fn select_message_at(
        &mut self,
        n: Network,
        id: &str,
        ts: i64,
        remember: bool,
    ) -> bool {
        let Some(chat_id) = self.ns(n).open.clone() else {
            return false;
        };
        let held = |app: &App| {
            app.ns(n)
                .messages
                .get(&chat_id)
                .is_some_and(|l| l.iter().any(|m| m.id == id))
        };
        if !held(self) {
            let refused = match self.load_window(n, &chat_id, id, ts) {
                Err(e) => Some((e, true)),
                Ok(Window::Missing) => Some((
                    "That message is no longer stored on this PC.".to_string(),
                    false,
                )),
                Ok(Window::Unselectable) => {
                    Some(("That message cannot be selected.".to_string(), false))
                }
                Ok(Window::Loaded) => None,
            };
            if let Some((text, is_error)) = refused {
                self.push_notice(n, &text, is_error);
                return false;
            }
        }
        if !self.selectable_ids(n).iter().any(|s| s == id) {
            self.push_notice(n, "That message cannot be selected.", false);
            return false;
        }
        let ns = self.ns(n);
        if remember && ns.focus == Focus::Select {
            if let Some(cur) = ns.sel_msg.clone().filter(|c| c != id) {
                self.push_jump(n, cur);
            }
        }
        let ns = self.ns_mut(n);
        ns.select_on_load = None;
        ns.focus = Focus::Select;
        ns.sel_msg = Some(id.to_string());
        self.ensure_sel_visible(n);
        self.fill_gap_after_select(n, false);
        true
    }

    /// Puts the window around `(ts, id)` in the held messages: `PAGE` messages before it, the
    /// message and up to `PAGE` after it. What is held after the gap (or everything, with no gap)
    /// stays; a window from an earlier jump goes, and `jump_back` reads it again when needed.
    /// Nothing changes when the store does not have the message, or has it but selection cannot
    /// stop on it: the held messages and the selection stay as they were.
    fn load_window(
        &mut self,
        n: Network,
        chat_id: &str,
        id: &str,
        ts: i64,
    ) -> Result<Window, String> {
        let Some(store) = self.store.as_ref() else {
            return Err("Jumping to a message needs the local store: restart chatcli.".into());
        };
        let ns = self.ns(n);
        let old_gap = ns.gaps.get(chat_id).cloned();
        let kept = |m: &ChatMessage| match &old_gap {
            Some((gts, gid)) => unconfirmed(m) || key(m) > (*gts, gid.as_str()),
            None => true,
        };
        // The window runs from the hit to the first kept message after it (or the newest), so
        // a hit inside what is kept, but missing from it, still reads.
        let upper = first_after(
            ns.messages
                .get(chat_id)
                .into_iter()
                .flatten()
                .filter(|m| kept(m)),
            (ts, id),
        );
        let upper = upper.as_ref().map(|(t, i)| (*t, i.as_str()));
        // The hit (included) and after it: one more than a page says whether a gap is left.
        let mut after = store.messages_range(n, chat_id, (ts, id), upper, PAGE + 1)?;
        match after.first() {
            // Not stored (or deleted from the store since the search).
            Some(m) if m.id != id => return Ok(Window::Missing),
            None => return Ok(Window::Missing),
            Some(m) if !super::msg::selectable(m) => return Ok(Window::Unselectable),
            Some(_) => {}
        }
        let gap = (after.len() > PAGE as usize).then(|| {
            after.truncate(PAGE as usize);
            let last = after.last().expect("a full page");
            (last.ts, last.id.clone())
        });
        let mut window = store.messages_before(n, chat_id, ts, id, PAGE)?;
        window.append(&mut after);
        window.iter_mut().for_each(clean_message);
        let ns = self.ns_mut(n);
        let mut held = ns.messages.remove(chat_id).unwrap_or_default();
        held.retain(kept);
        ns.messages
            .insert(chat_id.to_string(), merge_in(held, window));
        match gap {
            Some(g) => ns.gaps.insert(chat_id.to_string(), g),
            None => ns.gaps.remove(chat_id),
        };
        // The oldest message held changed: what lies above it can be asked for again.
        let o = ns.older.entry(chat_id.to_string()).or_default();
        if *o != Older::Loading {
            *o = Older::Idle;
        }
        Ok(Window::Loaded)
    }

    /// Reads the next page of the open chat's gap from the store: its oldest messages going down
    /// (`up` false), its newest going up. What the user looks at stays still: going down the new
    /// lines push the newer messages below the view, going up they go in above it.
    pub(super) fn fill_gap(&mut self, n: Network, up: bool) -> Fill {
        let Some(chat_id) = self.ns(n).open.clone() else {
            return Fill::Nothing;
        };
        let Some((gts, gid)) = self.ns(n).gaps.get(&chat_id).cloned() else {
            return Fill::Nothing;
        };
        let Some(store) = self.store.as_ref() else {
            // No store, nothing to read the gap from: it can never be filled.
            self.ns_mut(n).gaps.remove(&chat_id);
            return Fill::Nothing;
        };
        // The first confirmed message held after the gap: where it ends (`None`: it runs to the
        // newest stored message).
        let upper = first_after(
            self.ns(n).messages.get(&chat_id).into_iter().flatten(),
            (gts, gid.as_str()),
        );
        let page = match (&upper, up) {
            // Up from the newer side: the page just before it, or the newest stored page when
            // only your unconfirmed sends are held after the gap.
            (Some((uts, uid)), true) => store.messages_before(n, &chat_id, *uts, uid, PAGE + 1),
            (None, true) => store.messages_before(n, &chat_id, i64::MAX, "", PAGE + 1),
            // From the message before the gap (included, so the read never comes back empty
            // while the store still has it).
            (_, false) => store.messages_range(
                n,
                &chat_id,
                (gts, &gid),
                upper.as_ref().map(|(t, i)| (*t, i.as_str())),
                PAGE + 2,
            ),
        };
        let mut fresh: Vec<ChatMessage> = match page {
            Ok(p) => p
                .into_iter()
                .filter(|m| key(m) > (gts, gid.as_str()))
                .collect(),
            Err(e) => {
                // The gap stays marked (closing it would hide the messages it holds), and the
                // next key that reaches it tries again.
                self.push_notice(n, &e, true);
                return Fill::Failed;
            }
        };
        // More than a page between the two sides: a gap is left. Going down it now starts after
        // the page; going up it keeps its start and the page is the newest part of it.
        let mut gap = None;
        if fresh.len() > PAGE as usize {
            if up {
                fresh.drain(..fresh.len() - PAGE as usize);
                gap = Some((gts, gid));
            } else {
                fresh.truncate(PAGE as usize);
                let last = fresh.last().expect("a full page");
                gap = Some((last.ts, last.id.clone()));
            }
        }
        fresh.iter_mut().for_each(clean_message);
        let before = self.gap_view(n);
        let ns = self.ns_mut(n);
        let held = ns.messages.remove(&chat_id).unwrap_or_default();
        ns.messages.insert(chat_id.clone(), merge_in(held, fresh));
        match gap {
            Some(g) => ns.gaps.insert(chat_id, g),
            None => ns.gaps.remove(&chat_id),
        };
        // `scroll` counts lines below the view. Going down, everything that changes (the new
        // lines, the note going away) is at the note, so when the note is at or below the view's
        // top the change is counted in and the view keeps its place; it can be a line fewer when
        // the gap closed with nothing left to read. Going up the user looks at the newer
        // messages below the gap, and they stay where they are by themselves.
        if let (Some(b), false) = (before, up) {
            let (_, h) = self.convo_size(n);
            let ns = self.ns(n);
            let view_top = b.total.saturating_sub(ns.scroll).saturating_sub(h);
            if b.line >= view_top {
                let change = self.convo_lines(n) as isize - b.total as isize;
                let ns = self.ns_mut(n);
                ns.scroll = (ns.scroll as isize + change).max(0) as usize;
            }
        }
        Fill::Read
    }

    /// The open chat's gap note and the conversation's length, when it has a gap.
    fn gap_view(&self, n: Network) -> Option<GapView> {
        let ns = self.ns(n);
        let (_, gid) = ns.gaps.get(ns.open.as_deref()?)?;
        let (w, _) = self.convo_size(n);
        let (lines, spans) = ui::message_layout(ns, &self.look(), w);
        let line = spans.iter().find(|s| &s.id == gid)?.end;
        Some(GapView {
            line,
            total: lines.len(),
        })
    }

    /// True when the selection (while selecting in the open chat) is after the gap: a read then
    /// fills the gap from that side, so the selected message stays where it is.
    fn selection_after_gap(&self, n: Network) -> Option<bool> {
        let ns = self.ns(n);
        if ns.focus != Focus::Select {
            return None;
        }
        let chat = ns.open.as_deref()?;
        let (gts, gid) = ns.gaps.get(chat)?;
        let sel = ns.sel_msg.as_deref()?;
        let m = ns.messages.get(chat)?.iter().find(|m| m.id == sel)?;
        Some(unconfirmed(m) || key(m) > (*gts, gid.as_str()))
    }

    /// The gap note is on screen (the user scrolled or moved to it): read pages until it is off
    /// the screen again, or gone, from the side `up` says: the way a scroll went. True when
    /// anything was read.
    pub(super) fn fill_gap_in_view(&mut self, n: Network, up: bool) -> bool {
        let mut read = false;
        for _ in 0..SCROLL_PAGES {
            let Some(g) = self.gap_view(n) else {
                break;
            };
            let (_, h) = self.convo_size(n);
            let bottom = g.total - self.ns(n).scroll.min(g.total.saturating_sub(h));
            let top = bottom.saturating_sub(h);
            if g.line < top || g.line >= bottom || self.fill_gap(n, up) != Fill::Read {
                break;
            }
            read = true;
        }
        read
    }

    /// `fill_gap_in_view` after the selection moved: the gap is read from the side the selection
    /// is on (so it stays where it is), and the selection is shown again if a read moved it. A
    /// wheel scroll goes through `fill_gap_in_view` alone: its direction decides, and the view
    /// stays where the user put it.
    pub(super) fn fill_gap_after_select(&mut self, n: Network, up: bool) {
        let up = self.selection_after_gap(n).unwrap_or(up);
        if self.fill_gap_in_view(n, up) {
            self.ensure_sel_visible(n);
        }
    }

    /// The TUI holds message `id` of `chat_id`.
    pub(super) fn holds(&self, n: Network, chat_id: &str, id: &str) -> bool {
        self.ns(n)
            .messages
            .get(chat_id)
            .is_some_and(|l| l.iter().any(|m| m.id == id))
    }

    /// Whether a live event (`Event::Message`, or `Event::MessageChanged` for a message that is
    /// not held) may put `m` into its chat's held messages. Not when it is older than the oldest
    /// held message (older history loads through its own path, which would take it for its
    /// edge), nor when it falls inside a jump's gap (the gap reads it in its turn; held on its own
    /// it would end the gap early). The backends store it first, so it is not lost either way.
    pub(super) fn takes_live(&self, n: Network, m: &ChatMessage) -> bool {
        if unconfirmed(m) {
            return true;
        }
        let ns = self.ns(n);
        let Some(held) = ns.messages.get(&m.chat_id) else {
            return true;
        };
        if older::request_point(held).is_some_and(|(ts, id)| key(m) < (ts, id.as_str())) {
            return false;
        }
        let Some((gts, gid)) = ns.gaps.get(&m.chat_id) else {
            return true;
        };
        if key(m) <= (*gts, gid.as_str()) {
            return true;
        }
        match first_after(held.iter(), (*gts, gid.as_str())) {
            Some((uts, uid)) => key(m) > (uts, uid.as_str()),
            // Nothing held after the gap: it runs to the newest stored message, so only a message
            // newer than every stored one is outside it. A failed read cannot tell: it waits in
            // the store (the gap reads it in its turn), since held it could end the gap early.
            None => self.store.as_ref().is_none_or(|s| {
                s.messages_range(n, &m.chat_id, key(m), None, 2)
                    .is_ok_and(|r| r.iter().all(|x| key(x) <= key(m)))
            }),
        }
    }

    /// An older page (`Event::OlderMessages`) as it fits the held messages now (while a gap is
    /// open, only what is older than the oldest held message). `None` when it no longer fits there at all: the store
    /// has messages between the two, because a jump swapped the window while the request was out
    /// (merging it would leave an unmarked hole). Both backends read the store first, so a page
    /// that answers the current request always fits.
    pub(super) fn older_page_fit(
        &self,
        n: Network,
        chat_id: &str,
        mut batch: Vec<ChatMessage>,
    ) -> Option<Vec<ChatMessage>> {
        let Some(held) = self.ns(n).messages.get(chat_id) else {
            return Some(batch);
        };
        let Some((ots, oid)) = older::request_point(held) else {
            return Some(batch);
        };
        let oldest = (ots, oid.as_str());
        // With a gap, what is not older than the oldest held would land in it or the window.
        if self.ns(n).gaps.contains_key(chat_id) {
            batch.retain(|m| unconfirmed(m) || key(m) < oldest);
        }
        let Some(newest) = batch
            .iter()
            .filter(|m| !unconfirmed(m) && key(m) < oldest)
            .map(key)
            .max()
        else {
            return Some(batch);
        };
        let Some(store) = self.store.as_ref() else {
            return Some(batch);
        };
        // A failed read cannot tell either: the page is dropped, and the next scroll asks again.
        let adjacent = store
            .messages_range(n, chat_id, newest, Some((ots, &oid)), 2)
            .is_ok_and(|r| r.iter().all(|m| key(m) <= newest));
        adjacent.then_some(batch)
    }

    /// Before a move in selection from `cur` to `to` (indexes into the selectable ids): when it
    /// would step over the gap, read the next page of it from that side. Also when a move `down`
    /// is held at the last message, which the gap follows with nothing after it held: the gap
    /// is where the next messages are. `Read` means the move must be worked out again; `Failed`
    /// means it must not happen (it would skip the gap), and the next key tries again.
    pub(super) fn fill_gap_before_move(
        &mut self,
        n: Network,
        ids: &[String],
        cur: usize,
        to: usize,
        down: bool,
    ) -> Fill {
        let ns = self.ns(n);
        let Some(chat_id) = ns.open.as_deref() else {
            return Fill::Nothing;
        };
        let Some((gts, gid)) = ns.gaps.get(chat_id) else {
            return Fill::Nothing;
        };
        let Some(held) = ns.messages.get(chat_id) else {
            return Fill::Nothing;
        };
        let after_gap = |id: &str| {
            held.iter()
                .find(|m| m.id == id)
                .is_some_and(|m| unconfirmed(m) || key(m) > (*gts, gid.as_str()))
        };
        let (from, dest) = (after_gap(&ids[cur]), after_gap(&ids[to]));
        if from != dest {
            return self.fill_gap(n, from);
        }
        if down && to == cur && cur + 1 == ids.len() && !from {
            return self.fill_gap(n, false);
        }
        Fill::Nothing
    }

    /// The open chat has a gap with no confirmed message held after it: it runs to the newest
    /// stored message, so the newest message held is not the chat's newest.
    pub(super) fn gap_open_ended(&self, n: Network) -> bool {
        let ns = self.ns(n);
        let Some(chat_id) = ns.open.as_deref() else {
            return false;
        };
        let Some((gts, gid)) = ns.gaps.get(chat_id) else {
            return false;
        };
        first_after(
            ns.messages.get(chat_id).into_iter().flatten(),
            (*gts, gid.as_str()),
        )
        .is_none()
    }
}

#[cfg(test)]
mod tests {
    use chatcli_core::media::MediaCache;
    use chatcli_core::model::{ChatKind, ChatSummary, Command, Event, MessageState, Status};
    use chatcli_core::runtime::UiCtx;
    use chatcli_core::settings::MediaSettings;
    use chatcli_core::store::Store;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::text::Clock;

    const T0: i64 = 1_800_000_000_000;
    /// Messages in chat `a`, `m0` oldest; the TUI holds the newest `HELD`.
    const STORED: usize = 300;
    const HELD: usize = 50;

    fn id(i: usize) -> String {
        format!("m{i:03}")
    }

    fn msg(i: usize) -> ChatMessage {
        ChatMessage {
            chat_id: "a".into(),
            id: id(i),
            ts: T0 - 10_000_000 + i as i64 * 1_000,
            sender: "Alex Example".into(),
            text: match i {
                10 => "the needle is here".into(),
                150 => "a haystack word".into(),
                280 => "a missing pebble".into(),
                _ if i % 100 == 5 => format!("marker {i}"),
                _ => format!("line {i}"),
            },
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    /// Teams online with chats `a` (300 stored, the newest 50 held) and `b`.
    fn jump_app() -> App {
        let all: Vec<ChatMessage> = (0..STORED).map(msg).collect();
        let held = all[STORED - HELD..].to_vec();
        jump_app_with(&all, held)
    }

    /// Teams online with chats `a` and `b`: `all` in the store, `held` delivered for `a`.
    fn jump_app_with(all: &[ChatMessage], held: Vec<ChatMessage>) -> App {
        let store = Store::open_in_memory().unwrap();
        let chats: Vec<ChatSummary> = ["a", "b"]
            .iter()
            .map(|c| ChatSummary {
                id: (*c).into(),
                title: format!("Chat {c}"),
                kind: ChatKind::Direct,
                last_ts: Some(T0),
                ..ChatSummary::blank(Network::Teams)
            })
            .collect();
        store.upsert_chats(&chats).unwrap();
        store.upsert_messages(all).unwrap();
        let dir = std::env::temp_dir().join(format!("chatcli-jump-test-{}", std::process::id()));
        let media = MediaCache::open(&dir, &MediaSettings::default()).unwrap();
        let mut app = App::with_ctx(
            Clock {
                now_ms: T0,
                tz: chrono::FixedOffset::east_opt(0),
            },
            &UiCtx::new(store, media),
        );
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, chats));
        if !held.is_empty() {
            app.handle_event(Event::Messages {
                network: Network::Teams,
                chat_id: "a".into(),
                messages: held,
            });
        }
        app
    }

    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Vec<(Network, Command)> {
        app.handle_key(KeyEvent::new(code, mods))
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<(Network, Command)> {
        key(app, code, KeyModifiers::NONE)
    }

    fn search(app: &mut App, words: &str) -> Vec<(Network, Command)> {
        key(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        for c in words.chars() {
            press(app, KeyCode::Char(c));
        }
        press(app, KeyCode::Enter)
    }

    fn held(app: &App) -> Vec<String> {
        app.active().messages["a"]
            .iter()
            .map(|m| m.id.clone())
            .collect()
    }

    fn gap(app: &App) -> Option<String> {
        app.active().gaps.get("a").map(|(_, id)| id.clone())
    }

    fn sel(app: &App) -> Option<&str> {
        app.active().sel_msg.as_deref()
    }

    /// Chat `a` open with its newest message selected.
    fn selecting_newest(app: &mut App) {
        press(app, KeyCode::Enter);
        assert_eq!(sel(app), Some(id(STORED - 1).as_str()));
    }

    #[test]
    fn a_held_hit_is_selected_without_reading_the_store() {
        let mut app = jump_app();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: (0..STORED).map(msg).collect(),
        });
        let out = search(&mut app, "needle");
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::OpenChat {
                    chat_id: "a".into()
                }
            )]
        );
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m010"));
        assert_eq!(held(&app).len(), STORED);
        assert_eq!(gap(&app), None);
        // The hit is on screen.
        let (w, h) = app.convo_size(Network::Teams);
        let (lines, spans) = ui::message_layout(app.active(), &app.look(), w);
        let sp = spans.iter().find(|s| s.id == "m010").unwrap();
        let bottom = lines.len() - app.active().scroll;
        assert!(sp.start >= bottom.saturating_sub(h) && sp.end <= bottom);
    }

    #[test]
    fn a_far_hit_loads_a_window_and_leaves_a_gap_before_the_newer_messages() {
        let mut app = jump_app();
        search(&mut app, "needle");
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m010"));
        // m0..m9 above the hit, the hit and 49 after it, then the 50 held before.
        let want: Vec<String> = (0..60).chain(STORED - HELD..STORED).map(id).collect();
        assert_eq!(held(&app), want);
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        assert_eq!(app.active().older.get("a"), Some(&Older::Idle));
        let (w, _) = app.convo_size(Network::Teams);
        let text: Vec<String> = ui::message_lines(app.active(), &app.look(), w)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.iter().any(|l| l.contains(ui::GAP_NOTE)), "{text:?}");
    }

    #[test]
    fn moving_down_reads_the_gap_page_by_page_until_it_closes() {
        let mut app = jump_app();
        search(&mut app, "needle");
        // Down to the last message before the gap, then one more: the next page is read.
        for _ in 0..60 {
            if sel(&app) == Some("m059") {
                break;
            }
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(sel(&app), Some("m059"), "the selection stopped moving");
        assert!(gap(&app).is_some_and(|g| g.as_str() >= "m059"));
        press(&mut app, KeyCode::Down);
        assert_eq!(sel(&app), Some("m060"), "no message skipped");
        // On down to the newer messages: every message in between, once, in order.
        for _ in 0..STORED {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        assert_eq!(gap(&app), None);
        assert_eq!(held(&app), (0..STORED).map(id).collect::<Vec<_>>());
    }

    #[test]
    fn bottom_goes_to_the_newest_message_without_reading_the_gap() {
        let mut app = jump_app();
        search(&mut app, "needle");
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        assert_eq!(held(&app).len(), 60 + HELD);
        assert!(gap(&app).is_some());
    }

    #[test]
    fn moving_up_from_the_newer_messages_reads_the_gap_from_its_top() {
        let mut app = jump_app();
        search(&mut app, "needle");
        press(&mut app, KeyCode::Char('G'));
        // Every step up is one message: the gap is read from above, nothing is skipped.
        let mut seen = vec![sel(&app).unwrap().to_string()];
        while sel(&app) != Some("m010") {
            press(&mut app, KeyCode::Up);
            seen.push(sel(&app).unwrap().to_string());
            assert!(seen.len() <= STORED, "stuck at {:?}", sel(&app));
        }
        let want: Vec<String> = (10..STORED).rev().map(id).collect();
        assert_eq!(seen, want);
        assert_eq!(gap(&app), None);
        assert_eq!(held(&app), (0..STORED).map(id).collect::<Vec<_>>());
    }

    #[test]
    fn ctrl_o_returns_to_where_the_search_started_in_the_same_chat() {
        let mut app = jump_app();
        selecting_newest(&mut app);
        let out = search(&mut app, "needle");
        assert!(out.is_empty(), "the chat is open already: {out:?}");
        assert_eq!(sel(&app), Some("m010"));
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
    }

    fn notice(app: &App) -> Option<&str> {
        app.current_notice().map(|n| n.text.as_str())
    }

    #[test]
    fn n_and_shift_n_step_through_the_last_search_in_this_chat() {
        // "marker" is in m005, m105 and m205 of chat a, and once in chat b between them.
        let mut all = all();
        let mut in_b = msg(150);
        in_b.chat_id = "b".into();
        in_b.id = "b1".into();
        in_b.text = "marker in another chat".into();
        all.push(in_b);
        let mut app = jump_app_with(&all, all[STORED - HELD..STORED].to_vec());
        selecting_newest(&mut app);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(
            notice(&app),
            Some("No search yet: press / to search this chat.")
        );
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        press(&mut app, KeyCode::Char('/'));
        for c in "marker".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(sel(&app), Some("m205"), "the newest match");
        // n goes older, past what is held, and only in this chat.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(sel(&app), Some("m105"));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(sel(&app), Some("m005"));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(sel(&app), Some("m005"), "nothing older: it stays");
        assert_eq!(notice(&app), Some("No older message matches \"marker\"."));
        // N goes newer.
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(sel(&app), Some("m105"));
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(sel(&app), Some("m205"));
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(notice(&app), Some("No newer message matches \"marker\"."));
        // Each step is a place Ctrl+O comes back to.
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some("m105"));
    }

    #[test]
    fn n_follows_a_ctrl_f_search_into_the_chat_it_opened() {
        let mut app = jump_app();
        search(&mut app, "marker");
        assert_eq!(sel(&app), Some("m205"));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(sel(&app), Some("m105"));
    }

    #[test]
    fn ctrl_o_reads_a_window_a_later_jump_dropped() {
        let mut app = jump_app();
        selecting_newest(&mut app);
        search(&mut app, "needle");
        search(&mut app, "haystack");
        assert_eq!(sel(&app), Some("m150"));
        assert!(
            !held(&app).contains(&"m010".to_string()),
            "the first window went"
        );
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some("m010"), "read again from the store");
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
    }

    #[test]
    fn a_history_batch_that_reaches_the_gap_closes_it() {
        let mut app = jump_app();
        search(&mut app, "needle");
        // A batch from after the gap leaves it.
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: (100..STORED).map(msg).collect(),
        });
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        assert_eq!(sel(&app), Some("m010"), "the selection stays");
        // One from before it fills it.
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: (40..STORED).map(msg).collect(),
        });
        assert_eq!(gap(&app), None);
        assert_eq!(held(&app), (0..STORED).map(id).collect::<Vec<_>>());
        assert_eq!(sel(&app), Some("m010"));
    }

    #[test]
    fn scrolling_down_from_the_message_box_reads_the_gap_and_keeps_the_view() {
        let mut app = jump_app();
        search(&mut app, "needle");
        // To the message box, the view where the selection left it.
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.active().focus, Focus::Composer);
        let before = gap(&app);
        for _ in 0..20 {
            press(&mut app, KeyCode::PageDown);
            if gap(&app) != before {
                break;
            }
        }
        assert_ne!(
            gap(&app),
            before,
            "the page keys reached the gap and read it"
        );
        // The note is not on screen right after a read (it moved down with the new lines), or the
        // gap is gone.
        if let Some(g) = app.gap_view(Network::Teams).map(|g| g.line) {
            let (_, h) = app.convo_size(Network::Teams);
            let total = app.convo_lines(Network::Teams);
            let bottom = total - app.active().scroll.min(total.saturating_sub(h));
            assert!(g >= bottom || g < bottom.saturating_sub(h));
        }
    }

    #[test]
    fn a_hit_the_store_lost_leaves_the_message_box_with_a_notice() {
        let mut app = jump_app();
        key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        for c in "needle".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        app.store
            .as_ref()
            .unwrap()
            .rename_chat(Network::Teams, "a", "elsewhere")
            .unwrap();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.active().open.as_deref(), Some("a"));
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("That message is no longer stored on this PC.")
        );
        assert_eq!(held(&app).len(), HELD, "nothing changed");
    }

    fn all() -> Vec<ChatMessage> {
        (0..STORED).map(msg).collect()
    }

    /// The highlighted message's lines are all inside the message pane.
    fn sel_in_view(app: &App) -> bool {
        let (w, h) = app.convo_size(Network::Teams);
        let (lines, spans) = ui::message_layout(app.active(), &app.look(), w);
        let total = lines.len();
        let bottom = total - app.active().scroll.min(total.saturating_sub(h));
        let sp = spans
            .iter()
            .find(|s| Some(s.id.as_str()) == sel(app))
            .unwrap();
        sp.start >= bottom.saturating_sub(h) && sp.end <= bottom
    }

    /// Review: `G` with the gap note on screen read the gap downwards and pushed the newest
    /// message off the screen. The gap is read from the side the selection is on.
    #[test]
    fn bottom_with_the_gap_note_on_screen_keeps_the_newest_message_in_view() {
        let all = all();
        let mut app = jump_app_with(&all, all[STORED - 2..].to_vec());
        search(&mut app, "needle");
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        assert!(sel_in_view(&app));
        // The note was on screen above it: the messages just above the newest were read, not
        // the window's end of the gap.
        let h = held(&app);
        assert!(h.contains(&id(STORED - 3)), "{h:?}");
        assert!(!h.contains(&id(60)), "{h:?}");
        // And up from there reads on without skipping.
        press(&mut app, KeyCode::Up);
        assert_eq!(sel(&app), Some(id(STORED - 2).as_str()));
        press(&mut app, KeyCode::Up);
        assert_eq!(sel(&app), Some(id(STORED - 3).as_str()));
        assert!(sel_in_view(&app));
    }

    /// Review: a page merged by time alone put a message of the same second before the gap's
    /// last message. Held messages keep the store's `(ts, id)` order.
    #[test]
    fn messages_of_one_second_keep_the_store_order_across_the_gap() {
        let mut all = all();
        all[60].ts = all[59].ts;
        let mut app = jump_app_with(&all, all[STORED - HELD..].to_vec());
        search(&mut app, "needle");
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        for _ in 0..60 {
            if sel(&app) == Some("m060") {
                break;
            }
            press(&mut app, KeyCode::Down);
        }
        let h = held(&app);
        let at = h.iter().position(|i| i == "m059").unwrap();
        assert_eq!(&h[at..at + 3], ["m059", "m060", "m061"]);
    }

    /// Review: reading the gap upwards with nothing confirmed held after it did nothing. It
    /// starts from the newest stored message.
    #[test]
    fn reading_up_with_nothing_after_the_gap_starts_from_the_newest_stored() {
        let all = all();
        let mut app = jump_app_with(&all, Vec::new());
        search(&mut app, "needle");
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        assert_eq!(app.fill_gap(Network::Teams, true), Fill::Read);
        let h = held(&app);
        let newest_page: Vec<String> = (STORED - PAGE as usize..STORED).map(id).collect();
        assert_eq!(h[h.len() - PAGE as usize..], newest_page[..]);
        assert_eq!(h.len(), 60 + PAGE as usize);
        assert_eq!(gap(&app).as_deref(), Some("m059"), "the rest is still out");
    }

    /// Review: a hit inside the held stretch but missing from it read an empty range and said
    /// the message was not stored.
    #[test]
    fn a_hit_missing_from_the_held_messages_is_read_into_place() {
        let all = all();
        let held_but_one: Vec<ChatMessage> = all[STORED - HELD..]
            .iter()
            .filter(|m| m.id != "m280")
            .cloned()
            .collect();
        let mut app = jump_app_with(&all, held_but_one);
        search(&mut app, "pebble");
        assert_eq!(sel(&app), Some("m280"));
        assert_eq!(gap(&app), None);
        assert_eq!(held(&app), (230..STORED).map(id).collect::<Vec<_>>());
    }

    /// Review: a gap that closed with nothing left to read lost its note line, and the view
    /// moved a line. The change is counted in either way.
    #[test]
    fn a_gap_that_closes_with_nothing_to_read_keeps_the_view_still() {
        let all = all();
        let mut app = jump_app_with(&all, all.clone());
        search(&mut app, "needle");
        // A gap where nothing is missing (the store has nothing in it), its note on screen.
        let m = &all[279];
        app.ns_mut(Network::Teams)
            .gaps
            .insert("a".into(), (m.ts, m.id.clone()));
        let g = app.gap_view(Network::Teams).unwrap();
        let (w, h) = app.convo_size(Network::Teams);
        app.ns_mut(Network::Teams).scroll = g.total - (g.line + h / 2);
        let shown = |app: &App| {
            let lines = ui::message_lines(app.active(), &app.look(), w);
            let bottom = lines.len() - app.active().scroll;
            lines[bottom - h..bottom - h / 2]
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
        };
        let before = shown(&app);
        assert_eq!(app.fill_gap(Network::Teams, false), Fill::Read);
        assert_eq!(gap(&app), None);
        assert_eq!(shown(&app), before, "what was above the note did not move");
    }

    #[test]
    fn jump_back_reads_the_selectable_messages_again_after_each_place() {
        let mut app = jump_app();
        selecting_newest(&mut app);
        search(&mut app, "needle");
        search(&mut app, "haystack");
        // Places: newest (held), m010 (dropped with its window). Ctrl+O reads m010 back, then
        // the newest is still held and selected.
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some("m010"));
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        assert!(sel_in_view(&app));
    }

    /// Review: an edit or reaction for a message in the gap was inserted on its own, and the gap
    /// took it for its edge and closed early, skipping the rest.
    #[test]
    fn a_change_to_a_message_in_the_gap_is_not_inserted_and_nothing_is_skipped() {
        let mut app = jump_app();
        search(&mut app, "needle");
        let mut edited = msg(150);
        edited.text = "edited".into();
        edited.edited = true;
        app.handle_event(Event::MessageChanged(edited));
        assert!(!held(&app).contains(&"m150".to_string()));
        for _ in 0..STORED {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(held(&app), (0..STORED).map(id).collect::<Vec<_>>());
        assert_eq!(gap(&app), None);
    }

    /// Review: an older page asked for before a jump swapped the window was merged into the new
    /// one, leaving an unmarked hole. It is dropped; a page that fits is merged as before.
    #[test]
    fn an_older_page_for_a_replaced_window_is_dropped() {
        let mut app = jump_app();
        search(&mut app, "haystack");
        assert_eq!(held(&app).first().map(String::as_str), Some("m100"));
        let older = |from: usize, to: usize| Event::OlderMessages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: (from..to).map(msg).collect(),
            at_start: false,
        };
        // m000..m049: the store has m050..m099 between it and m100.
        app.handle_event(older(0, 50));
        assert_eq!(held(&app).first().map(String::as_str), Some("m100"));
        assert_eq!(app.active().older.get("a"), Some(&Older::Idle));
        // m050..m099 ends right above m100: merged.
        app.handle_event(older(50, 100));
        assert_eq!(held(&app).first().map(String::as_str), Some("m050"));
    }

    /// Review: history batches and live messages were sorted by time alone, so messages of one
    /// second kept the backend's order instead of the store's.
    #[test]
    fn messages_of_one_second_are_held_in_store_order_whatever_the_batch_order() {
        let mut app = jump_app();
        let mut late = msg(STORED - 1);
        late.id = "m299a".into();
        let mut early = msg(STORED - 1);
        early.id = "m299b".into();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![early, late],
        });
        let h = held(&app);
        assert_eq!(&h[h.len() - 3..], ["m299", "m299a", "m299b"]);
    }

    /// Review: a live message timed inside the gap (an offline redelivery) was held on its own,
    /// and the gap took it for its edge and closed early. It waits in the store for the gap.
    #[test]
    fn a_live_message_timed_inside_the_gap_waits_for_it() {
        let mut app = jump_app();
        search(&mut app, "needle");
        app.handle_event(Event::Message(msg(150)));
        assert!(!held(&app).contains(&"m150".to_string()));
        for _ in 0..STORED {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(held(&app), (0..STORED).map(id).collect::<Vec<_>>());
        assert_eq!(gap(&app), None);
    }

    /// Review: a redelivered stored message (`MessageChanged` for one that is not held) was
    /// dropped. It goes in when it is newer than what is held; not above it.
    #[test]
    fn a_redelivered_message_goes_in_after_what_is_held_but_not_above_it() {
        let all = all();
        let mut app = jump_app_with(&all, all[STORED - HELD..STORED - 1].to_vec());
        app.handle_event(Event::MessageChanged(msg(STORED - 1)));
        assert_eq!(
            held(&app).last().map(String::as_str),
            Some(id(STORED - 1).as_str())
        );
        app.handle_event(Event::MessageChanged(msg(100)));
        assert!(!held(&app).contains(&"m100".to_string()));
        assert_eq!(held(&app).len(), HELD);
    }

    /// Review: a wheel scroll read the gap from the side the selection was on, not the side it
    /// scrolled from, and moved what the user was looking at.
    #[test]
    fn a_wheel_scroll_reads_the_gap_from_the_side_it_comes_from() {
        let mut app = jump_app();
        search(&mut app, "needle");
        // Selecting m010 (before the gap), the view at the newest messages, then scrolling up.
        app.ns_mut(Network::Teams).scroll = 0;
        for _ in 0..500 {
            if held(&app).contains(&id(249)) {
                break;
            }
            app.scroll_by(Network::Teams, super::super::WHEEL_LINES);
        }
        let h = held(&app);
        assert!(h.contains(&id(249)), "read from the newer side");
        assert!(!h.contains(&id(60)), "not from the window's side");
        assert_eq!(sel(&app), Some("m010"), "the selection did not move");
    }

    /// CodeRabbit: an older-history answer for a window a jump has since replaced was applied:
    /// an empty "at the start" marked the new window as the start of the chat for good.
    #[test]
    fn a_stale_at_the_start_answer_does_not_end_older_history() {
        let mut app = jump_app();
        selecting_newest(&mut app);
        // `g` at the top asks for older history from m250.
        let out = press(&mut app, KeyCode::Char('g'));
        assert!(
            out.iter()
                .any(|(_, c)| matches!(c, Command::LoadOlder { .. })),
            "{out:?}"
        );
        // A jump replaces the window before the answer comes.
        search(&mut app, "needle");
        app.handle_event(Event::OlderMessages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: Vec::new(),
            at_start: true,
        });
        assert_eq!(app.active().older.get("a"), Some(&Older::Idle));
        assert_eq!(held(&app).first().map(String::as_str), Some("m000"));
    }

    /// CodeRabbit: a hit selection cannot stop on (a failed send) swapped the window first, then
    /// said it could not be selected. Nothing changes now.
    #[test]
    fn a_hit_selection_cannot_stop_on_changes_nothing() {
        let mut all = all();
        let mut failed = msg(10);
        failed.id = "f10".into();
        failed.from_me = true;
        failed.state = MessageState::Failed("offline".into());
        all.push(failed.clone());
        let mut app = jump_app_with(&all, all[STORED - HELD..STORED].to_vec());
        selecting_newest(&mut app);
        assert!(!app.select_message_at(Network::Teams, "f10", failed.ts, true));
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("That message cannot be selected.")
        );
        assert_eq!(held(&app).len(), HELD, "the window stayed");
        assert_eq!(gap(&app), None);
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
    }

    /// CodeRabbit: with nothing held after the gap, `G` stopped at the window's end, not the
    /// chat's newest message.
    #[test]
    fn bottom_with_nothing_held_after_the_gap_goes_to_the_newest_stored() {
        let all = all();
        let mut app = jump_app_with(&all, Vec::new());
        search(&mut app, "needle");
        assert_eq!(gap(&app).as_deref(), Some("m059"));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(sel(&app), Some(id(STORED - 1).as_str()));
        assert!(sel_in_view(&app));
    }

    /// CodeRabbit: Down at the last message held, with the gap after it and nothing held past
    /// the gap, stayed put when the gap note was not on screen.
    #[test]
    fn down_at_the_last_held_message_reads_the_gap_after_it() {
        let all = all();
        let mut app = jump_app_with(&all, Vec::new());
        search(&mut app, "needle");
        // Straight to the gap's edge, the view not following (so no read from the view).
        app.ns_mut(Network::Teams).sel_msg = Some("m059".into());
        app.ns_mut(Network::Teams).scroll = 10_000;
        press(&mut app, KeyCode::Down);
        assert_eq!(sel(&app), Some("m060"));
    }
}
