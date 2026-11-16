//! Plays the guided tour (`crate::tour`): one timer through `next_deadline` / `fire_timers`, and
//! every key goes through `key_now`, the same handler typed keys use. The tour opens no browser
//! and no app, and raises no toast: it only presses keys the demo already handles.

use chatcli_core::model::Network;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState};
use ratatui::layout::Rect;

use super::{App, Focus, Out};
use crate::keymap::{Action, Context, KeyChord};
use crate::settings_view::RowId;
use crate::tour::{self as script, Cond, Prim, Tour};

/// Prims one `tour_tick` may run: a bound so `n` (no pauses) can never loop for ever.
const MAX_PRIMS: usize = 2000;

/// What the caption bar shows.
pub(crate) struct TourBar {
    pub text: String,
    pub hint: &'static str,
}

/// `area` without the caption row at the top.
pub(super) fn trim_top(area: Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    Rect::new(area.x, area.y + rows, area.width, area.height - rows)
}

impl App {
    /// Starts the tour from the chat list of Teams. Closes everything open, uses the default
    /// keys (the tour presses them), and plays from step 1.
    pub fn start_tour(&mut self) {
        self.pending = None;
        // Plain default keys, none of the user's: the tour's key presses must hit the actions
        // its captions name. The user's own are put back by `end_tour`.
        let saved = (
            std::mem::take(&mut self.keys),
            std::mem::take(&mut self.settings.keys),
        );
        self.show_lists();
        self.active = Network::Teams;
        let mut tour = Tour::new(self.clock.now_ms);
        tour.saved = Some(Box::new(saved));
        self.tour = Some(tour);
        self.relayout();
    }

    /// Ends the tour and puts the user's keys back. `clear`: also close whatever the tour left
    /// open (a key capture, a y/n prompt, a page).
    fn end_tour(&mut self, clear: bool) {
        if let Some(saved) = self.tour.take().and_then(|t| t.saved) {
            let (keys, settings) = *saved;
            self.keys = keys;
            self.settings.keys = settings;
            self.pending = None;
        }
        if clear {
            self.show_lists();
        }
        self.relayout();
    }

    pub fn tour_running(&self) -> bool {
        self.tour.is_some()
    }

    /// The last step is showing: any key leaves the tour.
    pub fn tour_done(&self) -> bool {
        self.tour.as_ref().is_some_and(|t| t.done)
    }

    /// The step playing, counted from 1.
    pub fn tour_step(&self) -> Option<usize> {
        self.tour.as_ref().map(|t| t.step + 1)
    }

    pub fn tour_paused(&self) -> bool {
        self.tour.as_ref().is_some_and(|t| t.paused)
    }

    /// The caption bar's text, while the tour shows.
    pub(crate) fn tour_bar(&self) -> Option<TourBar> {
        let t = self.tour.as_ref()?;
        Some(TourBar {
            text: t.bar_text(),
            hint: t.bar_hint(),
        })
    }

    /// Both tabs on their chat list, nothing open on top of it.
    fn show_lists(&mut self) {
        self.help = None;
        self.palette = None;
        self.settings_page = None;
        self.prompt = None;
        self.picker = None;
        self.search = None;
        self.popup = None;
        self.media.attach = None;
        for n in Network::ALL {
            let ns = self.ns_mut(n);
            ns.focus = Focus::List;
            ns.sel_msg = None;
        }
    }

    /// Clock time of the next tour action, unless it is paused or finished.
    pub(super) fn tour_deadline(&self) -> Option<i64> {
        self.tour
            .as_ref()
            .filter(|t| !t.paused && !t.done)
            .map(|t| t.due)
    }

    fn cond_holds(&self, c: Cond) -> bool {
        match c {
            Cond::Online(n) => matches!(
                self.ns(n).status,
                chatcli_core::model::Status::Online { .. }
            ),
            Cond::ChatLoaded => {
                let ns = self.active();
                ns.open
                    .as_ref()
                    .and_then(|id| ns.messages.get(id))
                    .is_some_and(|m| !m.is_empty())
            }
        }
    }

    /// Runs every action whose time has come.
    pub(super) fn tour_tick(&mut self) -> Out {
        let mut out = Vec::new();
        let now = self.clock.now_ms;
        for _ in 0..MAX_PRIMS {
            let Some(t) = self.tour.as_mut() else { break };
            if t.paused || t.done || t.due > now {
                break;
            }
            let Some(prim) = t.queue.pop_front() else {
                t.done = true;
                break;
            };
            let fast = t.fast;
            match prim {
                Prim::Wait(ms) => t.due = now + if fast { 0 } else { ms },
                Prim::End => {
                    t.step += 1;
                    t.queue = script::unroll(t.step);
                    t.fast = false;
                }
                Prim::Done => {
                    t.done = true;
                    t.fast = false;
                }
                Prim::Until(c) => {
                    let since = *t.until_since.get_or_insert(now);
                    if self.cond_holds(c) || now - since >= script::UNTIL_MAX_MS {
                        if let Some(t) = self.tour.as_mut() {
                            t.until_since = None;
                        }
                    } else if let Some(t) = self.tour.as_mut() {
                        t.queue.push_front(prim);
                        t.due = now + script::UNTIL_POLL_MS;
                    }
                }
                Prim::Key(code, modifiers) => {
                    out.extend(self.key_now(KeyEvent {
                        code,
                        modifiers,
                        kind: KeyEventKind::Press,
                        state: KeyEventState::NONE,
                    }));
                }
                Prim::Row(id) => self.tour_row(id),
                Prim::SelectMsg(text) => self.tour_select(text),
                Prim::ToList => self.show_lists(),
            }
        }
        out
    }

    /// Keys while the tour plays: Space pauses, Right or `n` finishes the step at once, Esc ends
    /// the tour, the `[keys.common]` quit key (Ctrl+C) quits. Anything else is ignored, so a stray key cannot derail it. On the
    /// last step any key ends the tour and is then handled as usual.
    pub(super) fn tour_key(&mut self, key: KeyEvent) -> Out {
        let chord = KeyChord::from_event(&key);
        if self.keys.lookup(Context::Common, chord) == Some(Action::Quit) {
            self.quit = true;
            return Vec::new();
        }
        let now = self.clock.now_ms;
        let Some(t) = self.tour.as_mut() else {
            return self.key_now(key);
        };
        if t.done {
            self.end_tour(false);
            return self.key_now(key);
        }
        match key.code {
            KeyCode::Char(' ') => {
                t.paused = !t.paused;
                t.due = now;
            }
            KeyCode::Right | KeyCode::Char('n') => {
                t.paused = false;
                t.fast = true;
                t.due = now;
                return self.tour_tick();
            }
            KeyCode::Esc => self.end_tour(true),
            _ => {}
        }
        Vec::new()
    }

    /// Settings page: the cursor goes to `id` (tour-only: no fixed number of Down presses).
    fn tour_row(&mut self, id: RowId) {
        let at = self.page_rows().iter().position(|r| r.id == id);
        if let (Some(p), Some(i)) = (self.settings_page.as_mut(), at) {
            p.in_rows = true;
            p.row = i;
            p.error = None;
        }
        self.settings_fit();
    }

    /// Message selection: the highlight goes to the newest message whose text starts with `text`.
    fn tour_select(&mut self, text: &str) {
        let n = self.active;
        let ns = self.ns_mut(n);
        let id = ns.open.as_ref().and_then(|open| {
            ns.messages
                .get(open)?
                .iter()
                .rev()
                .find(|m| m.text.starts_with(text))
                .map(|m| m.id.clone())
        });
        if id.is_some() && ns.focus == Focus::Select {
            ns.sel_msg = id;
            self.ensure_sel_visible(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Clock;
    use chatcli_core::settings::Settings;

    fn user_app() -> App {
        // The user moved `next_network` from Tab to F6.
        let mut s = Settings::default();
        let global = s.keys.bindings.entry("common".to_string()).or_default();
        global.insert("f6".to_string(), "next_network".to_string());
        global.insert("tab".to_string(), "unbound".to_string());
        App::with_settings(
            Clock {
                now_ms: 1_800_000_000_000,
                tz: chrono::FixedOffset::east_opt(0),
            },
            &s,
        )
    }

    fn run_ms(app: &mut App, ms: i64) -> Vec<Network> {
        let mut seen = Vec::new();
        for _ in 0..ms / 250 {
            app.clock.now_ms += 250;
            app.fire_timers();
            seen.push(app.active().network);
        }
        seen
    }

    #[test]
    fn the_tour_plays_on_default_keys_and_gives_the_users_keys_back() {
        let mut app = user_app();
        let own = app.keys().user_bindings();
        assert!(own > 0);
        app.start_tour();
        assert_eq!(app.keys().user_bindings(), 0);
        // Step 2 presses Tab: with the user's keys it would do nothing.
        let seen = run_ms(&mut app, 8_000);
        assert!(
            seen.contains(&Network::WhatsApp),
            "Tab did not switch network"
        );
        app.tour_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.tour_running());
        assert_eq!(app.keys().user_bindings(), own);
        assert_eq!(app.settings.keys.bindings["common"]["f6"], "next_network");
    }
}
