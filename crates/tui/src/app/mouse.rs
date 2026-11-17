//! Mouse on what is open over the main screen: help, the y/n question, the attach prompt,
//! search, the command bar and the settings page; and a click on a footer key hint. The main
//! screen's own mouse handling is `App::handle_mouse` in app.rs.
//!
//! A click only starts what the matching key starts (same functions, same questions): it never
//! sends, deletes or opens anything the key would not.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Margin, Position};

use super::{App, Out, WHEEL_LINES};
use crate::keymap::{Action, Context};
use crate::ui::{self, FooterHit};

/// A click this soon after a click raised a question is ignored by that question.
const DOUBLE_CLICK_MS: i64 = 400;

impl App {
    /// The mouse while an overlay is open (the order the keys use: help, a question, the rest).
    pub(super) fn overlay_mouse(&mut self, m: MouseEvent) -> Out {
        let pos = Position::new(m.column, m.row);
        let up = match m.kind {
            MouseEventKind::Down(MouseButton::Left) => None,
            MouseEventKind::ScrollUp => Some(true),
            MouseEventKind::ScrollDown => Some(false),
            _ => return Vec::new(),
        };
        if self.help.is_some() {
            self.help_mouse(pos, up);
        } else if self.prompt.is_some() {
            if up.is_none() {
                return self.prompt_click(pos);
            }
        } else if self.settings_page.is_some() {
            return match up {
                None => self.settings_click(pos),
                Some(up) => {
                    self.settings_wheel(up);
                    Vec::new()
                }
            };
        } else if self.palette.is_some() {
            return match up {
                None => self.palette_click(pos),
                Some(up) => {
                    self.palette_wheel(up);
                    Vec::new()
                }
            };
        } else if self.search.is_some() {
            return match up {
                None => self.search_click(pos),
                Some(up) => {
                    self.search_wheel(up);
                    Vec::new()
                }
            };
        } else if self.media.attach.is_some() {
            self.attach_mouse(pos, up);
        }
        Vec::new()
    }

    /// Help: the wheel scrolls, a click outside the box closes it.
    fn help_mouse(&mut self, pos: Position, up: Option<bool>) {
        match up {
            Some(up) => self.scroll_help(if up { -WHEEL_LINES } else { WHEEL_LINES }),
            None => {
                let body = self.layout_in(self.size).body;
                if !ui::help_fits(self.size, body) || !ui::help_box(body).contains(pos) {
                    self.help = None;
                }
            }
        }
    }

    /// The y/n question: the drawn y answers yes, the drawn n answers no, nothing else answers.
    fn prompt_click(&mut self, pos: Position) -> Out {
        // The second press of a double-click must not answer the question the first one raised.
        if self.clock.now_ms - self.prompt_opened_ms < DOUBLE_CLICK_MS {
            return Vec::new();
        }
        let footer = self.layout_in(self.size).footer;
        match ui::footer_hit_at(self, footer, self.size.height, pos) {
            Some(FooterHit::Yes) => self.run_click_action(Action::ConfirmYes),
            Some(FooterHit::No) => self.run_click_action(Action::ConfirmNo),
            _ => Vec::new(),
        }
    }

    /// The attach prompt's file list: a click on an entry chooses it like Enter (a folder is
    /// entered, a file attaches), the wheel moves the choice.
    fn attach_mouse(&mut self, pos: Position, up: Option<bool>) {
        let n = self.active;
        let Some(a) = self.media.attach.as_ref() else {
            return;
        };
        let Some(up) = up else {
            let lay = self.layout_for(n);
            let Some((list, shown)) = ui::attach_list(&lay, a.cands.len()) else {
                return;
            };
            let rows = list.inner(Margin::new(1, 1));
            if !rows.contains(pos) {
                return;
            }
            let i = ui::attach_top(a.sel, shown) + (pos.y - rows.y) as usize;
            let Some(dir) = a.cands.get(i).map(|c| c.dir) else {
                return;
            };
            if let Some(a) = self.media.attach.as_mut() {
                a.sel = i;
            }
            // Tab puts the entry in the box; Enter then attaches a file (a folder just opened).
            self.attach_complete();
            if !dir {
                self.attach_action(n, Action::Accept);
            }
            return;
        };
        let key = if up { Action::Up } else { Action::Down };
        for _ in 0..WHEEL_LINES {
            self.attach_action(n, key);
        }
    }

    /// The action of the footer hint under `pos`, if one is drawn there whole.
    pub(crate) fn footer_hit(&self, pos: Position) -> Option<Action> {
        let footer = self.layout_in(self.size).footer;
        match ui::footer_hit_at(self, footer, self.size.height, pos)? {
            FooterHit::Hint(a) => Some(a),
            FooterHit::Yes | FooterHit::No => None,
        }
    }

    /// The footer hint a left click lands on. Taken before a held vim `g` is cleared, so the
    /// hit map is the footer as drawn.
    pub(super) fn footer_click_hit(&self, m: MouseEvent) -> Option<Action> {
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        // Send and Quit have no question after them, so a stray click must not run them.
        self.footer_hit(Position::new(m.column, m.row))
            .filter(|a| !matches!(a, Action::Send | Action::Quit))
    }

    /// An action started by the pointer, with the same after-effects a key press has (typing
    /// state, the `:shortcode` list).
    pub(super) fn run_click_action(&mut self, action: Action) -> Out {
        self.popup_alive = false;
        let (net, chat) = (self.active, self.active().open.clone());
        let was_empty = self.active().draft().is_empty();
        let ctx = self.context();
        // The completion list owns Up, Down, Enter and Esc over the message box, as for keys.
        let list_key = matches!(
            action,
            Action::Up | Action::Down | Action::Accept | Action::Back
        );
        let mut out = if self.popup.is_some() && ctx == Context::Composer && list_key {
            self.popup_action(action, ctx)
        } else {
            self.run_action(action, ctx)
        };
        out.extend(self.composing_after(net, chat, was_empty));
        if self.popup_alive {
            out.extend(self.refresh_popup());
        } else {
            self.popup = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Prompt;
    use crate::settings_view::{PageEdit, RowId, Section};
    use crate::text::Clock;
    use chatcli_core::model::{ChatSummary, Event, Network, Status};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        let mut a = App::new(Clock {
            now_ms: 1_800_000_000_000,
            tz: chrono::FixedOffset::east_opt(0),
        });
        a.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        a.handle_event(Event::Chats(
            Network::Teams,
            vec![ChatSummary {
                network: Network::Teams,
                id: "a".into(),
                title: "Alex Example".into(),
                last_ts: Some(2000),
                last_preview: "hi".into(),
                unread: 2,
                ..ChatSummary::blank(Network::Teams)
            }],
        ));
        a
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ev(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(a: &mut App, x: u16, y: u16) -> Out {
        a.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), x, y))
    }

    fn wheel(a: &mut App, up: bool) {
        let kind = if up {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        };
        a.handle_mouse(ev(kind, 10, 10));
    }

    /// The footer column where `want` is drawn.
    fn footer_x(a: &App, want: FooterHit) -> u16 {
        let footer = a.layout_in(a.size).footer;
        let y = a.size.height - 1;
        (0..a.size.width)
            .find(|x| {
                ui::footer_hit_at(a, footer, a.size.height, Position::new(*x, y)) == Some(want)
            })
            .expect("the hit is drawn on the footer")
    }

    fn footer_y(a: &App) -> u16 {
        a.size.height - 1
    }

    #[test]
    fn a_footer_hint_click_runs_its_action_and_a_gap_does_nothing() {
        let mut a = app();
        let y = footer_y(&a);
        let x = footer_x(&a, FooterHit::Hint(Action::CommandBar));
        click(&mut a, x, y);
        assert!(a.palette.is_some());

        let mut a = app();
        let x = footer_x(&a, FooterHit::Hint(Action::Help));
        click(&mut a, x, y);
        assert!(a.help.is_some());

        // The far right of the row is empty: nothing opens, nothing changes.
        let mut a = app();
        let far = a.size.width - 1;
        let out = click(&mut a, far, y);
        assert!(out.is_empty() && a.palette.is_none() && a.help.is_none());
        assert_eq!(a.footer_hit(Position::new(a.size.width - 1, y)), None);
    }

    #[test]
    fn the_slash_hint_in_an_empty_message_box_opens_the_command_bar() {
        let mut a = app();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.context(), Context::Composer);
        let x = footer_x(&a, FooterHit::Hint(Action::CommandBar));
        let y = footer_y(&a);
        click(&mut a, x, y);
        assert!(a.palette.is_some());
    }

    #[test]
    fn help_wheel_scrolls_and_a_click_outside_the_box_closes_it() {
        let mut a = app();
        a.help = Some(0);
        wheel(&mut a, false);
        assert_eq!(a.help, Some(3));
        wheel(&mut a, true);
        assert_eq!(a.help, Some(0));
        click(&mut a, 10, 10);
        assert!(a.help.is_some(), "inside the box");
        click(&mut a, 10, 0);
        assert!(a.help.is_none(), "the header row is outside the box");
    }

    #[test]
    fn only_the_drawn_y_and_n_answer_a_question() {
        for (hit, silent) in [(FooterHit::No, true), (FooterHit::Yes, false)] {
            let mut a = app();
            a.prompt = Some(Prompt::MarkAllRead { count: 1 });
            let row = footer_y(&a);
            // Anywhere else: still asking, nothing sent.
            assert!(click(&mut a, 0, row).is_empty());
            assert!(click(&mut a, 5, 5).is_empty());
            assert!(a.prompt.is_some());
            let x = footer_x(&a, hit);
            let out = click(&mut a, x, row);
            assert!(a.prompt.is_none());
            assert_eq!(out.is_empty(), silent, "{hit:?}");
        }
        // A click on the slash between them is neither.
        let mut a = app();
        a.prompt = Some(Prompt::MarkAllRead { count: 1 });
        let slash = footer_x(&a, FooterHit::Yes) + 1;
        let y = footer_y(&a);
        click(&mut a, slash, y);
        assert!(a.prompt.is_some());
    }

    #[test]
    fn the_command_bar_runs_a_clicked_item_scrolls_and_closes_outside() {
        let mut a = app();
        a.open_palette();
        a.palette_type("help");
        let g = ui::palette_geometry(a.layout_in(a.size).body).unwrap();
        // Row 0 is the "Commands" heading: a click on it does nothing.
        click(&mut a, g.list.x + 1, g.list.y);
        assert!(a.palette.is_some());
        click(&mut a, g.list.x + 1, g.list.y + 1);
        assert!(a.palette.is_none());
        assert!(a.help.is_some(), "/help ran");

        let mut a = app();
        a.open_palette();
        wheel(&mut a, false);
        assert_eq!(a.palette.as_ref().unwrap().sel, 3);
        wheel(&mut a, true);
        assert_eq!(a.palette.as_ref().unwrap().sel, 0);
        click(&mut a, 10, 0);
        assert!(a.palette.is_none(), "outside the box");
    }

    #[test]
    fn settings_click_shows_a_section_selects_a_row_and_a_second_click_changes_it() {
        let mut a = app();
        a.handle_key(key(KeyCode::F(2)));
        assert!(a.settings_page.is_some());
        let g = crate::settings_ui::geometry(a.layout_in(a.size).body).unwrap();
        let left = g.left.unwrap();
        // Section names start on the pane's first inner row.
        click(&mut a, left.x + 2, left.y + 2);
        assert_eq!(a.settings_page.as_ref().unwrap().section, Section::Layout);
        click(&mut a, left.x + 2, left.y + 1);
        let p = a.settings_page.as_ref().unwrap();
        assert_eq!((p.section, p.in_rows), (Section::Appearance, false));

        let k = a.page_rows().iter().position(|r| r.selectable()).unwrap();
        let at = g.list.y + k as u16;
        click(&mut a, g.list.x + 1, at);
        let p = a.settings_page.as_ref().unwrap();
        assert!(p.in_rows && p.row == k);
        assert!(
            a.take_setting_edits().is_empty(),
            "the first click only selects"
        );
        click(&mut a, g.list.x + 1, at);
        assert!(
            !a.take_setting_edits().is_empty(),
            "the second click changes it"
        );

        wheel(&mut a, false);
        assert!(a.settings_page.as_ref().unwrap().row > k);
    }

    #[test]
    fn a_second_click_on_a_key_row_starts_capture_and_the_next_cancels_it() {
        let mut a = app();
        a.open_settings(Section::Keys);
        let g = crate::settings_ui::geometry(a.layout_in(a.size).body).unwrap();
        let k = a
            .page_rows()
            .iter()
            .position(|r| matches!(r.id, RowId::KeyAction(..)))
            .unwrap();
        assert!(k < g.list.height as usize);
        let at = g.list.y + k as u16;
        click(&mut a, g.list.x + 1, at);
        assert!(a.settings_page.as_ref().unwrap().edit.is_none());
        click(&mut a, g.list.x + 1, at);
        assert!(matches!(
            a.settings_page.as_ref().unwrap().edit,
            Some(PageEdit::Capture { .. })
        ));
        // A click while a key is wanted is Esc: nothing binds.
        click(&mut a, g.list.x + 1, at);
        assert!(a.settings_page.as_ref().unwrap().edit.is_none());
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn a_click_on_reset_all_keys_asks_first() {
        let mut a = app();
        a.open_settings(Section::Keys);
        let g = crate::settings_ui::geometry(a.layout_in(a.size).body).unwrap();
        let rows = a.page_rows();
        let k = rows
            .iter()
            .position(|r| r.id == RowId::ResetAllKeys)
            .unwrap();
        if let Some(p) = a.settings_page.as_mut() {
            p.in_rows = true;
            p.row = k;
        }
        a.settings_fit();
        let p = a.settings_page.as_ref().unwrap();
        let top = crate::list::keep_visible(p.top, p.row, g.list.height as usize, rows.len());
        let at = g.list.y + (k - top) as u16;
        click(&mut a, g.list.x + 1, at);
        assert!(matches!(a.prompt, Some(Prompt::ResetKeys)));
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn the_attach_list_chooses_a_file_enters_a_folder_and_the_wheel_moves() {
        let dir = std::env::temp_dir().join(format!("chatcli-mouse-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("adir")).unwrap();
        std::fs::write(dir.join("b.txt"), b"hello").unwrap();
        let mut a = app();
        a.set_attach_dir(dir.clone());
        a.handle_key(key(KeyCode::Enter));
        a.start_attach(Network::Teams);
        let cands = a.media.attach.as_ref().unwrap().cands.clone();
        assert_eq!(cands.len(), 2);
        assert!(cands[0].dir && !cands[1].dir, "folders first");
        let row_y = |a: &App, i: usize| {
            let lay = a.layout_for(a.active);
            let (list, shown) = ui::attach_list(&lay, 2).unwrap();
            let rows = list.inner(Margin::new(1, 1));
            (
                rows.x + 1,
                rows.y + (i - ui::attach_top(a.media.attach.as_ref().unwrap().sel, shown)) as u16,
            )
        };
        wheel(&mut a, false);
        assert_eq!(
            a.media.attach.as_ref().unwrap().sel,
            1,
            "clamped, not wrapped"
        );
        wheel(&mut a, true);
        assert_eq!(a.media.attach.as_ref().unwrap().sel, 0);

        // The folder: entered, the prompt stays.
        let (x, y) = row_y(&a, 0);
        click(&mut a, x, y);
        let p = a.media.attach.as_ref().unwrap();
        assert!(p.input.text().starts_with("adir"));
        // Back to the top level and click the file: it attaches and the prompt closes.
        a.media.attach.as_mut().unwrap().input.clear();
        a.attach_refresh();
        let (x, y) = row_y(&a, 1);
        click(&mut a, x, y);
        assert!(a.media.attach.is_none());
        assert!(a.active().attachment().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
