//! What you can do to a message and to the message box around it: select a message (issue 11),
//! reply to it (12), react (13), edit or delete your own (27), and complete `@mentions` (28).
//! Pure App behaviour, like the rest of `app`: no terminal, no network. `ui` draws the result.

use chatcli_core::model::{ChatMessage, Command, MessageState, Network, PersonRef, QuotedRef};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use unicode_segmentation::UnicodeSegmentation;

use super::{too_long, App, Focus, Out, Prompt};
use crate::composer::{self, Composer};
use crate::keymap::Action;
use crate::links;
use crate::popup::{Popup, PopupItem};
use crate::text::clean_line;
use crate::ui;
use chatcli_core::limits;

/// Longest quote preview (the contract says 120 characters).
const PREVIEW_MAX: usize = 120;
/// Most members the mention list shows.
const MENTION_ITEMS: usize = 8;
/// A mention query longer than this is ordinary text that happens to follow an `@`.
const MENTION_QUERY_MAX: usize = 40;
/// Most members kept per chat (the store caps its cache the same way).
const MEMBERS_MAX: usize = 1024;

/// What the message box is for besides a plain message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// The next send quotes this message.
    Reply(QuotedRef),
    /// The box holds the text of one of your messages; Enter sends it as an edit. The draft
    /// that was in the box is kept and comes back when the edit ends.
    Edit {
        message_id: String,
        original: String,
        saved: Composer,
    },
}

/// A message the selection may stop on: one the network has confirmed and nobody deleted.
/// Pending and failed sends have only a local id, and a deleted message has nothing to act on.
pub(crate) fn selectable(m: &ChatMessage) -> bool {
    matches!(m.state, MessageState::Sent)
}

/// The nearest selectable message to `from` (the one before it first, then the one after);
/// the newest selectable when `from` is not in the list.
pub(crate) fn nearest_selectable(list: &[ChatMessage], from: Option<&str>) -> Option<String> {
    let pos = from.and_then(|id| list.iter().position(|m| m.id == id));
    let found = match pos {
        Some(p) => list[..p]
            .iter()
            .rev()
            .find(|m| selectable(m))
            .or_else(|| list[p + 1..].iter().find(|m| selectable(m))),
        None => list.iter().rev().find(|m| selectable(m)),
    };
    found.map(|m| m.id.clone())
}

/// One line of plain text for a quote: whitespace folded, at most `PREVIEW_MAX` characters
/// (clusters, so an emoji is never cut), with "..." when it was cut.
pub(crate) fn preview_of(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let gs: Vec<&str> = flat.graphemes(true).collect();
    if gs.len() <= PREVIEW_MAX {
        return flat;
    }
    format!("{}...", gs[..PREVIEW_MAX - 3].concat())
}

/// The quote a reply to `m` carries.
fn quote_of(m: &ChatMessage) -> QuotedRef {
    QuotedRef {
        id: m.id.clone(),
        sender: if m.from_me {
            "You".to_string()
        } else {
            m.sender.clone()
        },
        sender_id: m.sender_id.clone(),
        preview: preview_of(&m.text),
    }
}

/// The text typed after an `@` that starts a word, when the cursor is inside such a mention:
/// `Some("")` right after the `@`, `Some("Alex E")` further on. `None` for an `@` inside a word
/// (an email address), a query that ends in a space (the mention is finished) or one that is
/// too long to be a name.
pub(crate) fn mention_query(before_cursor: &str) -> Option<&str> {
    let line = before_cursor.rsplit('\n').next().unwrap_or(before_cursor);
    let at = line.rfind('@')?;
    let head = &line[..at];
    if !head.is_empty() && !head.ends_with(char::is_whitespace) {
        return None;
    }
    let query = &line[at + 1..];
    if query.starts_with(char::is_whitespace)
        || query.ends_with(char::is_whitespace)
        || query.chars().count() > MENTION_QUERY_MAX
    {
        return None;
    }
    Some(query)
}

/// Members whose name starts with the query or has a word that does ("ex" finds "Alex Example"
/// through "Example"), name-start matches first. At most `limit`.
pub(crate) fn suggest_members(members: &[PersonRef], query: &str, limit: usize) -> Vec<PopupItem> {
    let q = query.to_lowercase();
    let mut hits: Vec<(u8, String, &PersonRef)> = members
        .iter()
        .filter_map(|p| {
            let name = p.name.to_lowercase();
            let rank = if name.starts_with(&q) {
                0
            } else if name
                .char_indices()
                .any(|(i, _)| name[..i].ends_with(char::is_whitespace) && name[i..].starts_with(&q))
            {
                1
            } else {
                return None;
            };
            Some((rank, name, p))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    hits.into_iter()
        .take(limit)
        .map(|(_, _, p)| PopupItem {
            label: p.name.clone(),
            detail: String::new(),
            // The trailing space ends the mention, so the list does not reopen on it.
            insert: format!("@{} ", p.name),
            person: Some(p.clone()),
            command: None,
        })
        .collect()
}

/// How many places `jump_back` remembers per network.
const JUMPS_KEPT: usize = 50;

impl App {
    // ---- selection (issue 11) ----

    fn selectable_ids(&self, n: Network) -> Vec<String> {
        let ns = self.ns(n);
        ns.open
            .as_ref()
            .and_then(|id| ns.messages.get(id))
            .map(|l| {
                l.iter()
                    .filter(|m| selectable(m))
                    .map(|m| m.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The highlighted message, while selecting.
    pub(crate) fn selected_message(&self, n: Network) -> Option<&ChatMessage> {
        let ns = self.ns(n);
        let id = ns.sel_msg.as_ref()?;
        ns.open
            .as_ref()
            .and_then(|c| ns.messages.get(c))?
            .iter()
            .find(|m| &m.id == id)
    }

    /// `select_messages` (no default key; Esc from the message box does the same): select the newest
    /// message of the open chat. From the chat list this
    /// also opens the chat, like Enter. An empty chat stays as it was, with a notice.
    pub(super) fn enter_select(&mut self, n: Network) -> Out {
        let ids = self.selectable_ids(n);
        let Some(newest) = ids.last().cloned() else {
            self.push_notice(n, "No message to select in this chat.", false);
            return Vec::new();
        };
        let mut out = Vec::new();
        if self.ns(n).focus != Focus::Composer {
            out = self.commit(n);
        }
        let ns = self.ns_mut(n);
        ns.focus = Focus::Select;
        ns.sel_msg = Some(newest);
        self.ensure_sel_visible(n);
        out
    }

    /// Highlights the newest message of the open chat. False, and nothing changes, when it has no
    /// message to select.
    pub(crate) fn try_select(&mut self, n: Network) -> bool {
        let Some(newest) = self.selectable_ids(n).last().cloned() else {
            return false;
        };
        let ns = self.ns_mut(n);
        ns.focus = Focus::Select;
        ns.sel_msg = Some(newest);
        self.ensure_sel_visible(n);
        true
    }

    /// Messages for `chat_id` arrived: if it was entered from the list before any were loaded
    /// (`select_on_load`) and is still in its message box, its newest message is selected now.
    /// A batch with nothing selectable yet keeps it waiting.
    pub(super) fn select_when_loaded(&mut self, n: Network, chat_id: &str) {
        let ns = self.ns(n);
        if ns.select_on_load.as_deref() != Some(chat_id) {
            return;
        }
        if ns.open.as_deref() != Some(chat_id) || ns.focus != Focus::Composer {
            self.ns_mut(n).select_on_load = None;
            return;
        }
        if self.try_select(n) {
            self.ns_mut(n).select_on_load = None;
        }
    }

    /// Back from selection: the chat list, with the chat still shown.
    pub(crate) fn select_to_list(&mut self, n: Network) {
        self.leave_select(n);
        self.back_to_list(n);
    }

    /// To the message box.
    pub(super) fn leave_select(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        ns.focus = Focus::Composer;
        ns.sel_msg = None;
        if matches!(self.prompt, Some(Prompt::Delete { .. })) {
            self.prompt = None;
        }
    }

    /// Moves the highlight. Page keys move by about a screenful of lines, always at least one
    /// message; every move ends with the highlight in view.
    pub(super) fn select_move(&mut self, n: Network, action: Action) -> Out {
        let ids = self.selectable_ids(n);
        if ids.is_empty() {
            return Vec::new();
        }
        let cur = self
            .ns(n)
            .sel_msg
            .as_ref()
            .and_then(|s| ids.iter().position(|i| i == s))
            .unwrap_or(ids.len() - 1);
        let to = match action {
            Action::Up => cur.saturating_sub(1),
            Action::Down => (cur + 1).min(ids.len() - 1),
            Action::Top => 0,
            Action::Bottom => ids.len() - 1,
            Action::ScreenTop | Action::ScreenMiddle | Action::ScreenBottom => {
                // The messages whose first line is on screen.
                let (w, h) = self.convo_size(n);
                let (lines, spans) = ui::message_layout(self.ns(n), &self.look(), w);
                let total = lines.len();
                let bottom = total - self.ns(n).scroll.min(total.saturating_sub(h));
                let top = bottom.saturating_sub(h);
                let shown: Vec<usize> = ids
                    .iter()
                    .enumerate()
                    .filter(|(_, id)| {
                        spans
                            .iter()
                            .any(|s| &s.id == *id && s.start >= top && s.start < bottom)
                    })
                    .map(|(i, _)| i)
                    .collect();
                match (shown.first(), shown.last()) {
                    (Some(&first), Some(&last)) => match action {
                        Action::ScreenTop => first,
                        Action::ScreenBottom => last,
                        _ => shown[(shown.len() - 1) / 2],
                    },
                    _ => cur,
                }
            }
            _ => {
                let (w, h) = self.convo_size(n);
                let (_, spans) = ui::message_layout(self.ns(n), &self.look(), w);
                let height = |id: &str| {
                    spans
                        .iter()
                        .find(|s| s.id == id)
                        .map_or(1, |s| s.end - s.start)
                };
                let budget = h.saturating_sub(1).max(1);
                let (mut i, mut used) = (cur, 0);
                if action == Action::PageUp {
                    while i > 0 && used < budget {
                        i -= 1;
                        used += height(&ids[i]);
                    }
                } else {
                    while i + 1 < ids.len() && used < budget {
                        i += 1;
                        used += height(&ids[i]);
                    }
                }
                i
            }
        };
        let big = matches!(
            action,
            Action::Top
                | Action::Bottom
                | Action::ScreenTop
                | Action::ScreenMiddle
                | Action::ScreenBottom
        );
        if big && to != cur {
            self.push_jump(n, ids[cur].clone());
        }
        self.ns_mut(n).sel_msg = Some(ids[to].clone());
        self.ensure_sel_visible(n);
        // Up, page up or first at the oldest message held: the user wants more history.
        let wants_older = to == 0 && matches!(action, Action::Up | Action::PageUp | Action::Top);
        if wants_older {
            self.request_older(n)
        } else {
            Vec::new()
        }
    }

    /// True while selecting in the open chat `chat_id` with its newest message highlighted: new
    /// messages then take the highlight with them (`follow_newest`).
    pub(super) fn selecting_newest(&self, n: Network, chat_id: &str) -> bool {
        let ns = self.ns(n);
        ns.focus == Focus::Select
            && ns.open.as_deref() == Some(chat_id)
            && ns.sel_msg.is_some()
            && ns.sel_msg.as_ref() == self.selectable_ids(n).last()
    }

    /// Highlights the newest message that can be selected, the pane pinned to the bottom like a
    /// chat that follows (a new message taller than the pane shows its end, not its start).
    pub(super) fn follow_newest(&mut self, n: Network) {
        if let Some(last) = self.selectable_ids(n).last().cloned() {
            let ns = self.ns_mut(n);
            ns.sel_msg = Some(last);
            ns.scroll = 0;
        }
    }

    /// Message lines below the end of the highlighted message, when one is highlighted in the
    /// open chat. A change to the messages that alters it is added to `scroll` so the highlight
    /// stays where it was on screen.
    pub(super) fn lines_below_selection(&self, n: Network) -> Option<usize> {
        let ns = self.ns(n);
        let id = ns.sel_msg.as_ref().filter(|_| ns.focus == Focus::Select)?;
        let (w, _) = self.convo_size(n);
        let (lines, spans) = ui::message_layout(ns, &self.look(), w);
        let sp = spans.iter().find(|s| &s.id == id)?;
        Some(lines.len().saturating_sub(sp.end))
    }

    /// Remembers `from` (in the open chat) before a big jump. The list keeps the last 50.
    fn push_jump(&mut self, n: Network, from: String) {
        let ns = self.ns_mut(n);
        let Some(chat) = ns.open.clone() else {
            return;
        };
        ns.jumps.push((chat, from));
        if ns.jumps.len() > JUMPS_KEPT {
            ns.jumps.remove(0);
        }
    }

    /// `jump_back` (Ctrl+O): the newest remembered place in this chat whose message can still be
    /// selected. Places in other chats and messages that went away are dropped on the way.
    pub(super) fn jump_back(&mut self, n: Network) {
        let ids = self.selectable_ids(n);
        let Some(open) = self.ns(n).open.clone() else {
            return;
        };
        // This chat's places, newest first; a message that went away is dropped, other chats'
        // places stay for when that chat is open again.
        let jumps = &mut self.ns_mut(n).jumps;
        while let Some(i) = jumps.iter().rposition(|(chat, _)| *chat == open) {
            let (_, id) = jumps.remove(i);
            if ids.contains(&id) {
                self.ns_mut(n).sel_msg = Some(id);
                self.ensure_sel_visible(n);
                return;
            }
        }
        self.push_notice(n, "No earlier place to jump back to.", false);
    }

    /// Scrolls the message pane just enough to show the highlighted message (its first lines
    /// when it is taller than the pane).
    pub(super) fn ensure_sel_visible(&mut self, n: Network) {
        let Some(id) = self.ns(n).sel_msg.clone() else {
            return;
        };
        let (w, h) = self.convo_size(n);
        let (lines, spans) = ui::message_layout(self.ns(n), &self.look(), w);
        let Some(sp) = spans.iter().find(|s| s.id == id) else {
            return;
        };
        let total = lines.len();
        let max = total.saturating_sub(h);
        let scroll = self.ns(n).scroll.min(max);
        // The view shows lines `bottom - h .. bottom`.
        let bottom = total - scroll;
        let top = bottom.saturating_sub(h);
        let new = if sp.start < top || sp.end - sp.start > h {
            total.saturating_sub(sp.start + h)
        } else if sp.end > bottom {
            total - sp.end
        } else {
            scroll
        };
        self.ns_mut(n).scroll = new.min(max);
    }

    /// The held messages of `chat_id` changed (history, an edit, a delete): the highlight must
    /// still be on a message that can be selected. A message that went away (deleted by its
    /// sender, say) hands the highlight to its nearest neighbour; with none left, selection ends.
    pub(super) fn keep_selection(&mut self, n: Network, chat_id: &str) {
        let ns = self.ns(n);
        if ns.open.as_deref() != Some(chat_id) {
            return;
        }
        let list = ns.messages.get(chat_id).map_or(&[][..], Vec::as_slice);
        let has = |id: &String| list.iter().any(|m| &m.id == id && selectable(m));
        // A delete question about a message that is gone has nothing left to ask.
        let stale_prompt =
            matches!(&self.prompt, Some(Prompt::Delete { message_id, .. }) if !has(message_id));
        let lost = ns.focus == Focus::Select && !ns.sel_msg.as_ref().is_some_and(has);
        let next = lost
            .then(|| nearest_selectable(list, ns.sel_msg.as_deref()))
            .flatten();
        if stale_prompt {
            self.prompt = None;
        }
        if !lost {
            return;
        }
        match next {
            Some(id) => {
                self.ns_mut(n).sel_msg = Some(id);
                self.ensure_sel_visible(n);
            }
            None => self.leave_select(n),
        }
    }

    // ---- per-message actions: reply, react, edit, delete, links ----

    pub(super) fn message_action(&mut self, n: Network, action: Action) {
        let Some(m) = self.selected_message(n).cloned() else {
            return;
        };
        let Some(chat_id) = self.ns(n).open.clone() else {
            return;
        };
        match action {
            Action::Reply => {
                self.cancel_target(n);
                let ns = self.ns_mut(n);
                ns.targets.insert(chat_id, Target::Reply(quote_of(&m)));
                self.leave_select(n);
            }
            Action::React => self.open_reaction_picker(&chat_id, &m.id),
            Action::Edit | Action::Delete if !m.from_me => {
                let what = if action == Action::Edit {
                    "edited"
                } else {
                    "deleted"
                };
                self.push_notice(n, &format!("Only your own messages can be {what}."), false);
            }
            Action::Edit => {
                self.cancel_target(n);
                let ns = self.ns_mut(n);
                let saved = ns.drafts.remove(&chat_id).unwrap_or_default();
                let mut draft = Composer::default();
                draft.insert_str(&m.text);
                ns.drafts.insert(chat_id.clone(), draft);
                ns.targets.insert(
                    chat_id,
                    Target::Edit {
                        message_id: m.id.clone(),
                        original: m.text.clone(),
                        saved,
                    },
                );
                self.leave_select(n);
            }
            Action::Delete => {
                self.prompt = Some(Prompt::Delete {
                    chat_id,
                    message_id: m.id.clone(),
                });
            }
            _ => {}
        }
    }

    /// `y` on the highlighted message: its text, without the attachment markers; for an attachment
    /// with no caption, its file name. Queued for the run loop, which owns the clipboard.
    pub(super) fn copy_selected(&mut self, n: Network) {
        let Some(m) = self.selected_message(n) else {
            return;
        };
        let text = crate::media_view::strip_markers(&m.text, m.attachments.len())
            .trim()
            .to_string();
        let text = if text.is_empty() {
            m.attachments
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            text
        };
        if text.is_empty() {
            self.push_notice(n, "Nothing to copy in this message.", false);
        } else {
            self.copy = Some((n, text));
        }
    }

    /// `u` on the highlighted message: its links, behind the same "Open host? y/n" line.
    pub(super) fn link_selected(&mut self, n: Network) {
        let urls = self
            .selected_message(n)
            .map(|m| links::urls(&m.text))
            .unwrap_or_default();
        if urls.is_empty() {
            self.push_notice(n, "No link in this message.", false);
        } else {
            self.offer_links(urls);
        }
    }

    // ---- mouse on the main screen ----

    /// A click on a message selects it and moves focus to it (from the list the chat is opened
    /// first, as Enter on the chat does). On a link it then asks y/n with the host, on an
    /// attachment chip it runs the `o` flow: a click never does more than the key would.
    pub(super) fn click_message(
        &mut self,
        n: Network,
        id: String,
        kind: Option<ui::HitKind>,
    ) -> Out {
        let ns = self.ns(n);
        let known = ns
            .open
            .as_ref()
            .and_then(|c| ns.messages.get(c))
            .is_some_and(|l| l.iter().any(|m| m.id == id && selectable(m)));
        if !known {
            return Vec::new();
        }
        let mut out = Vec::new();
        let from_list = !matches!(ns.focus, Focus::Composer | Focus::Select);
        if from_list {
            out = self.commit(n);
        }
        let ns = self.ns_mut(n);
        ns.focus = Focus::Select;
        ns.sel_msg = Some(id);
        // Opening the chat scrolled it to the newest message.
        if from_list {
            self.ensure_sel_visible(n);
        }
        match kind {
            Some(ui::HitKind::Link(url)) => self.offer_links(vec![url]),
            Some(ui::HitKind::Chip(i)) => out.extend(self.open_media_at(n, i)),
            None => {}
        }
        out
    }

    /// A click in the message box focuses it and puts the cursor on the clicked character.
    pub(super) fn click_composer(&mut self, n: Network, lay: &ui::Layout, pos: Position) -> Out {
        let mut out = Vec::new();
        if self.ns(n).focus != Focus::Composer {
            out = self.commit(n);
        }
        let area = ui::composer_text_area(self, lay);
        if area.contains(pos) {
            let w = ui::composer_text_width(lay.composer.width);
            let crow = self.ns(n).composer().map_or(0, |c| c.layout(w).1);
            let row = composer::top_row(crow, area.height as usize) + (pos.y - area.y) as usize;
            // The first two columns are the `> ` prompt.
            let col = ((pos.x - area.x) as usize).saturating_sub(2);
            if let Some(c) = self.ns_mut(n).composer_mut() {
                c.click(w, row, col);
            }
        }
        out
    }

    /// The mouse while the completion list is open: a click on an item chooses it (as Enter
    /// does), the wheel moves the choice. None when the pointer is outside the list.
    pub(super) fn popup_mouse(
        &mut self,
        m: MouseEvent,
        lay: &ui::Layout,
        pos: Position,
    ) -> Option<Out> {
        let g = ui::popup_geometry(self, lay)?;
        if !g.outer.contains(pos) {
            return None;
        }
        let p = self.popup.as_mut()?;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if g.rows.contains(pos) => {
                p.selected = g.start + (pos.y - g.rows.y) as usize;
                // Same path as Enter: typing notice and list refresh included.
                return Some(self.run_click_action(Action::Accept));
            }
            MouseEventKind::ScrollUp => p.selected = p.selected.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                p.selected = (p.selected + 1).min(p.items.len().saturating_sub(1));
            }
            _ => {}
        }
        Some(Vec::new())
    }

    /// Drops the reply or edit in the message box. An edit gives the earlier draft back.
    pub(super) fn cancel_target(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        let Some(chat_id) = ns.open.clone() else {
            return;
        };
        match ns.targets.remove(&chat_id) {
            Some(Target::Edit { saved, .. }) => {
                ns.drafts.insert(chat_id, saved);
            }
            Some(Target::Reply(_)) | None => {}
        }
    }

    /// Enter while editing: sends `Command::Edit`. Nothing changed cancels the edit; an empty
    /// text or one over the network limit is refused with the box kept as it is.
    pub(super) fn send_edit(&mut self, n: Network, chat_id: String, text: String) -> Out {
        let Some(Target::Edit {
            message_id,
            original,
            ..
        }) = self.ns(n).targets.get(&chat_id).cloned()
        else {
            return Vec::new();
        };
        if text.is_empty() {
            self.push_notice(
                n,
                "EDIT IS EMPTY: press Esc to cancel, or select the message and delete it.",
                true,
            );
            return Vec::new();
        }
        if text == original.trim() {
            self.cancel_target(n);
            self.push_notice(n, "Nothing changed: edit cancelled.", false);
            return Vec::new();
        }
        if !limits::text_fits(n, &text) {
            self.push_notice(n, &too_long(n), true);
            return Vec::new();
        }
        self.cancel_target(n);
        vec![(
            n,
            Command::Edit {
                chat_id,
                message_id,
                text,
            },
        )]
    }

    // ---- @mentions (issue 28) ----

    /// The members of a group arrived (or the backend said why not). Names are cleaned like
    /// every network text.
    pub(super) fn members_arrived(
        &mut self,
        n: Network,
        chat_id: String,
        members: Vec<PersonRef>,
        error: Option<String>,
    ) {
        if let Some(e) = error {
            self.push_notice(n, &e, true);
            return;
        }
        let members = clean_members(members);
        self.ns_mut(n).members.insert(chat_id, members);
    }

    /// Opens, updates or closes the `@` list from the text before the cursor. The first `@` in
    /// a group chat shows the cached members at once and asks the backend for fresh ones, once
    /// per chat per session (a user action: Teams answers it with one Graph call).
    pub(super) fn refresh_mention_popup(&mut self, n: Network, before: &str) -> Out {
        let Some(query) = mention_query(before).map(str::to_string) else {
            self.popup = None;
            return Vec::new();
        };
        if !self.ns(n).open_is_group() {
            self.popup = None;
            return Vec::new();
        }
        let Some(chat_id) = self.ns(n).open.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !self.ns(n).members_asked.contains(&chat_id) {
            self.ns_mut(n).members_asked.insert(chat_id.clone());
            let cached = self
                .store
                .as_ref()
                .and_then(|s| s.members(n, &chat_id, MEMBERS_MAX as u32).ok())
                .filter(|rows| !rows.is_empty());
            if let Some(rows) = cached {
                self.ns_mut(n)
                    .members
                    .entry(chat_id.clone())
                    .or_insert_with(|| clean_members(rows));
            }
            out.push((
                n,
                Command::LoadMembers {
                    chat_id: chat_id.clone(),
                },
            ));
        }
        // Only the `@`: nothing to match yet.
        if query.is_empty() {
            self.popup = None;
            return out;
        }
        if self.popup.as_ref().is_some_and(|p| p.query == query) {
            return out;
        }
        let items = suggest_members(
            self.ns(n)
                .members
                .get(&chat_id)
                .map_or(&[][..], Vec::as_slice),
            &query,
            MENTION_ITEMS,
        );
        self.popup = if items.is_empty() {
            None
        } else {
            let replace = 1 + query.graphemes(true).count();
            Some(Popup::new(items, query, replace).titled("Mention"))
        };
        out
    }
}

fn clean_members(mut members: Vec<PersonRef>) -> Vec<PersonRef> {
    for p in &mut members {
        p.name = clean_line(&p.name);
    }
    members.retain(|p| !p.name.is_empty());
    members.truncate(MEMBERS_MAX);
    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Clock;
    use chatcli_core::model::{Capabilities, ChatKind, ChatSummary, Event, Reaction, Status};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;

    fn clock() -> Clock {
        Clock {
            now_ms: 1_800_000_000_000,
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

    /// Into message selection from the message box (what `select_messages` does; Esc does it too
    /// when nothing is being replied to). Nothing happens while already selecting.
    fn to_selection(app: &mut App) -> Out {
        if app.active().focus == Focus::Composer {
            app.enter_select(Network::Teams)
        } else {
            Vec::new()
        }
    }

    fn ctrl_u(app: &mut App) {
        app.handle_key(key_with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    }

    fn typed(app: &mut App, s: &str) -> Out {
        let mut out = Vec::new();
        for c in s.chars() {
            out.extend(ch(app, c));
        }
        out
    }

    /// A confirmed message; `at` orders it.
    fn msg(id: &str, at: i64, from_me: bool, text: &str) -> ChatMessage {
        ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: id.into(),
            ts: at,
            sender: if from_me { "You" } else { "Alex Example" }.into(),
            from_me,
            text: text.into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    fn with_state(mut m: ChatMessage, state: MessageState) -> ChatMessage {
        m.state = state;
        m
    }

    fn summary(kind: ChatKind) -> ChatSummary {
        ChatSummary {
            id: "a".into(),
            title: "Alex Example".into(),
            kind,
            last_ts: Some(1),
            ..ChatSummary::blank(Network::Teams)
        }
    }

    fn online(app: &mut App, kind: ChatKind) {
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, vec![summary(kind)]));
    }

    /// Teams signed in, chat "a" (direct or group) opened with the message box focused.
    fn app_with(kind: ChatKind, messages: Vec<ChatMessage>) -> App {
        let mut app = App::new(clock());
        online(&mut app, kind);
        press(&mut app, KeyCode::Enter);
        // The message box keeps the focus when the history arrives, as after a key press.
        app.ns_mut(Network::Teams).select_on_load = None;
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages,
        });
        app
    }

    fn three() -> Vec<ChatMessage> {
        vec![
            msg("m1", 1000, false, "first"),
            msg("m2", 2000, true, "second, mine"),
            msg("m3", 3000, false, "third https://docs.example.invalid/a"),
        ]
    }

    fn sel(app: &App) -> Option<&str> {
        app.active().sel_msg.as_deref()
    }

    /// Every line of the selected message is inside the pane as drawn.
    fn in_view(app: &App) -> bool {
        let n = Network::Teams;
        let (w, h) = app.convo_size(n);
        let (lines, spans) = ui::message_layout(app.ns(n), &app.look(), w);
        let Some(sp) = spans.iter().find(|s| Some(s.id.as_str()) == sel(app)) else {
            return false;
        };
        let total = lines.len();
        let bottom = total - app.ns(n).scroll.min(total.saturating_sub(h));
        let top = bottom.saturating_sub(h);
        sp.start >= top && sp.end <= bottom
    }

    fn many(count: usize) -> Vec<ChatMessage> {
        (0..count)
            .map(|i| {
                msg(
                    &format!("m{i}"),
                    1000 + i as i64,
                    i % 2 == 1,
                    &format!("line {i}"),
                )
            })
            .collect()
    }

    fn people(names: &[&str]) -> Vec<PersonRef> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| PersonRef {
                id: format!("u{i}"),
                name: (*n).to_string(),
            })
            .collect()
    }

    // ---- pure helpers ----

    #[test]
    fn only_confirmed_messages_are_selectable() {
        let m = msg("x", 1, false, "t");
        assert!(selectable(&m));
        assert!(!selectable(&with_state(m.clone(), MessageState::Pending)));
        assert!(!selectable(&with_state(
            m.clone(),
            MessageState::Failed("no".into())
        )));
        assert!(!selectable(&with_state(m, MessageState::Deleted)));
    }

    #[test]
    fn nearest_selectable_prefers_the_one_before() {
        let list = vec![
            msg("a", 1, false, ""),
            msg("b", 2, false, ""),
            with_state(msg("c", 3, true, ""), MessageState::Pending),
            msg("d", 4, false, ""),
        ];
        assert_eq!(nearest_selectable(&list, Some("c")).as_deref(), Some("b"));
        assert_eq!(nearest_selectable(&list, Some("a")).as_deref(), Some("b"));
        assert_eq!(nearest_selectable(&list, Some("zz")).as_deref(), Some("d"));
        assert_eq!(nearest_selectable(&list, None).as_deref(), Some("d"));
        assert_eq!(nearest_selectable(&[], None), None);
    }

    #[test]
    fn previews_are_one_line_and_at_most_120_characters() {
        assert_eq!(preview_of("one\n  two\tthree"), "one two three");
        assert_eq!(preview_of(""), "");
        let long = "x".repeat(300);
        let p = preview_of(&long);
        assert_eq!(p.chars().count(), 120);
        assert!(p.ends_with("..."));
        // An emoji family is one cluster: never cut in half.
        let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".repeat(130);
        let p = preview_of(&fam);
        assert!(p.ends_with("..."));
        assert_eq!(p.trim_end_matches('.').graphemes(true).count(), 117);
    }

    #[test]
    fn mention_query_table() {
        for (before, want) in [
            ("@", Some("")),
            ("@al", Some("al")),
            ("hi @al", Some("al")),
            ("hi @Alex E", Some("Alex E")),
            ("line one\n@sa", Some("sa")),
            ("mail@al", None),
            ("no at sign", None),
            ("@Alex Example ", None),
            ("@ hello", None),
            ("done @Alex hi@", None),
            ("", None),
        ] {
            assert_eq!(mention_query(before), want, "{before:?}");
        }
        let long = format!("@{}", "a".repeat(MENTION_QUERY_MAX + 1));
        assert_eq!(mention_query(&long), None);
    }

    #[test]
    fn members_match_by_name_start_or_any_word() {
        let all = people(&[
            "Sam Sample",
            "Alex Example",
            "Jordan Placeholder",
            "Ellen Alexander",
        ]);
        let names = |q: &str| -> Vec<String> {
            suggest_members(&all, q, 8)
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        // Name starts first, then a match on a later word.
        assert_eq!(names("al"), ["Alex Example", "Ellen Alexander"]);
        assert_eq!(names("ex"), ["Alex Example"], "Example, not Alexander");
        assert_eq!(names("alex e"), ["Alex Example"]);
        assert_eq!(names("ALEX"), ["Alex Example", "Ellen Alexander"]);
        assert!(names("zzz").is_empty());
        assert_eq!(suggest_members(&all, "a", 2).len(), 2);
        let items = suggest_members(&all, "sam", 8);
        assert_eq!(items[0].insert, "@Sam Sample ");
        assert_eq!(items[0].person.as_ref().unwrap().id, "u0");
    }

    #[test]
    fn duplicate_names_count_once_per_token() {
        let mut c = Composer::default();
        let p = people(&["Alex Example"])[0].clone();
        c.mentions = vec![p.clone(), p];
        assert_eq!(c.mentions_in("@Alex Example once").len(), 1);
        assert_eq!(c.mentions_in("@Alex Example and @Alex Example").len(), 2);
        assert_eq!(c.mentions_in("none").len(), 0);
    }

    // ---- selection (issue 11) ----

    /// Teams signed in, chat "a" entered from the list before any of its messages were loaded.
    fn entered_before_load() -> App {
        let mut app = App::new(clock());
        online(&mut app, ChatKind::Direct);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.active().focus, Focus::Composer, "nothing to select yet");
        app
    }

    fn history(app: &mut App, messages: Vec<ChatMessage>) {
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages,
        });
    }

    #[test]
    fn a_chat_entered_before_its_messages_load_selects_the_newest_when_they_come() {
        let mut app = entered_before_load();
        history(&mut app, three());
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m3"));
        assert!(in_view(&app));
        // Only once: back in the box, more history leaves the focus alone.
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.active().focus, Focus::Composer);
        history(&mut app, three());
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn a_key_before_the_messages_load_keeps_the_message_box() {
        let mut app = entered_before_load();
        press(&mut app, KeyCode::Char('h'));
        history(&mut app, three());
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(app.active().draft(), "h");
    }

    #[test]
    fn an_empty_first_answer_waits_for_a_message_to_select() {
        let mut app = entered_before_load();
        history(&mut app, Vec::new());
        assert_eq!(app.active().focus, Focus::Composer);
        // WhatsApp's history from the phone, or a live message, comes later.
        app.handle_event(Event::Message(msg("m9", 9000, false, "late")));
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m9"));
    }

    #[test]
    fn leaving_the_chat_before_its_messages_load_selects_nothing() {
        let mut app = entered_before_load();
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.active().focus, Focus::List);
        history(&mut app, three());
        assert_eq!(app.active().focus, Focus::List);
        assert_eq!(sel(&app), None);
    }

    #[test]
    fn esc_from_the_message_box_selects_the_newest_message() {
        let mut app = app_with(ChatKind::Direct, three());
        // Shift+Up is no longer a key for it.
        app.handle_key(key_with(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.active().focus, Focus::Composer);
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m3"));
        assert_eq!(app.context(), crate::keymap::Context::Select);
    }

    #[test]
    fn an_empty_chat_has_nothing_to_select() {
        let mut app = app_with(ChatKind::Direct, Vec::new());
        to_selection(&mut app);
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(sel(&app), None);
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("No message to select in this chat.")
        );
    }

    #[test]
    fn first_last_and_the_ends() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        press(&mut app, KeyCode::Home);
        assert_eq!(sel(&app), Some("m1"));
        press(&mut app, KeyCode::Up);
        assert_eq!(sel(&app), Some("m1"), "stays on the first");
        press(&mut app, KeyCode::Down);
        assert_eq!(sel(&app), Some("m2"));
        press(&mut app, KeyCode::End);
        assert_eq!(sel(&app), Some("m3"));
        press(&mut app, KeyCode::Down);
        assert_eq!(sel(&app), Some("m3"), "stays on the last");
        ch(&mut app, 'k');
        assert_eq!(sel(&app), Some("m2"));
        ch(&mut app, 'j');
        ch(&mut app, 'G');
        assert_eq!(sel(&app), Some("m3"));
    }

    #[test]
    fn pending_and_failed_messages_are_skipped() {
        let mut list = three();
        list.push(with_state(
            msg("p", 4000, true, "sending"),
            MessageState::Pending,
        ));
        list.push(with_state(
            msg("f", 5000, true, "failed"),
            MessageState::Failed("no".into()),
        ));
        let mut app = app_with(ChatKind::Direct, list);
        to_selection(&mut app);
        assert_eq!(sel(&app), Some("m3"));
        press(&mut app, KeyCode::Down);
        assert_eq!(sel(&app), Some("m3"));
        for _ in 0..5 {
            press(&mut app, KeyCode::Up);
        }
        assert_eq!(sel(&app), Some("m1"));
    }

    #[test]
    fn the_view_follows_the_highlight() {
        let mut app = app_with(ChatKind::Direct, many(60));
        app.resize(80, 24);
        to_selection(&mut app);
        assert!(in_view(&app));
        press(&mut app, KeyCode::Home);
        assert_eq!(sel(&app), Some("m0"));
        assert!(app.active().scroll > 0);
        assert!(in_view(&app));
        for _ in 0..30 {
            press(&mut app, KeyCode::Down);
            assert!(in_view(&app), "{:?}", sel(&app));
        }
        press(&mut app, KeyCode::End);
        assert_eq!(app.active().scroll, 0);
        assert!(in_view(&app));
    }

    #[test]
    fn page_keys_move_about_a_screen() {
        let mut app = app_with(ChatKind::Direct, many(80));
        app.resize(80, 24);
        to_selection(&mut app);
        press(&mut app, KeyCode::PageUp);
        let idx = |app: &App| -> usize { sel(app).unwrap()[1..].parse().unwrap() };
        let rows = app.convo_size(Network::Teams).1;
        let moved = 79 - idx(&app);
        assert!(
            moved >= rows / 2 && moved < rows + 2,
            "moved {moved} of {rows}"
        );
        assert!(in_view(&app));
        press(&mut app, KeyCode::PageDown);
        assert_eq!(idx(&app), 79);
        assert!(in_view(&app));
    }

    #[test]
    fn selection_survives_an_incoming_message_and_a_resize() {
        let mut app = app_with(ChatKind::Direct, many(40));
        app.resize(80, 24);
        to_selection(&mut app);
        for _ in 0..6 {
            press(&mut app, KeyCode::Up);
        }
        let held = sel(&app).map(str::to_string);
        for i in 0..5 {
            app.handle_event(Event::Message(msg(
                &format!("new{i}"),
                5000 + i,
                false,
                "ping",
            )));
        }
        assert_eq!(sel(&app).map(str::to_string), held);
        assert!(in_view(&app), "an incoming message must not push it off");
        app.resize(50, 16);
        assert_eq!(sel(&app).map(str::to_string), held);
        assert!(in_view(&app));
        app.resize(120, 40);
        assert!(in_view(&app));
    }

    #[test]
    fn on_the_newest_message_the_selection_follows_new_ones() {
        let mut app = app_with(ChatKind::Direct, many(40));
        app.resize(80, 24);
        to_selection(&mut app);
        assert_eq!(sel(&app), Some("m39"));
        app.handle_event(Event::Message(msg("new0", 5000, false, "ping")));
        assert_eq!(sel(&app), Some("new0"));
        assert!(in_view(&app));
        assert_eq!(app.active().scroll, 0, "the pane stays at the bottom");
        // A batch that brings newer messages (a catch-up) follows too.
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![msg("new1", 5001, false, "a"), msg("new2", 5002, false, "b")],
        });
        assert_eq!(sel(&app), Some("new2"));
        // One up from the newest: it stays where it is.
        press(&mut app, KeyCode::Up);
        app.handle_event(Event::Message(msg("new3", 5003, false, "c")));
        assert_eq!(sel(&app), Some("new1"));
    }

    #[test]
    fn a_batch_of_newer_messages_keeps_an_older_selection_where_it_is() {
        let mut app = app_with(ChatKind::Direct, many(40));
        app.resize(80, 24);
        to_selection(&mut app);
        for _ in 0..4 {
            press(&mut app, KeyCode::Up);
        }
        let held = sel(&app).map(str::to_string);
        // Rows between the end of the highlight and the bottom of the pane.
        let row =
            |app: &App| app.lines_below_selection(Network::Teams).unwrap() - app.active().scroll;
        let before = row(&app);
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![msg("new1", 5001, false, "a"), msg("new2", 5002, false, "b")],
        });
        assert_eq!(sel(&app).map(str::to_string), held);
        assert_eq!(row(&app), before, "the highlight did not move on screen");
        assert!(in_view(&app));
    }

    #[test]
    fn following_onto_a_message_taller_than_the_pane_keeps_the_bottom_in_view() {
        let mut app = app_with(ChatKind::Direct, many(10));
        app.resize(80, 16);
        to_selection(&mut app);
        let tall = vec!["line"; 60].join("\n");
        app.handle_event(Event::Message(msg("tall", 5000, false, &tall)));
        assert_eq!(sel(&app), Some("tall"));
        assert_eq!(app.active().scroll, 0, "the pane stays at the bottom");
    }

    #[test]
    fn wheel_scrolling_keeps_the_selection() {
        let mut app = app_with(ChatKind::Direct, many(60));
        app.resize(80, 24);
        to_selection(&mut app);
        let held = sel(&app).map(str::to_string);
        let convo = app.layout_in(Rect::new(0, 0, 80, 24)).convo;
        for _ in 0..4 {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: convo.x + 5,
                row: convo.y + 3,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(app.active().scroll > 0);
        assert_eq!(sel(&app).map(str::to_string), held);
        assert_eq!(app.active().focus, Focus::Select);
    }

    #[test]
    fn i_goes_to_the_message_box_and_escape_h_and_left_to_the_chat_list() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'i');
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(sel(&app), None);
        // Typing goes to the message box again.
        typed(&mut app, "hi");
        assert_eq!(app.active().draft(), "hi");
        for back in [KeyCode::Esc, KeyCode::Char('h'), KeyCode::Left] {
            to_selection(&mut app);
            press(&mut app, back);
            assert_eq!(app.active().focus, Focus::List, "{back:?}");
            assert_eq!(sel(&app), None);
            assert_eq!(app.active().open.as_deref(), Some("a"), "still shown");
            press(&mut app, KeyCode::Char('i'));
        }
    }

    #[test]
    fn escape_from_an_empty_box_goes_to_selection() {
        let mut app = app_with(ChatKind::Direct, three());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m3"));
        // With nothing to select, it goes to the chat list.
        let mut app = app_with(ChatKind::Direct, Vec::new());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.active().focus, Focus::List);
    }

    #[test]
    fn y_copies_the_text_or_the_file_name() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'y');
        assert_eq!(
            app.take_copy(),
            Some((
                Network::Teams,
                "third https://docs.example.invalid/a".to_string()
            ))
        );
        assert_eq!(app.take_copy(), None, "once");
        app.copy_done(Network::Teams, Ok("Copied."));
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("Copied.")
        );
        // An attachment: its caption without the marker, or its name when there is none.
        let file = |text: &str| {
            let mut m = msg("f", 4000, false, text);
            m.attachments = vec![chatcli_core::model::Attachment {
                kind: chatcli_core::model::MediaKind::Document,
                name: Some("plan.pdf".into()),
                mime: None,
                size: None,
                fetch_ref: chatcli_core::model::FetchRef::new("ref"),
                open_url: None,
            }];
            m
        };
        for (text, want) in [
            ("[document: plan.pdf] see p2", "see p2"),
            ("[document: plan.pdf]", "plan.pdf"),
        ] {
            let mut app = app_with(ChatKind::Direct, vec![file(text)]);
            to_selection(&mut app);
            ch(&mut app, 'y');
            assert_eq!(
                app.take_copy().map(|c| c.1).as_deref(),
                Some(want),
                "{text}"
            );
        }
    }

    #[test]
    fn ctrl_o_in_one_chat_keeps_the_places_of_another() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        // A place in some other chat, then one in this chat.
        app.ns_mut(Network::Teams)
            .jumps
            .extend([("other".into(), "x1".into()), ("a".into(), "m1".into())]);
        app.handle_key(key_with(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(sel(&app), Some("m1"));
        app.handle_key(key_with(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(
            app.ns(Network::Teams).jumps,
            [("other".to_string(), "x1".to_string())],
            "the other chat's place is still there"
        );
    }

    #[test]
    fn ctrl_o_walks_back_through_the_big_jumps() {
        let many: Vec<ChatMessage> = (0..40)
            .map(|i| msg(&format!("m{i:02}"), 1000 + i, false, "x"))
            .collect();
        let mut app = app_with(ChatKind::Direct, many);
        let ctrl_o =
            |app: &mut App| app.handle_key(key_with(KeyCode::Char('o'), KeyModifiers::CONTROL));
        to_selection(&mut app);
        ch(&mut app, 'k');
        assert_eq!(sel(&app), Some("m38"));
        // g, then G: two places remembered.
        ch(&mut app, 'g');
        assert_eq!(sel(&app), Some("m00"));
        ch(&mut app, 'G');
        assert_eq!(sel(&app), Some("m39"));
        // Small moves are not remembered.
        ch(&mut app, 'k');
        ctrl_o(&mut app);
        assert_eq!(sel(&app), Some("m00"), "back to before G");
        assert!(in_view(&app));
        ctrl_o(&mut app);
        assert_eq!(sel(&app), Some("m38"), "back to before g");
        ctrl_o(&mut app);
        assert_eq!(sel(&app), Some("m38"), "nothing left");
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("No earlier place to jump back to.")
        );
        // A jump that lands where it started remembers nothing.
        ch(&mut app, 'G');
        ch(&mut app, 'G');
        ctrl_o(&mut app);
        assert_eq!(sel(&app), Some("m38"));
    }

    #[test]
    fn screen_keys_pick_the_first_middle_and_last_message_on_screen() {
        let many: Vec<ChatMessage> = (0..40)
            .map(|i| msg(&format!("m{i:02}"), 1000 + i, false, "x"))
            .collect();
        let mut app = app_with(ChatKind::Direct, many);
        to_selection(&mut app);
        assert_eq!(sel(&app), Some("m39"));
        ch(&mut app, 'H');
        let top = sel(&app).unwrap().to_string();
        assert!(top.as_str() < "m39" && in_view(&app), "{top}");
        ch(&mut app, 'L');
        assert_eq!(sel(&app), Some("m39"));
        ch(&mut app, 'M');
        let mid = sel(&app).unwrap().to_string();
        assert!(top < mid && mid.as_str() < "m39", "{mid}");
        // g is the very first message held.
        ch(&mut app, 'g');
        assert_eq!(sel(&app), Some("m00"));
    }

    #[test]
    fn a_deleted_message_hands_the_highlight_to_its_neighbour() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        let mut gone = msg("m3", 3000, false, "");
        gone.state = MessageState::Deleted;
        app.handle_event(Event::MessageChanged(gone));
        assert_eq!(sel(&app), Some("m2"));
        for id in ["m1", "m2"] {
            let mut gone = msg(id, 1, false, "");
            gone.state = MessageState::Deleted;
            app.handle_event(Event::MessageChanged(gone));
        }
        assert_eq!(
            app.active().focus,
            Focus::Composer,
            "nothing left to select"
        );
    }

    // ---- reply (issue 12) ----

    fn sent_of(out: &Out) -> (String, Option<QuotedRef>, Vec<PersonRef>) {
        match &out[0].1 {
            Command::Send {
                text,
                reply_to,
                mentions,
                ..
            } => (text.clone(), reply_to.clone(), mentions.clone()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reply_quotes_the_selected_message_and_sends_with_it() {
        let mut m3 = msg("m3", 3000, false, "third\nline with   spaces");
        m3.sender_id = "aad-1".into();
        let mut app = app_with(ChatKind::Direct, vec![msg("m1", 1000, false, "x"), m3]);
        to_selection(&mut app);
        ch(&mut app, 'r');
        assert_eq!(app.active().focus, Focus::Composer, "back to typing");
        let want = QuotedRef {
            id: "m3".into(),
            sender: "Alex Example".into(),
            sender_id: "aad-1".into(),
            preview: "third line with spaces".into(),
        };
        assert_eq!(app.active().target(), Some(&Target::Reply(want.clone())));
        typed(&mut app, "ok");
        let out = press(&mut app, KeyCode::Enter);
        let (text, reply_to, _) = sent_of(&out);
        assert_eq!((text.as_str(), reply_to), ("ok", Some(want.clone())));
        // The pending copy carries the quote too, and the target is spent.
        let held = &app.ns(Network::Teams).messages["a"];
        assert_eq!(held.last().unwrap().reply_to, Some(want));
        assert!(app.active().target().is_none());
        // The next send is a plain one.
        typed(&mut app, "again");
        assert_eq!(sent_of(&press(&mut app, KeyCode::Enter)).1, None);
    }

    #[test]
    fn replying_to_your_own_message_is_quoted_as_you() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'k');
        ch(&mut app, 'r');
        match app.active().target() {
            Some(Target::Reply(q)) => {
                assert_eq!((q.id.as_str(), q.sender.as_str()), ("m2", "You"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escape_cancels_the_reply_first_and_then_leaves() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'r');
        press(&mut app, KeyCode::Esc);
        assert!(app.active().target().is_none());
        assert_eq!(app.active().focus, Focus::Composer);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.active().focus, Focus::Select);
    }

    #[test]
    fn a_refused_reply_stays_a_failed_reply() {
        // The backend answers MessageFailed; the message keeps its quote and shows why.
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'r');
        typed(&mut app, "ok");
        let out = press(&mut app, KeyCode::Enter);
        let client_id = match &out[0].1 {
            Command::Send { client_id, .. } => client_id.clone(),
            other => panic!("{other:?}"),
        };
        app.handle_event(Event::MessageFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            client_id,
            error: "REPLY NEEDS ChatMessage.Send, sign in again".into(),
        });
        let last = app.ns(Network::Teams).messages["a"].last().unwrap().clone();
        assert!(matches!(last.state, MessageState::Failed(_)));
        assert!(last.reply_to.is_some());
    }

    // ---- reactions (issue 13) ----

    #[test]
    fn react_opens_the_picker_and_choosing_sends_the_reaction() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'e');
        assert!(app.picker().is_some());
        let out = press(&mut app, KeyCode::Enter);
        match &out[..] {
            [(
                Network::Teams,
                Command::React {
                    chat_id,
                    message_id,
                    emoji,
                    remove,
                },
            )] => {
                assert_eq!((chat_id.as_str(), message_id.as_str()), ("a", "m3"));
                assert!(!emoji.is_empty() && !remove);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(app.active().focus, Focus::Select, "still on the message");
    }

    #[test]
    fn choosing_your_own_reaction_again_removes_it() {
        let mut m3 = msg("m3", 3000, false, "third");
        let mut app = app_with(ChatKind::Direct, vec![m3.clone()]);
        to_selection(&mut app);
        ch(&mut app, 'e');
        // Whatever the first cell is, make it already yours.
        let first = app
            .picker()
            .unwrap()
            .items(app.recents())
            .first()
            .unwrap()
            .as_str()
            .to_string();
        m3.reactions = vec![Reaction {
            emoji: first.clone(),
            count: 2,
            mine: true,
        }];
        app.handle_event(Event::MessageChanged(m3));
        let out = press(&mut app, KeyCode::Enter);
        match &out[..] {
            [(_, Command::React { emoji, remove, .. })] => {
                assert_eq!(emoji, &first);
                assert!(remove);
            }
            other => panic!("{other:?}"),
        }
        // Someone else's reaction with the same emoji is not yours to take back.
        let mut m3 = msg("m3", 3000, false, "third");
        m3.reactions = vec![Reaction {
            emoji: first,
            count: 2,
            mine: false,
        }];
        app.handle_event(Event::MessageChanged(m3));
        ch(&mut app, 'e');
        let out = press(&mut app, KeyCode::Enter);
        assert!(
            matches!(&out[..], [(_, Command::React { remove: false, .. })]),
            "{out:?}"
        );
    }

    // ---- edit and delete (issue 27) ----

    #[test]
    fn edit_loads_the_text_and_enter_sends_the_change() {
        let mut app = app_with(ChatKind::Direct, three());
        typed(&mut app, "unsent draft");
        to_selection(&mut app);
        ch(&mut app, 'k');
        assert_eq!(sel(&app), Some("m2"));
        ch(&mut app, 'c');
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(app.active().draft(), "second, mine");
        assert!(matches!(app.active().target(), Some(Target::Edit { .. })));
        typed(&mut app, "!");
        let out = press(&mut app, KeyCode::Enter);
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::Edit {
                    chat_id: "a".into(),
                    message_id: "m2".into(),
                    text: "second, mine!".into(),
                }
            )]
        );
        // Nothing shows as changed until the backend says so, and the old draft is back.
        assert_eq!(app.ns(Network::Teams).messages["a"][1].text, "second, mine");
        assert_eq!(app.active().draft(), "unsent draft");
        assert!(app.active().target().is_none());
    }

    #[test]
    fn escape_ends_an_edit_and_gives_the_draft_back() {
        let mut app = app_with(ChatKind::Direct, three());
        typed(&mut app, "unsent draft");
        to_selection(&mut app);
        ch(&mut app, 'k');
        ch(&mut app, 'c');
        press(&mut app, KeyCode::Esc);
        assert!(app.active().target().is_none());
        assert_eq!(app.active().draft(), "unsent draft");
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn an_unchanged_or_empty_edit_sends_nothing() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'k');
        ch(&mut app, 'c');
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert!(app.active().target().is_none(), "unchanged ends the edit");
        // An empty edit is refused with the box kept.
        to_selection(&mut app);
        ch(&mut app, 'k');
        ch(&mut app, 'c');
        ctrl_u(&mut app);
        assert_eq!(app.active().draft(), "");
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert!(matches!(app.active().target(), Some(Target::Edit { .. })));
        assert!(app.current_notice().is_some_and(|n| n.is_error));
    }

    #[test]
    fn edit_and_delete_are_for_your_own_messages_only() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        assert_eq!(sel(&app), Some("m3"));
        ch(&mut app, 'c');
        assert_eq!(app.active().focus, Focus::Select, "no edit of theirs");
        assert!(app.active().target().is_none());
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("Only your own messages can be edited.")
        );
        ch(&mut app, 'd');
        assert!(app.prompt().is_none());
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("Only your own messages can be deleted.")
        );
    }

    #[test]
    fn delete_asks_first_and_y_sends_it() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'k');
        ch(&mut app, 'd');
        assert_eq!(
            app.prompt(),
            Some(&Prompt::Delete {
                chat_id: "a".into(),
                message_id: "m2".into()
            })
        );
        assert_eq!(app.context(), crate::keymap::Context::Confirm);
        // n and Esc cancel; nothing is sent.
        assert!(ch(&mut app, 'n').is_empty());
        assert!(app.prompt().is_none());
        ch(&mut app, 'd');
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert!(app.prompt().is_none());
        assert_eq!(app.active().focus, Focus::Select, "Esc closed the question");
        ch(&mut app, 'd');
        let out = ch(&mut app, 'y');
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::Delete {
                    chat_id: "a".into(),
                    message_id: "m2".into()
                }
            )]
        );
        // Still shown until the backend confirms.
        assert_eq!(
            app.ns(Network::Teams).messages["a"][1].state,
            MessageState::Sent
        );
    }

    // ---- incoming changes and capabilities ----

    #[test]
    fn a_changed_message_replaces_the_held_one_quietly() {
        let mut app = app_with(ChatKind::Direct, three());
        let mut edited = msg("m1", 1000, false, "first, fixed");
        edited.edited = true;
        edited.reactions = vec![Reaction {
            emoji: "\u{1F44D}".into(),
            count: 2,
            mine: false,
        }];
        let before = app.notices().len();
        let out = app.handle_event(Event::MessageChanged(edited));
        assert!(out.is_empty(), "no read, no command: {out:?}");
        let held = &app.ns(Network::Teams).messages["a"];
        assert_eq!(held.len(), 3);
        assert_eq!(held[0].text, "first, fixed");
        assert!(held[0].edited && held[0].reactions.len() == 1);
        assert_eq!(app.notices().len(), before);
    }

    #[test]
    fn a_change_in_a_chat_never_loaded_is_not_stored() {
        let mut app = app_with(ChatKind::Direct, three());
        let mut other = msg("z1", 1, false, "x");
        other.chat_id = "elsewhere".into();
        app.handle_event(Event::MessageChanged(other));
        assert!(!app.ns(Network::Teams).messages.contains_key("elsewhere"));
    }

    #[test]
    fn capabilities_start_false_and_are_stored() {
        let mut app = app_with(ChatKind::Direct, three());
        assert_eq!(app.active().caps, Capabilities::default());
        app.handle_event(Event::Capabilities {
            network: Network::Teams,
            caps: Capabilities::all(),
        });
        assert!(app.active().caps.reply && app.active().caps.edit);
        assert_eq!(app.ns(Network::WhatsApp).caps, Capabilities::default());
        // A dimmed action still works: before any Capabilities event, `r` replies.
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'r');
        assert!(matches!(app.active().target(), Some(Target::Reply(_))));
    }

    #[test]
    fn a_link_in_the_selected_message_opens_behind_the_question() {
        let mut app = app_with(ChatKind::Direct, three());
        to_selection(&mut app);
        ch(&mut app, 'u');
        assert!(matches!(app.prompt(), Some(Prompt::Confirm(_))));
        ch(&mut app, 'y');
        assert_eq!(app.take_open_urls(), ["https://docs.example.invalid/a"]);
        ch(&mut app, 'k');
        ch(&mut app, 'u');
        assert!(app.prompt().is_none());
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("No link in this message.")
        );
    }

    #[test]
    fn quotes_reactions_and_mentions_from_the_network_are_cleaned() {
        let mut app = app_with(ChatKind::Direct, three());
        let mut m = msg("m1", 1000, false, "x");
        m.reply_to = Some(QuotedRef {
            id: "q".into(),
            sender: "Evil\u{1b}[2JName".into(),
            sender_id: String::new(),
            preview: "a\u{1b}[31mb".into(),
        });
        m.mentions = people(&["Bad\u{1b}[2J"]);
        app.handle_event(Event::MessageChanged(m));
        let held = &app.ns(Network::Teams).messages["a"][0];
        let q = held.reply_to.as_ref().unwrap();
        assert!(!q.sender.contains('\u{1b}') && !q.preview.contains('\u{1b}'));
        assert!(!held.mentions[0].name.contains('\u{1b}'));
    }

    // ---- mentions (issue 28) ----

    fn members_event(names: &[&str]) -> Event {
        Event::Members {
            network: Network::Teams,
            chat_id: "a".into(),
            members: people(names),
            error: None,
        }
    }

    #[test]
    fn at_in_a_group_asks_for_members_once_and_completes_a_name() {
        let mut app = app_with(ChatKind::Group, three());
        let out = ch(&mut app, '@');
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::LoadMembers {
                    chat_id: "a".into()
                }
            )]
        );
        assert!(app.popup().is_none(), "nothing to match yet");
        app.handle_event(members_event(&[
            "Sam Sample",
            "Alex Example",
            "Jordan Placeholder",
        ]));
        let out = ch(&mut app, 'a');
        assert!(out.is_empty(), "asked once per chat: {out:?}");
        assert_eq!(app.popup().unwrap().items.len(), 1);
        assert_eq!(app.popup().unwrap().title, "Mention");
        // Enter inserts the token and a space; it does not send.
        let out = press(&mut app, KeyCode::Enter);
        assert!(out.is_empty());
        assert_eq!(app.active().draft(), "@Alex Example ");
        assert!(app.popup().is_none());
        typed(&mut app, "can you look?");
        let (text, _, mentions) = sent_of(&press(&mut app, KeyCode::Enter));
        assert_eq!(text, "@Alex Example can you look?");
        assert_eq!(
            mentions,
            vec![PersonRef {
                id: "u1".into(),
                name: "Alex Example".into()
            }]
        );
        // The pending copy highlights it too.
        let held = &app.ns(Network::Teams).messages["a"];
        assert_eq!(held.last().unwrap().mentions.len(), 1);
    }

    #[test]
    fn members_arriving_late_open_the_list() {
        let mut app = app_with(ChatKind::Group, three());
        ch(&mut app, '@');
        typed(&mut app, "sa");
        assert!(app.popup().is_none());
        app.handle_event(members_event(&["Sam Sample"]));
        assert_eq!(app.popup().unwrap().items.len(), 1);
    }

    #[test]
    fn no_mention_list_in_a_direct_chat_or_inside_a_word() {
        let mut app = app_with(ChatKind::Direct, three());
        let out = typed(&mut app, "@al");
        assert!(out.is_empty());
        assert!(app.popup().is_none());
        let mut app = app_with(ChatKind::Group, three());
        app.handle_event(members_event(&["Alex Example"]));
        typed(&mut app, "mail@al");
        assert!(app.popup().is_none());
    }

    #[test]
    fn backspace_removes_a_mention_token_whole() {
        let mut app = app_with(ChatKind::Group, three());
        app.handle_event(members_event(&["Alex Example"]));
        typed(&mut app, "hi @al");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.active().draft(), "hi @Alex Example ");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.active().draft(), "hi ");
        // One more Backspace is an ordinary one.
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.active().draft(), "hi");
        typed(&mut app, " x");
        let (_, _, mentions) = sent_of(&press(&mut app, KeyCode::Enter));
        assert!(mentions.is_empty(), "the removed token is no mention");
    }

    #[test]
    fn a_token_edited_away_is_not_sent_as_a_mention() {
        let mut app = app_with(ChatKind::Group, three());
        app.handle_event(members_event(&["Alex Example"]));
        typed(&mut app, "@al");
        press(&mut app, KeyCode::Enter);
        // Cut the whole line with Ctrl+U, then type a half name by hand.
        ctrl_u(&mut app);
        typed(&mut app, "plain @Alex text");
        let (text, _, mentions) = sent_of(&press(&mut app, KeyCode::Enter));
        assert_eq!(text, "plain @Alex text");
        assert!(mentions.is_empty(), "@Alex is not the chosen @Alex Example");
    }

    #[test]
    fn a_members_error_is_a_notice_and_the_text_stays() {
        let mut app = app_with(ChatKind::Group, three());
        ch(&mut app, '@');
        app.handle_event(Event::Members {
            network: Network::Teams,
            chat_id: "a".into(),
            members: Vec::new(),
            error: Some("Members is not available on Teams.".into()),
        });
        assert!(app.current_notice().is_some_and(|n| n.is_error));
        typed(&mut app, "al");
        assert_eq!(app.active().draft(), "@al");
        assert!(app.popup().is_none());
    }

    #[test]
    fn members_arriving_do_not_reopen_a_list_closed_with_esc() {
        let mut app = app_with(ChatKind::Group, three());
        typed(&mut app, ":he");
        assert!(app.popup().is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.popup().is_none());
        app.handle_event(Event::Members {
            network: Network::Teams,
            chat_id: "a".into(),
            members: Vec::new(),
            error: None,
        });
        assert!(app.popup().is_none(), "closed until the draft is edited");
        typed(&mut app, "l");
        assert!(app.popup().is_some(), "an edit brings it back");
    }

    #[test]
    fn members_arriving_do_not_reopen_a_list_closed_by_the_mouse() {
        let mut app = app_with(ChatKind::Group, three());
        typed(&mut app, ":he");
        assert!(app.popup().is_some());
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(app.popup().is_none());
        app.handle_event(Event::Members {
            network: Network::Teams,
            chat_id: "a".into(),
            members: Vec::new(),
            error: None,
        });
        assert!(app.popup().is_none(), "closed until the draft is edited");
    }

    #[test]
    fn a_paste_is_an_edit_so_members_may_open_the_list_again() {
        let mut app = app_with(ChatKind::Group, three());
        typed(&mut app, ":he");
        press(&mut app, KeyCode::Esc);
        app.handle_paste(" @al");
        app.handle_event(Event::Members {
            network: Network::Teams,
            chat_id: "a".into(),
            members: Vec::new(),
            error: None,
        });
        assert!(
            !app.popup_closed,
            "the paste reopened it for the next refresh"
        );
        // A paste that changes nothing (no chat focus) leaves it closed.
        let mut app = app_with(ChatKind::Group, three());
        typed(&mut app, ":he");
        press(&mut app, KeyCode::Esc);
        to_selection(&mut app);
        app.handle_paste("x");
        assert!(app.popup_closed);
    }

    /// The cached member list answers the first `@` at once, before the backend does.
    #[test]
    fn the_stored_member_list_fills_the_mention_list_at_once() {
        use chatcli_core::media::MediaCache;
        use chatcli_core::runtime::UiCtx;
        use chatcli_core::settings::Settings;
        use chatcli_core::store::Store;
        let dir = std::env::temp_dir().join(format!(
            "chatcli-tui-mention-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .replace_members(Network::Teams, "a", &people(&["Alex Example"]))
            .unwrap();
        let ctx = UiCtx::new(store, media);
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::with_ctx(clock(), &ctx);
        online(&mut app, ChatKind::Group);
        press(&mut app, KeyCode::Enter);
        app.ns_mut(Network::Teams).select_on_load = None;
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: three(),
        });
        let out = typed(&mut app, "@al");
        assert_eq!(out.len(), 1, "one LoadMembers: {out:?}");
        assert_eq!(app.popup().unwrap().items[0].label, "Alex Example");
    }

    // ---- profile photos in avatars ----

    /// A message from a named person id.
    fn from_id(id: &str, at: i64, who: &str) -> ChatMessage {
        ChatMessage {
            sender_id: who.into(),
            ..msg(id, at, false, "hello")
        }
    }

    fn group_app(messages: Vec<ChatMessage>) -> App {
        let mut app = app_with(ChatKind::Group, messages);
        app.resize(100, 30);
        app
    }

    fn photo(app: &mut App, who: &str) {
        app.ns_mut(Network::Teams)
            .avatars
            .insert(who.into(), Some(format!("{who}.png").into()));
    }

    #[test]
    fn a_run_start_has_an_avatar_slot_at_its_name_row() {
        let app = group_app(vec![
            from_id("m1", 1000, "u1"),
            from_id("m2", 2000, "u1"),
            from_id("m3", 3000, ""),
        ]);
        let ns = app.ns(Network::Teams);
        let (_, spans) = ui::message_layout(ns, &app.look(), 60);
        let slot = spans[0].avatar.as_ref().expect("run start");
        assert_eq!((slot.person.as_str(), slot.line), ("u1", spans[0].start));
        assert!(spans[1].avatar.is_none(), "continuation");
        assert!(spans[2].avatar.is_none(), "no sender id");
        let (_, narrow) = ui::message_layout(ns, &app.look(), 20);
        assert!(narrow.iter().all(|s| s.avatar.is_none()), "too narrow");
        let direct = app_with(ChatKind::Direct, vec![from_id("m1", 1000, "u1")]);
        let (_, spans) = ui::message_layout(direct.ns(Network::Teams), &direct.look(), 60);
        assert!(spans[0].avatar.is_none(), "1:1 has no avatar by default");
    }

    #[test]
    fn photos_are_drawn_at_the_avatar_cells_anchored_to_the_bottom() {
        let mut app = group_app(vec![from_id("m1", 1000, "u1"), from_id("m2", 5000, "u2")]);
        photo(&mut app, "u1");
        photo(&mut app, "u2");
        let lay = app.layout_in(Rect::new(0, 0, 100, 30));
        let vis = ui::visible_images(&app, &lay);
        assert_eq!(vis.people, ["u2", "u1"], "newest first");
        assert_eq!(vis.images.len(), 2);
        // Newest first: u2's box is the last two rows, as the short chat sits at the bottom.
        let r = vis.images[0].rect;
        assert_eq!(vis.images[0].path.to_str(), Some("u2.png"));
        assert_eq!((r.x, r.width, r.height), (lay.messages.x, 4, 2));
        assert_eq!(r.y + 2, lay.messages.y + lay.messages.height);
        // u1 is 3 rows higher: a blank row, then name and text rows of u2's run.
        assert_eq!(vis.images[1].rect.y + 3 + 2, r.y + 2);
    }

    #[test]
    fn a_partly_scrolled_avatar_keeps_its_initials() {
        let list = (0..40)
            .map(|i| from_id(&format!("m{i}"), 1000 + i, &format!("u{i}")))
            .collect();
        let mut app = group_app(list);
        photo(&mut app, "u39");
        photo(&mut app, "u38");
        let lay = app.layout_in(Rect::new(0, 0, 100, 30));
        let vis = ui::visible_images(&app, &lay);
        assert_eq!(vis.people[0], "u39");
        assert_eq!(vis.images.len(), 2);
        // One line up: u39's text row is gone, so its box is cut and nobody asks for it.
        app.ns_mut(Network::Teams).scroll = 1;
        let vis = ui::visible_images(&app, &lay);
        assert_eq!(vis.people[0], "u38");
        assert!(!vis.people.contains(&"u39".to_string()));
        assert_eq!(vis.images.len(), 1);
        assert_eq!(vis.images[0].path.to_str(), Some("u38.png"));
    }

    #[test]
    fn photos_off_draws_and_asks_for_nothing() {
        let mut app = group_app(vec![from_id("m1", 1000, "u1")]);
        photo(&mut app, "u1");
        app.layout.photos = false;
        let lay = app.layout_in(Rect::new(0, 0, 100, 30));
        let vis = ui::visible_images(&app, &lay);
        assert!(vis.images.is_empty() && vis.people.is_empty());
        assert!(app.avatar_requests(&["u1".to_string()]).is_empty());
    }

    #[test]
    fn no_colour_draws_and_asks_for_no_photos() {
        let mut app = group_app(vec![from_id("m1", 1000, "u1")]);
        photo(&mut app, "u1");
        app.no_colour = true;
        let lay = app.layout_in(Rect::new(0, 0, 100, 30));
        let vis = ui::visible_images(&app, &lay);
        assert!(vis.images.is_empty() && vis.people.is_empty());
        assert!(app.avatar_requests(&["u2".to_string()]).is_empty());
    }

    #[test]
    fn an_answer_nobody_asked_for_is_dropped() {
        let mut app = group_app(Vec::new());
        app.ns_mut(Network::Teams).avatars.clear();
        app.handle_event(Event::Avatar {
            network: Network::Teams,
            person_id: "late".to_string(),
            path: Some("late.png".into()),
        });
        assert!(app.ns(Network::Teams).avatars.is_empty());
    }

    #[test]
    fn each_person_is_asked_once_and_none_stops_the_asking() {
        let mut app = group_app(Vec::new());
        let ask = |app: &mut App, who: &[&str]| {
            let people: Vec<String> = who.iter().map(|s| s.to_string()).collect();
            app.avatar_requests(&people)
        };
        let out = ask(&mut app, &["u1", "u2"]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0].1, Command::FetchAvatars { people } if people == &["u1", "u2"]),
            "{out:?}"
        );
        assert!(ask(&mut app, &["u1", "u2"]).is_empty(), "never twice");
        let out = ask(&mut app, &["u2", "u3"]);
        assert!(matches!(&out[0].1, Command::FetchAvatars { people } if people == &["u3"]));
        // The answer is stored; a None answer does not bring the question back.
        app.handle_event(Event::Avatar {
            network: Network::Teams,
            person_id: "u1".into(),
            path: Some("u1.png".into()),
        });
        app.handle_event(Event::Avatar {
            network: Network::Teams,
            person_id: "u2".into(),
            path: None,
        });
        let ns = app.ns(Network::Teams);
        assert_eq!(
            ns.avatars["u1"].as_deref().and_then(|p| p.to_str()),
            Some("u1.png")
        );
        assert_eq!(ns.avatars["u2"], None);
        assert!(ask(&mut app, &["u1", "u2"]).is_empty());
        // Signed out: the next account is asked afresh.
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        assert!(app.ns(Network::Teams).avatars.is_empty());
    }

    #[test]
    fn receipt_state_is_forgotten_on_sign_out() {
        let mut app = App::new(clock());
        app.handle_event(Event::Receipts {
            network: Network::WhatsApp,
            chat_id: "w".into(),
            read: Some(1),
            delivered: Some(2),
            since: Some(0),
        });
        assert_eq!(app.ns(Network::WhatsApp).receipts.len(), 1);
        app.handle_event(Event::Status(
            Network::WhatsApp,
            Status::NeedsLogin { reason: None },
        ));
        assert!(app.ns(Network::WhatsApp).receipts.is_empty());
    }

    #[test]
    fn nobody_is_asked_while_the_network_is_not_online() {
        let mut app = App::new(clock());
        let people = ["u1".to_string()];
        assert!(app.avatar_requests(&people).is_empty());
        assert!(
            app.ns(Network::Teams).avatars.is_empty(),
            "not marked asked"
        );
    }

    // ---- mouse on the main screen ----

    fn left(app: &mut App, x: u16, y: u16) -> Out {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn wheel(app: &mut App, up: bool, x: u16, y: u16) {
        app.handle_mouse(MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// Screen cell of column `x` on conversation line `line`, for a conversation that fits.
    fn cell(app: &App, line: usize, x: usize) -> (u16, u16) {
        let lay = app.layout_for(Network::Teams);
        let w = lay.messages.width as usize;
        let (lines, _) = ui::message_layout(app.ns(Network::Teams), &app.look(), w);
        assert!(lines.len() <= lay.messages.height as usize);
        let y = lay.messages.y + lay.messages.height - (lines.len() - line) as u16;
        (lay.messages.x + x as u16, y)
    }

    fn span_of(app: &App, id: &str) -> ui::MsgSpan {
        let w = app.layout_for(Network::Teams).messages.width as usize;
        let (_, spans) = ui::message_layout(app.ns(Network::Teams), &app.look(), w);
        spans.into_iter().find(|s| s.id == id).unwrap()
    }

    fn clicked_app() -> App {
        let mut app = app_with(ChatKind::Direct, three());
        app.resize(80, 24);
        app
    }

    #[test]
    fn click_a_message_selects_it_from_the_message_box() {
        let mut app = clicked_app();
        assert_eq!(app.active().focus, Focus::Composer);
        let (x, y) = cell(&app, span_of(&app, "m1").end - 1, 3);
        left(&mut app, x, y);
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m1"));
        // Clicking the selected one keeps it; another one moves the highlight.
        left(&mut app, x, y);
        assert_eq!(sel(&app), Some("m1"));
        let (x, y) = cell(&app, span_of(&app, "m2").end - 1, 3);
        left(&mut app, x, y);
        assert_eq!(sel(&app), Some("m2"));
        assert!(app.prompt.is_none());
    }

    #[test]
    fn click_a_day_separator_or_a_pending_message_selects_nothing() {
        let mut m = three();
        m.push(with_state(
            msg("m4", 4000, true, "sending"),
            MessageState::Pending,
        ));
        let mut app = app_with(ChatKind::Direct, m);
        app.resize(80, 24);
        let (x, y) = cell(&app, span_of(&app, "m4").start, 3);
        left(&mut app, x, y);
        assert_eq!(app.active().focus, Focus::Composer);
        let (x, y) = cell(&app, 0, 3);
        left(&mut app, x, y);
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(sel(&app), None);
    }

    #[test]
    fn click_a_link_selects_the_message_and_asks_y_n() {
        let mut app = clicked_app();
        let sp = span_of(&app, "m3");
        let hit = sp.hits.first().expect("link hit").clone();
        let url = "https://docs.example.invalid/a".to_string();
        assert_eq!(hit.kind, ui::HitKind::Link(url.clone()));
        let (x, y) = cell(&app, hit.line, hit.x0);
        left(&mut app, x, y);
        assert_eq!(sel(&app), Some("m3"));
        assert_eq!(app.prompt, Some(Prompt::Confirm(url)));
        assert!(app.open_urls.is_empty(), "nothing opens before y");
        // The last cell of the link counts, the cell after does not.
        app.prompt = None;
        let (x, y) = cell(&app, hit.line, hit.x1);
        left(&mut app, x, y);
        assert!(app.prompt.is_none());
        let (x, y) = cell(&app, hit.line, hit.x1 - 1);
        left(&mut app, x, y);
        assert!(app.prompt.is_some());
    }

    #[test]
    fn a_1_1_chat_sits_two_columns_in_and_clicks_follow() {
        let mut app = clicked_app();
        let w = app.layout_for(Network::Teams).messages.width as usize;
        let (lines, _) = ui::message_layout(app.ns(Network::Teams), &app.look(), w);
        let sp = span_of(&app, "m3");
        // Name row and body both start at column 2.
        for line in &lines[sp.start..sp.end] {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with("  ") && !text[2..].starts_with(' '),
                "{text:?}"
            );
        }
        let hit = sp.hits.first().expect("link hit").clone();
        assert!(hit.x0 >= 2, "{hit:?}");
        // The gutter is not the link; the first drawn cell is.
        let (x, y) = cell(&app, hit.line, hit.x0 - 1);
        left(&mut app, x, y);
        assert!(app.prompt.is_none());
        let (x, y) = cell(&app, hit.line, hit.x0);
        left(&mut app, x, y);
        assert!(matches!(app.prompt, Some(Prompt::Confirm(_))));
    }

    #[test]
    fn a_link_that_wraps_is_clickable_on_every_line() {
        let long = format!("see https://example.invalid/{}", "x".repeat(60));
        let mut app = app_with(ChatKind::Direct, vec![msg("m1", 1000, false, &long)]);
        app.resize(40, 24);
        let hits = span_of(&app, "m1").hits;
        assert!(hits.len() >= 2, "{hits:?}");
        for h in hits {
            let (x, y) = cell(&app, h.line, h.x0);
            app.prompt = None;
            left(&mut app, x, y);
            assert!(matches!(app.prompt, Some(Prompt::Confirm(_))));
        }
    }

    #[test]
    fn click_a_chip_runs_the_o_flow_for_that_attachment() {
        use chatcli_core::model::{Attachment, FetchRef, MediaKind};
        let doc = |name: &str| Attachment {
            kind: MediaKind::Document,
            name: Some(name.into()),
            mime: None,
            size: Some(10),
            fetch_ref: FetchRef::new("r"),
            open_url: None,
        };
        let mut m = msg("m1", 1000, false, "[file] [file]");
        m.attachments = vec![doc("fine.pdf"), doc("run.exe")];
        let mut app = app_with(ChatKind::Direct, vec![m]);
        app.resize(80, 24);
        let hits = span_of(&app, "m1").hits;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].kind, ui::HitKind::Chip(1));
        let (x, y) = cell(&app, hits[1].line, hits[1].x0 + 1);
        let out = left(&mut app, x, y);
        assert_eq!(sel(&app), Some("m1"));
        assert!(out.is_empty());
        assert_eq!(
            app.current_notice().map(|n| n.text.as_str()),
            Some("BLOCKED: executable")
        );
        // The first chip is a fetch request, not the refusal.
        let (x, y) = cell(&app, hits[0].line, hits[0].x0 + 1);
        let out = left(&mut app, x, y);
        assert!(
            matches!(out[0].1, Command::FetchMedia { index: 0, .. }),
            "{out:?}"
        );
    }

    #[test]
    fn click_a_message_from_the_chat_list_opens_the_chat_first() {
        let mut app = clicked_app();
        // Esc steps out one level at a time: message box, selection, chat list.
        for _ in 0..3 {
            if app.active().focus != Focus::List {
                press(&mut app, KeyCode::Esc);
            }
        }
        assert_eq!(app.active().focus, Focus::List);
        let (x, y) = cell(&app, span_of(&app, "m2").end - 1, 40);
        left(&mut app, x, y);
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(sel(&app), Some("m2"));
        assert!(in_view(&app));
    }

    #[test]
    fn click_in_the_message_box_puts_the_cursor_on_the_clicked_character() {
        let mut app = clicked_app();
        for c in "hello wide".chars() {
            ch(&mut app, c);
        }
        let lay = app.layout_for(Network::Teams);
        let area = ui::composer_text_area(&app, &lay);
        left(&mut app, area.x + 2 + 3, area.y);
        assert_eq!(app.active().composer().unwrap().cursor(), 3);
        // Past the end: the end. On the prompt: the start.
        left(&mut app, area.x + 40, area.y);
        assert_eq!(app.active().composer().unwrap().cursor(), 10);
        left(&mut app, area.x, area.y);
        assert_eq!(app.active().composer().unwrap().cursor(), 0);
        // A click in the box from a selected message moves focus back to it.
        let (x, y) = cell(&app, span_of(&app, "m1").end - 1, 3);
        left(&mut app, x, y);
        assert_eq!(app.active().focus, Focus::Select);
        left(&mut app, area.x + 2 + 1, area.y);
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(app.active().composer().unwrap().cursor(), 1);
    }

    fn popup_item(label: &str, insert: &str) -> PopupItem {
        PopupItem {
            label: label.into(),
            detail: String::new(),
            insert: insert.into(),
            person: None,
            command: None,
        }
    }

    fn open_popup(app: &mut App) {
        for c in "x :sm".chars() {
            ch(app, c);
        }
        app.popup = Some(Popup::new(
            vec![
                popup_item("A", "AA"),
                popup_item("B", "BB"),
                popup_item("C", "CC"),
            ],
            "sm".into(),
            3,
        ));
    }

    #[test]
    fn click_a_popup_item_inserts_it_like_enter() {
        let mut app = clicked_app();
        open_popup(&mut app);
        let lay = app.layout_for(Network::Teams);
        let g = ui::popup_geometry(&app, &lay).expect("popup drawn");
        left(&mut app, g.rows.x + 1, g.rows.y + 1);
        assert!(app.popup.is_none());
        assert_eq!(app.active().draft(), "x BB");
    }

    #[test]
    fn click_outside_the_popup_closes_it_and_the_wheel_over_it_scrolls() {
        let mut app = clicked_app();
        open_popup(&mut app);
        let lay = app.layout_for(Network::Teams);
        let g = ui::popup_geometry(&app, &lay).unwrap();
        wheel(&mut app, false, g.rows.x + 1, g.rows.y);
        assert_eq!(app.popup.as_ref().map(|p| p.selected), Some(1));
        wheel(&mut app, true, g.rows.x + 1, g.rows.y);
        wheel(&mut app, true, g.rows.x + 1, g.rows.y);
        assert_eq!(app.popup.as_ref().map(|p| p.selected), Some(0));
        left(&mut app, 0, 0);
        assert!(app.popup.is_none());
        assert_eq!(app.active().draft(), "x :sm");
    }

    fn mv(app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Out {
        app.handle_mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Footer column where the drawn y of a question is.
    fn yes_x(app: &App) -> u16 {
        let footer = app.layout_in(app.size).footer;
        let y = app.size.height - 1;
        (0..app.size.width)
            .find(|x| {
                ui::footer_hit_at(app, footer, app.size.height, Position::new(*x, y))
                    == Some(ui::FooterHit::Yes)
            })
            .expect("y is drawn")
    }

    #[test]
    fn pointer_moves_drags_and_releases_leave_the_completion_list_open() {
        let mut app = clicked_app();
        open_popup(&mut app);
        let lay = app.layout_for(Network::Teams);
        let g = ui::popup_geometry(&app, &lay).expect("popup drawn");
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(mv(&mut app, kind, 0, 0).is_empty());
            assert!(app.popup.is_some(), "{kind:?}");
        }
        left(&mut app, g.rows.x + 1, g.rows.y + 1);
        assert_eq!(app.active().draft(), "x BB");
    }

    #[test]
    fn a_terminal_too_small_to_draw_takes_no_clicks() {
        let mut app = clicked_app();
        app.resize(29, 10);
        app.prompt = Some(Prompt::MarkAllRead { count: 1 });
        let x = yes_x(&app);
        let y = app.size.height - 1;
        assert!(left(&mut app, x, y).is_empty());
        assert!(app.prompt.is_some(), "a stray click is never yes");
    }

    #[test]
    fn the_second_click_of_a_double_click_does_not_answer_the_question() {
        let mut app = clicked_app();
        let hit = span_of(&app, "m3").hits.first().expect("link hit").clone();
        let (x, y) = cell(&app, hit.line, hit.x0);
        left(&mut app, x, y);
        assert!(app.prompt.is_some());
        let (yx, yy) = (yes_x(&app), app.size.height - 1);
        left(&mut app, yx, yy);
        assert!(app.prompt.is_some() && app.open_urls.is_empty());
        // A deliberate click later answers.
        app.tick(app.clock().now_ms + 1000);
        left(&mut app, yx, yy);
        assert!(app.prompt.is_none());
        assert_eq!(app.take_open_urls().len(), 1);
    }

    #[test]
    fn the_send_and_quit_hints_are_not_clickable() {
        let app = clicked_app();
        let y = app.size.height - 1;
        let m = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(app.footer_hit(Position::new(0, y)), Some(Action::Send));
        assert_eq!(app.footer_click_hit(m), None);
    }
}
