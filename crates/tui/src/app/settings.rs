//! The settings screen's behaviour: keys, live apply and the save queue. `App` stays pure: a
//! change takes effect at once, and the `settings.toml` edits wait in `setting_edits` for the
//! run loop (`take_setting_edits`), like the other `take_*` queues.

use std::sync::Arc;

use chatcli_core::model::{Command, Network};
use chatcli_core::paths;
use chatcli_core::runtime::UiCtx;
use chatcli_core::settings::{SettingEdit, Settings};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Out, Prompt};
use crate::keymap::{Action, Context, KeyChord, Keymap, Preset, CTRL_C};
use crate::keys_edit::{self, Adding, Captured};
use crate::settings_view::{self, Change, Kind, PageEdit, Row, RowId, Section, SettingsPage};

/// The About page's lines. Tests built without a context show `-` for the paths.
pub(super) fn about_lines(ctx: &UiCtx) -> Vec<(&'static str, String)> {
    let shown = |p: Result<std::path::PathBuf, String>| {
        p.map_or_else(|_| "-".to_string(), |p| p.display().to_string())
    };
    let settings = ctx.settings_path.as_ref().map_or("-".to_string(), |p| {
        let text = p.display().to_string();
        if ctx.demo {
            format!("{text} (demo copy, deleted on exit)")
        } else {
            text
        }
    });
    vec![
        (
            "Version",
            concat!("chatcli ", env!("CARGO_PKG_VERSION")).to_string(),
        ),
        ("Data folder", shown(paths::data_dir_path())),
        ("Log file", shown(paths::log_file_path())),
        ("Settings file", settings),
    ]
}

impl App {
    /// Opens the settings screen on `section`.
    pub(super) fn open_settings(&mut self, section: Section) {
        self.popup = None;
        self.settings_page = Some(SettingsPage::new(section));
        self.settings_fit();
    }

    /// The rows of the page that is open (empty when none is).
    pub(crate) fn page_rows(&self) -> Vec<Row> {
        match &self.settings_page {
            Some(p) => {
                let mut rows = settings_view::rows(
                    p.section,
                    &self.settings,
                    &self.keys,
                    &self.about,
                    &self.file_problems,
                );
                // NO_COLOR strips every colour: say so, or a colour change looks broken.
                if p.section == Section::Appearance && self.no_colour {
                    rows.insert(0, settings_view::no_colour_row());
                }
                rows
            }
            None => Vec::new(),
        }
    }

    /// Settings edits waiting to be written to `settings.toml`. The run loop saves them.
    pub fn take_setting_edits(&mut self) -> Vec<SettingEdit> {
        std::mem::take(&mut self.setting_edits)
    }

    /// The run loop saved the file: `problems` is what is wrong with it now.
    pub fn settings_saved(&mut self, problems: Vec<String>) {
        // The problems header comes and goes above the rows: keep the cursor on its row.
        let was = self.current_row().map(|r| r.id);
        self.file_problems = problems;
        if let Some(id) = was {
            self.keep_row(id);
        }
    }

    /// Puts the cursor back on the row `id` after the rows moved.
    fn keep_row(&mut self, id: RowId) {
        let at = self.page_rows().iter().position(|r| r.id == id);
        if let (Some(p), Some(i)) = (self.settings_page.as_mut(), at) {
            p.row = i;
        }
        self.settings_fit();
    }

    /// The run loop could not save: one red line. The change still applies for this session.
    pub fn settings_save_failed(&mut self, error: &str) {
        self.push_global_notice(error, true);
        if let Some(p) = self.settings_page.as_mut() {
            p.error = Some(error.to_string());
        }
    }

    /// Makes `new` the live settings: theme, keymap, layout, notifications and the rest follow
    /// at once. The backends get a copy when something they read changed.
    pub(crate) fn apply_settings(&mut self, new: Settings) -> Out {
        let old = &self.settings;
        let backend =
            old.teams != new.teams || old.whatsapp != new.whatsapp || old.media != new.media;
        if old.keys != new.keys {
            let named = (old.keys.preset != new.keys.preset)
                .then(|| crate::keymap::Preset::from_name(&new.keys.preset))
                .flatten();
            let preset = named.unwrap_or(self.keys.preset());
            self.keys = Keymap::build(preset, &new.keys).0;
            self.pending = None;
        }
        self.apply_view(&new);
        self.settings = new;
        self.relayout();
        if !backend {
            return Vec::new();
        }
        let shared = Arc::new(self.settings.clone());
        Network::ALL
            .iter()
            .map(|n| (*n, Command::Settings(shared.clone())))
            .collect()
    }

    /// Applies a change and queues its file edits.
    fn commit_change(&mut self, new: Settings, edits: Vec<SettingEdit>) -> Out {
        let out = self.apply_settings(new);
        self.setting_edits.extend(edits);
        out
    }

    /// `/theme <name>`: opens the Appearance page and switches to that theme.
    pub(super) fn cmd_theme(&mut self, name: Option<&str>) -> Out {
        self.open_settings(Section::Appearance);
        let Some(name) = name else {
            return Vec::new();
        };
        match settings_view::change(
            &self.settings,
            RowId::ThemeName,
            Change::Text(name.to_string()),
        ) {
            Ok((new, edits)) => self.commit_change(new, edits),
            Err(msg) => {
                if let Some(p) = self.settings_page.as_mut() {
                    p.error = Some(msg);
                }
                Vec::new()
            }
        }
    }

    pub(crate) fn current_row(&self) -> Option<Row> {
        let p = self.settings_page.as_ref()?;
        self.page_rows().into_iter().nth(p.row)
    }

    /// Keeps the chosen row in range and on screen.
    pub(super) fn settings_fit(&mut self) {
        let rows = self.page_rows().len();
        let shown = crate::settings_ui::rows_shown(self.layout_in(self.size).body);
        if let Some(p) = self.settings_page.as_mut() {
            p.row = p.row.min(rows.saturating_sub(1));
            p.top = settings_view::fit_top(p.top, p.row, shown, rows);
        }
    }

    /// Moves to the next selectable row in `step` direction, wrapping.
    fn move_row(&mut self, step: isize) {
        let rows = self.page_rows();
        let n = rows.len() as isize;
        let Some(p) = self.settings_page.as_mut() else {
            return;
        };
        for k in 1..=n {
            let i = (p.row as isize + step * k).rem_euclid(n.max(1)) as usize;
            if rows.get(i).is_some_and(Row::selectable) {
                p.row = i;
                break;
            }
        }
        self.settings_fit();
    }

    fn move_section(&mut self, step: isize) {
        if let Some(p) = self.settings_page.as_mut() {
            p.section = p.section.step(step);
            p.row = 0;
            p.top = 0;
        }
        self.settings_fit();
    }

    /// Into the rows of the current section. Stays on the sections when none can be changed.
    fn enter_rows(&mut self) {
        let first = self.page_rows().iter().position(Row::selectable);
        if let (Some(p), Some(i)) = (self.settings_page.as_mut(), first) {
            p.in_rows = true;
            p.row = i;
        }
        self.settings_fit();
    }

    /// One change to the chosen row; a refusal is the inline red line.
    fn change_row(&mut self, c: Change) -> Out {
        let Some(row) = self.current_row().filter(Row::selectable) else {
            return Vec::new();
        };
        match row.id {
            RowId::KeyPreset => return self.change_preset(c == Change::Reset),
            RowId::KeyAction(ctx, action) if c == Change::Reset => {
                let done = keys_edit::reset_action(&self.keys, &self.settings.keys, ctx, action);
                return self.key_result(done);
            }
            _ => {}
        }
        match settings_view::change(&self.settings, row.id, c) {
            Ok((new, edits)) => self.commit_change(new, edits),
            Err(msg) => {
                self.set_error(Some(msg));
                Vec::new()
            }
        }
    }

    fn set_error(&mut self, error: Option<String>) {
        if let Some(p) = self.settings_page.as_mut() {
            p.error = error;
        }
    }

    fn set_edit(&mut self, edit: Option<PageEdit>) {
        if let Some(p) = self.settings_page.as_mut() {
            p.edit = edit;
        }
    }

    /// The key preset row: Left/Right/Space/Enter switch to the other preset (from the one that
    /// is live, which `--keys` may have set), `r` goes back to the default. Saved as
    /// `[keys] preset`.
    fn change_preset(&mut self, reset: bool) -> Out {
        let target = if reset {
            Preset::Default
        } else {
            self.keys.preset().other()
        };
        let c = if reset {
            Change::Reset
        } else {
            Change::Text(target.name().to_string())
        };
        match settings_view::change(&self.settings, RowId::KeyPreset, c) {
            Ok((new, edits)) => {
                let out = self.commit_change(new, edits);
                // The file may already name this preset while `--keys` held the other one.
                if self.keys.preset() != target {
                    self.keys = Keymap::build(target, &self.settings.keys).0;
                    self.pending = None;
                }
                self.keep_row(RowId::KeyPreset);
                out
            }
            Err(msg) => {
                self.set_error(Some(msg));
                Vec::new()
            }
        }
    }

    /// A key edit from the keys page: applied at once and queued for the file, or the red line.
    fn key_result(&mut self, done: keys_edit::Done) -> Out {
        match done {
            Ok((keys, edits)) => {
                let mut new = self.settings.clone();
                new.keys = keys;
                self.commit_change(new, edits)
            }
            Err(msg) => {
                self.set_error(Some(msg));
                Vec::new()
            }
        }
    }

    /// The y of "Reset every key to the preset? y/n".
    pub(super) fn reset_all_keys(&mut self) -> Out {
        let done = keys_edit::reset_all(&self.settings.keys);
        self.key_result(done)
    }

    /// One key press while a key is being captured, or a swap is asked. The key is read here,
    /// before the keymap, so a key that already does something can still be chosen. Only Ctrl+C
    /// (quits), Esc (cancels) and Backspace (removes the first key) are not captured.
    fn capture_key(&mut self, chord: KeyChord, code: KeyCode) -> Out {
        if chord == CTRL_C {
            self.quit = true;
            return Vec::new();
        }
        self.set_error(None);
        let edit = self.settings_page.as_ref().and_then(|p| p.edit.clone());
        match edit {
            Some(PageEdit::Conflict {
                ctx,
                action,
                key,
                other,
            }) => match code {
                KeyCode::Enter => {
                    self.set_edit(None);
                    let done =
                        keys_edit::swap(&self.keys, &self.settings.keys, ctx, action, key, other);
                    return self.key_result(done);
                }
                KeyCode::Esc => self.set_edit(None),
                _ => {}
            },
            Some(PageEdit::Add { ctx, action }) => {
                match keys_edit::capture_add(&self.keys, ctx, action, chord) {
                    Adding::Cancel | Adding::Same => self.set_edit(None),
                    Adding::Refused(msg) => self.set_error(Some(msg)),
                    Adding::Overlaps(note) => self.set_edit(Some(PageEdit::AddOverlap {
                        ctx,
                        action,
                        key: chord,
                        note,
                    })),
                    Adding::Free => {
                        self.set_edit(None);
                        let done =
                            keys_edit::add(&self.keys, &self.settings.keys, ctx, action, chord);
                        return self.key_result(done);
                    }
                }
            }
            Some(PageEdit::AddOverlap {
                ctx, action, key, ..
            }) => match code {
                KeyCode::Enter => {
                    self.set_edit(None);
                    let done = keys_edit::add(&self.keys, &self.settings.keys, ctx, action, key);
                    return self.key_result(done);
                }
                KeyCode::Esc => self.set_edit(None),
                _ => {}
            },
            Some(PageEdit::Pick { ctx, action, index }) => {
                return self.pick_key(chord, code, ctx, action, index)
            }
            Some(PageEdit::Capture { ctx, action }) => {
                match keys_edit::capture(&self.keys, ctx, action, chord) {
                    Captured::Cancel | Captured::Same => self.set_edit(None),
                    Captured::Refused(msg) => self.set_error(Some(msg)),
                    Captured::Conflict(other) => self.set_edit(Some(PageEdit::Conflict {
                        ctx,
                        action,
                        key: chord,
                        other,
                    })),
                    Captured::Remove => {
                        match keys_edit::remove_first(&self.keys, &self.settings.keys, ctx, action)
                        {
                            Ok(done) => {
                                self.set_edit(None);
                                return self.key_result(Ok(done));
                            }
                            // A locked key: say why and keep listening.
                            Err(msg) => self.set_error(Some(msg)),
                        }
                    }
                    Captured::Bind => {
                        self.set_edit(None);
                        let done =
                            keys_edit::bind(&self.keys, &self.settings.keys, ctx, action, chord);
                        return self.key_result(done);
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// One key press while choosing which key of `action` to delete. Left/Right (h/l) move,
    /// Enter or d deletes, Esc leaves. A locked key is refused and the choice stays open.
    fn pick_key(
        &mut self,
        chord: KeyChord,
        code: KeyCode,
        ctx: Context,
        action: Action,
        index: usize,
    ) -> Out {
        let keys = self.keys.keys_in(ctx, action);
        if keys.is_empty() {
            self.set_edit(None);
            return Vec::new();
        }
        let plain = !chord
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let n = keys.len();
        let index = index.min(n - 1);
        let moved = |index| Some(PageEdit::Pick { ctx, action, index });
        match (code, plain) {
            (KeyCode::Left | KeyCode::Char('h'), true) => self.set_edit(moved((index + n - 1) % n)),
            (KeyCode::Right | KeyCode::Char('l'), true) => self.set_edit(moved((index + 1) % n)),
            (KeyCode::Enter | KeyCode::Char('d'), true) => {
                match keys_edit::remove_key(
                    &self.keys,
                    &self.settings.keys,
                    ctx,
                    action,
                    keys[index],
                ) {
                    Ok(done) => {
                        self.set_edit(None);
                        return self.key_result(Ok(done));
                    }
                    Err(msg) => self.set_error(Some(msg)),
                }
            }
            (KeyCode::Esc, _) => self.set_edit(None),
            _ => {}
        }
        Vec::new()
    }

    /// `a` and `d` on a key row: add a key, or choose one to delete.
    fn start_key_edit(&mut self, add: bool) {
        let Some(RowId::KeyAction(ctx, action)) = self.current_row().map(|r| r.id) else {
            return;
        };
        if add {
            self.set_edit(Some(PageEdit::Add { ctx, action }));
        } else if self.keys.keys_in(ctx, action).is_empty() {
            self.set_error(Some(format!("{} has no keys to delete", action.label())));
        } else {
            self.set_edit(Some(PageEdit::Pick {
                ctx,
                action,
                index: 0,
            }));
        }
    }

    /// Enter on a row: a number, width or colour starts typing, the rest change.
    fn enter_row(&mut self) -> Out {
        let Some(row) = self.current_row().filter(Row::selectable) else {
            return Vec::new();
        };
        match row.id {
            RowId::KeyAction(ctx, action) => {
                self.set_edit(Some(PageEdit::Capture { ctx, action }));
                return Vec::new();
            }
            RowId::ResetAllKeys => {
                self.prompt = Some(Prompt::ResetKeys);
                return Vec::new();
            }
            _ => {}
        }
        if row.kind == Kind::Edit {
            let start = settings_view::edit_start(&self.settings, row.id);
            if let Some(p) = self.settings_page.as_mut() {
                p.edit = Some(PageEdit::Text(start.into()));
            }
            return Vec::new();
        }
        self.change_row(Change::Next)
    }

    /// Enter while typing: a good value applies and saves, a bad one stays open with the error.
    fn finish_text(&mut self, text: String) -> Out {
        let out = self.change_row(Change::Text(text));
        if let Some(p) = self.settings_page.as_mut() {
            if p.error.is_none() {
                p.edit = None;
            }
        }
        out
    }

    /// Typed or pasted text into the value being edited. False when nothing is being edited.
    pub(super) fn settings_type(&mut self, text: &str) -> bool {
        match self.settings_page.as_mut().and_then(|p| p.edit.as_mut()) {
            Some(PageEdit::Text(buf)) => {
                buf.insert_str(text);
                if let Some(p) = self.settings_page.as_mut() {
                    p.error = None;
                }
                true
            }
            _ => false,
        }
    }

    /// Keys while the settings screen is open: fixed, not in the keymap. Ctrl+C quits (the
    /// keymap hard-wires it), F1 opens help, F2 and Esc close.
    pub(super) fn settings_key(&mut self, key: &KeyEvent) -> Out {
        let chord = KeyChord::from_event(key);
        let capturing = self
            .settings_page
            .as_ref()
            .and_then(|p| p.edit.as_ref())
            .is_some_and(PageEdit::is_key_edit);
        if capturing {
            return self.capture_key(chord, key.code);
        }
        if chord == CTRL_C {
            self.quit = true;
            return Vec::new();
        }
        // Global Quit is checked last (`settings_global`): the page's own keys win.
        let global = self.keys.lookup(Context::Global, chord);
        let Some(page) = self.settings_page.as_mut() else {
            return Vec::new();
        };
        page.error = None;
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let Some(PageEdit::Text(buf)) = page.edit.as_mut() {
            if let Some(c) = crate::keymap::typed_char(key) {
                buf.insert_char(c);
                return Vec::new();
            }
            match key.code {
                KeyCode::Backspace => buf.delete_back(),
                KeyCode::Enter => {
                    let text = buf.text().to_string();
                    return self.finish_text(text);
                }
                KeyCode::Esc => page.edit = None,
                _ => return self.settings_global(global, key),
            }
            return Vec::new();
        }
        let in_rows = page.in_rows;
        match (key.code, plain) {
            (KeyCode::Up | KeyCode::Char('k'), true) => {
                if in_rows {
                    self.move_row(-1)
                } else {
                    self.move_section(-1)
                }
            }
            (KeyCode::Down | KeyCode::Char('j'), true) => {
                if in_rows {
                    self.move_row(1)
                } else {
                    self.move_section(1)
                }
            }
            (KeyCode::Tab, _) if in_rows => {
                if let Some(p) = self.settings_page.as_mut() {
                    p.in_rows = false;
                }
            }
            (KeyCode::Tab | KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'), true)
                if !in_rows =>
            {
                self.enter_rows()
            }
            (KeyCode::Left | KeyCode::Char('h'), true) if in_rows => {
                return self.change_row(Change::Prev)
            }
            (KeyCode::Right | KeyCode::Char('l'), true) if in_rows => {
                return self.change_row(Change::Next)
            }
            (KeyCode::Char(' '), true) if in_rows => return self.change_row(Change::Toggle),
            (KeyCode::Enter, _) if in_rows => return self.enter_row(),
            (KeyCode::Char('r'), true) if in_rows => return self.change_row(Change::Reset),
            (KeyCode::Char(c @ ('a' | 'd')), true) if in_rows => self.start_key_edit(c == 'a'),
            _ => return self.settings_global(global, key),
        }
        Vec::new()
    }

    /// F1, F2 and Esc.
    fn settings_global(&mut self, global: Option<Action>, key: &KeyEvent) -> Out {
        match global {
            Some(Action::Quit) => self.quit = true,
            Some(Action::Settings) => self.settings_page = None,
            Some(Action::Help) => self.help = Some(0),
            _ if key.code == KeyCode::Esc => self.settings_page = None,
            _ => {}
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Clock;
    use crate::theme::Theme;
    use chatcli_core::settings::{Density, SettingValue};
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn app() -> App {
        App::new(Clock {
            now_ms: 1_800_000_000_000,
            tz: chrono::FixedOffset::east_opt(0),
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Out {
        app.handle_key(key(code))
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// The page open on `section`, the cursor on its first row.
    fn in_section(section: Section) -> App {
        let mut a = app();
        a.open_settings(section);
        press(&mut a, KeyCode::Enter);
        a
    }

    fn row_id(a: &App) -> RowId {
        a.current_row().unwrap().id
    }

    fn goto(a: &mut App, id: RowId) {
        for _ in 0..400 {
            if row_id(a) == id {
                return;
            }
            press(a, KeyCode::Down);
        }
        panic!("row {id:?} not found, on {:?}", row_id(a));
    }

    #[test]
    fn f2_opens_the_page_and_f2_or_esc_close_it() {
        let mut a = app();
        press(&mut a, KeyCode::F(2));
        assert!(a.settings_page().is_some());
        press(&mut a, KeyCode::F(2));
        assert!(a.settings_page().is_none());
        press(&mut a, KeyCode::F(2));
        press(&mut a, KeyCode::Esc);
        assert!(a.settings_page().is_none());
    }

    #[test]
    fn f1_over_the_page_opens_help_and_esc_returns_to_the_page() {
        let mut a = app();
        press(&mut a, KeyCode::F(2));
        press(&mut a, KeyCode::F(1));
        assert!(a.help_open().is_some());
        press(&mut a, KeyCode::Esc);
        assert!(a.help_open().is_none());
        assert!(a.settings_page().is_some(), "Esc closed help only");
    }

    #[test]
    fn up_down_walk_the_sections_and_enter_goes_into_the_rows() {
        let mut a = app();
        press(&mut a, KeyCode::F(2));
        press(&mut a, KeyCode::Down);
        assert_eq!(a.settings_page().unwrap().section, Section::Layout);
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Up);
        assert_eq!(a.settings_page().unwrap().section, Section::About, "wraps");
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        assert!(a.settings_page().unwrap().in_rows);
        assert_eq!(row_id(&a), RowId::ThemeName);
        press(&mut a, KeyCode::Tab);
        assert!(!a.settings_page().unwrap().in_rows);
    }

    #[test]
    fn a_section_with_nothing_to_change_stays_on_the_section_list() {
        let mut a = app();
        a.open_settings(Section::About);
        press(&mut a, KeyCode::Enter);
        assert!(!a.settings_page().unwrap().in_rows);
    }

    #[test]
    fn density_applies_at_once_and_queues_one_edit() {
        let mut a = in_section(Section::Layout);
        goto(&mut a, RowId::Density);
        assert_eq!(a.row_h(), 2);
        let out = press(&mut a, KeyCode::Right);
        assert!(out.is_empty(), "layout needs no backend");
        assert_eq!(a.row_h(), 1);
        assert_eq!(a.settings.layout.density, Density::Compact);
        assert_eq!(
            a.take_setting_edits(),
            vec![SettingEdit::set(
                "layout",
                "density",
                SettingValue::str("compact")
            )]
        );
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn theme_change_recolours_and_no_colour_stays() {
        let mut a = in_section(Section::Appearance);
        assert_eq!(a.theme, Theme::default());
        press(&mut a, KeyCode::Right);
        press(&mut a, KeyCode::Right);
        assert_eq!(a.theme, Theme::builtin("light").unwrap());
        let mut b = app();
        b.no_colour = true;
        b.open_settings(Section::Appearance);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Right);
        press(&mut b, KeyCode::Right);
        assert_eq!(b.theme, Theme::builtin("light").unwrap().without_colour());
        // The page says why nothing changes colour; without NO_COLOR there is no such line.
        assert_eq!(b.page_rows()[0].id, RowId::Info("no_colour"));
        assert_ne!(a.page_rows()[0].id, RowId::Info("no_colour"));
    }

    #[test]
    fn caps_lock_ctrl_c_quits_on_the_settings_page_and_a_global_quit_key_cannot_hide_page_keys() {
        let mut a = in_section(Section::Appearance);
        let mut ev = key(KeyCode::Char('C'));
        ev.modifiers = KeyModifiers::CONTROL;
        a.handle_key(ev);
        assert!(a.should_quit());
        // Quit bound to Enter: Enter still works as the page's key (it edits), no quit.
        let mut a = in_section(Section::Layout);
        let mut user = chatcli_core::settings::KeySettings::default();
        user.bindings
            .entry("global".to_string())
            .or_default()
            .insert("enter".to_string(), "quit".to_string());
        a.keys = Keymap::build(Preset::Default, &user).0;
        press(&mut a, KeyCode::Enter);
        assert!(!a.should_quit());
    }

    #[test]
    fn a_colour_override_follows_the_row_and_reset_drops_it() {
        let mut a = in_section(Section::Appearance);
        goto(&mut a, RowId::Role("accent"));
        press(&mut a, KeyCode::Right);
        assert_eq!(a.settings.theme.overrides["accent"], "black");
        assert_eq!(a.theme.accent.fg, Some(ratatui::style::Color::Black));
        press(&mut a, KeyCode::Char('r'));
        assert!(a.settings.theme.overrides.is_empty());
        assert_eq!(a.theme.accent, Theme::default().accent);
        let edits = a.take_setting_edits();
        assert_eq!(edits.last(), Some(&SettingEdit::remove("theme", "accent")));
    }

    #[test]
    fn whatsapp_typing_goes_to_both_backends_and_updates_the_ui() {
        let mut a = in_section(Section::WhatsApp);
        goto(&mut a, RowId::SendTyping);
        assert!(!a.send_typing);
        let out = press(&mut a, KeyCode::Char(' '));
        assert!(a.send_typing);
        assert_eq!(out.len(), 2);
        for (n, cmd) in &out {
            match cmd {
                Command::Settings(s) => assert!(s.whatsapp.send_typing, "{n:?}"),
                other => panic!("{other:?}"),
            }
        }
        let nets: Vec<Network> = out.iter().map(|(n, _)| *n).collect();
        assert_eq!(nets, Network::ALL.to_vec());
    }

    #[test]
    fn media_limits_apply_to_the_app_and_reach_the_backends() {
        let mut a = in_section(Section::Media);
        assert_eq!(a.media.max_file_bytes, 25 * 1024 * 1024);
        press(&mut a, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut a, KeyCode::Backspace);
        }
        type_text(&mut a, "300");
        let out = press(&mut a, KeyCode::Enter);
        assert_eq!(out.len(), 2);
        assert!(a.settings_page().unwrap().edit.is_none());
        goto(&mut a, RowId::MaxFileMb);
        press(&mut a, KeyCode::Right);
        assert_eq!(a.media.max_file_bytes, 30 * 1024 * 1024);
        goto(&mut a, RowId::InlineImages);
        assert!(a.media.inline);
        press(&mut a, KeyCode::Right);
        assert!(!a.media.inline);
    }

    #[test]
    fn a_theme_change_alone_sends_nothing_to_the_backends() {
        let mut a = in_section(Section::Appearance);
        assert!(press(&mut a, KeyCode::Right).is_empty());
    }

    #[test]
    fn a_range_error_stays_in_the_edit_and_typing_clears_it() {
        let mut a = in_section(Section::Media);
        press(&mut a, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut a, KeyCode::Backspace);
        }
        type_text(&mut a, "0");
        let out = press(&mut a, KeyCode::Enter);
        assert!(out.is_empty());
        let p = a.settings_page().unwrap();
        assert_eq!(p.error.as_deref(), Some(settings_view::MSG_CACHE));
        assert!(p.edit.is_some(), "still editing");
        assert!(a.take_setting_edits().is_empty());
        assert_eq!(a.settings.media.cache_max_mb, 200);
        type_text(&mut a, "1");
        assert!(a.settings_page().unwrap().error.is_none());
        press(&mut a, KeyCode::Esc);
        let p = a.settings_page().unwrap();
        assert!(p.edit.is_none() && p.error.is_none());
    }

    #[test]
    fn a_paste_goes_into_a_value_being_typed_and_nowhere_else() {
        let mut a = in_section(Section::Appearance);
        goto(&mut a, RowId::Role("dim"));
        a.handle_paste("#abc");
        assert!(a.settings_page().unwrap().edit.is_none());
        press(&mut a, KeyCode::Enter);
        a.handle_paste("#12ab3F\n");
        assert_eq!(
            a.settings_page().unwrap().edit,
            Some(PageEdit::Text("#12ab3F".into()))
        );
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.settings.theme.overrides["dim"], "#12ab3F");
    }

    #[test]
    fn a_failed_save_is_one_red_line_and_an_inline_error() {
        let mut a = in_section(Section::Layout);
        a.settings_save_failed("NOT SAVED - settings.toml line 3 has a syntax error");
        let n = a.current_notice().unwrap();
        assert!(n.is_error && n.text.starts_with("NOT SAVED"));
        let inline = a.settings_page().unwrap().error.as_deref().unwrap();
        assert!(inline.starts_with("NOT SAVED"));
    }

    #[test]
    fn problems_show_as_a_header_and_in_about() {
        let mut a = in_section(Section::Layout);
        assert_ne!(a.page_rows()[0].id, RowId::Info("problems"));
        a.settings_saved(vec!["one".into(), "two".into()]);
        let rows = a.page_rows();
        assert_eq!(rows[0].value, "SETTINGS FILE PROBLEMS: 2 - see About");
        a.open_settings(Section::About);
        assert!(a.page_rows().iter().any(|r| r.value == "two"));
    }

    #[test]
    fn a_problems_header_arriving_keeps_the_cursor_on_its_row() {
        let mut a = in_section(Section::Layout);
        goto(&mut a, RowId::Density);
        a.settings_saved(vec!["one".into()]);
        assert_eq!(row_id(&a), RowId::Density);
        a.settings_saved(Vec::new());
        assert_eq!(row_id(&a), RowId::Density);
    }

    #[test]
    fn theme_command_applies_and_saves_a_named_theme() {
        let mut a = app();
        a.cmd_theme(Some("light"));
        assert_eq!(a.settings.theme.name, "light");
        assert_eq!(a.theme, Theme::builtin("light").unwrap());
        assert_eq!(a.take_setting_edits().len(), 1);
        a.cmd_theme(Some("neon"));
        assert_eq!(a.settings.theme.name, "light");
        assert_eq!(
            a.settings_page().unwrap().error.as_deref(),
            Some("unknown theme: use default, high-contrast, light, dark")
        );
        assert!(a.take_setting_edits().is_empty());
        assert!(a.settings_page().is_some());
    }

    #[test]
    fn ctrl_c_quits_from_the_page_and_mid_edit() {
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        let mut a = in_section(Section::Media);
        press(&mut a, KeyCode::Enter);
        a.handle_key(ctrl_c);
        assert!(a.should_quit());
    }

    #[test]
    fn about_lines_name_the_settings_file_and_mark_the_demo_copy() {
        use chatcli_core::media::MediaCache;
        use chatcli_core::store::Store;
        let dir = std::env::temp_dir().join(format!("chatcli-about-test-{}", std::process::id()));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let mut ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
        ctx.settings_path = Some(dir.join("settings.toml"));
        let real = about_lines(&ctx);
        assert_eq!(real[0].0, "Version");
        assert!(real[0].1.starts_with("chatcli "));
        assert!(real[3].1.ends_with("settings.toml"), "{:?}", real[3]);
        ctx.demo = true;
        assert!(about_lines(&ctx)[3]
            .1
            .ends_with("(demo copy, deleted on exit)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn press_mod(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Out {
        app.handle_key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn keys_row(ctx: Context, a: Action) -> RowId {
        RowId::KeyAction(ctx, a)
    }

    fn plain(c: KeyCode) -> KeyChord {
        KeyChord::new(c, KeyModifiers::NONE)
    }

    /// The Keys page with the cursor on `ctx`'s `action` row.
    fn on_key_row(ctx: Context, action: Action) -> App {
        let mut a = in_section(Section::Keys);
        goto(&mut a, keys_row(ctx, action));
        a
    }

    fn value_of(a: &App, id: RowId) -> String {
        a.page_rows()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
            .value
    }

    fn capture_text(a: &App) -> Option<String> {
        crate::settings_view::capture_line(a.settings_page().unwrap().edit.as_ref())
    }

    fn kset(table: &str, key: &str, v: &str) -> SettingEdit {
        SettingEdit::set(table, key, SettingValue::str(v))
    }

    #[test]
    fn the_keys_page_lists_actions_by_context_with_their_keys() {
        let mut a = in_section(Section::Keys);
        assert_eq!(row_id(&a), RowId::KeyPreset);
        let rows = a.page_rows();
        assert_eq!(value_of(&a, RowId::KeyPreset), "default");
        assert!(rows.iter().any(|r| r.id == RowId::KeyHeading(Context::List)
            && r.label == "Chat list"
            && !r.selectable()));
        assert_eq!(value_of(&a, keys_row(Context::List, Action::Refresh)), "r");
        assert_eq!(
            value_of(&a, keys_row(Context::Global, Action::TogglePreset)),
            "-",
            "an action with no key still has a row"
        );
        assert_eq!(rows.last().unwrap().id, RowId::ResetAllKeys);
        let doc = rows
            .iter()
            .find(|r| r.id == keys_row(Context::List, Action::Refresh))
            .unwrap()
            .hint;
        assert_eq!(doc, Action::Refresh.doc());
        // Left and Right on a key row say what to do instead.
        goto(&mut a, keys_row(Context::List, Action::Refresh));
        press(&mut a, KeyCode::Right);
        assert_eq!(
            a.settings_page().unwrap().error.as_deref(),
            Some("Enter replaces a key, a adds one, d deletes one")
        );
    }

    #[test]
    fn capturing_f5_for_refresh_rebinds_it_at_once_and_queues_the_edits() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        assert_eq!(
            capture_text(&a).as_deref(),
            Some("Press the new key for Refresh (Esc cancels, Backspace removes)")
        );
        let out = press(&mut a, KeyCode::F(5));
        assert!(out.is_empty(), "keys need no backend");
        assert!(a.settings_page().unwrap().edit.is_none());
        assert_eq!(
            a.keys.lookup(Context::List, plain(KeyCode::F(5))),
            Some(Action::Refresh)
        );
        assert_eq!(
            a.keys.lookup(Context::List, plain(KeyCode::Char('r'))),
            None
        );
        assert_eq!(
            a.take_setting_edits(),
            vec![
                kset("keys.list", "f5", "refresh"),
                kset("keys.list", "r", "unbound")
            ]
        );
        assert_eq!(
            a.keys.hint(Context::List, &[Action::Refresh]).as_deref(),
            Some("F5")
        );
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "F5 *"
        );
        assert!(a.current_row().unwrap().custom);
    }

    #[test]
    fn a_adds_a_key_and_keeps_the_old_one() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(
            capture_text(&a).as_deref(),
            Some("Press a key to add to Refresh (Esc cancels)")
        );
        press(&mut a, KeyCode::F(5));
        assert!(a.settings_page().unwrap().edit.is_none());
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "F5, r *"
        );
        assert_eq!(
            a.take_setting_edits(),
            vec![kset("keys.list", "f5", "refresh")]
        );
        // Esc leaves an add without a change.
        press(&mut a, KeyCode::Char('a'));
        press(&mut a, KeyCode::Esc);
        assert!(a.settings_page().unwrap().edit.is_none());
        assert!(
            a.settings_page().is_some(),
            "Esc cancels the add, not the page"
        );
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn adding_a_key_of_another_action_is_refused_and_keeps_listening() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Char('a'));
        press(&mut a, KeyCode::Char('p'));
        let p = a.settings_page().unwrap();
        assert_eq!(
            p.error.as_deref(),
            Some("p is already Pin (Chat list): delete it there first")
        );
        assert!(
            matches!(p.edit, Some(PageEdit::Add { .. })),
            "still listening"
        );
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn an_overlapping_key_is_added_only_after_enter() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Char('a'));
        press_mod(&mut a, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(
            capture_text(&a).as_deref(),
            Some(
                "Ctrl+F is Search everywhere: in Chat list it will be Refresh instead. \
                 Enter adds it anyway, Esc cancels"
            )
        );
        assert!(a.take_setting_edits().is_empty(), "nothing until Enter");
        press(&mut a, KeyCode::Esc);
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Char('a'));
        press_mod(&mut a, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut a, KeyCode::Enter);
        assert_eq!(
            a.take_setting_edits(),
            vec![kset("keys.list", "ctrl+f", "refresh")]
        );
        assert_eq!(
            a.keys.lookup(
                Context::List,
                KeyChord::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            ),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn d_chooses_a_key_with_left_and_right_and_deletes_it() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        for k in [KeyCode::F(5), KeyCode::F(6)] {
            press(&mut a, KeyCode::Char('a'));
            press(&mut a, k);
        }
        a.take_setting_edits();
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "F5, F6, r *"
        );
        press(&mut a, KeyCode::Char('d'));
        let pick = |a: &App| match a.settings_page().unwrap().edit {
            Some(PageEdit::Pick { index, .. }) => index,
            ref other => panic!("not picking: {other:?}"),
        };
        assert_eq!(pick(&a), 0);
        press(&mut a, KeyCode::Right);
        assert_eq!(pick(&a), 1);
        press(&mut a, KeyCode::Char('l'));
        assert_eq!(pick(&a), 2);
        press(&mut a, KeyCode::Right);
        assert_eq!(pick(&a), 0, "wraps");
        press(&mut a, KeyCode::Left);
        assert_eq!(pick(&a), 2, "wraps back");
        press(&mut a, KeyCode::Char('h'));
        assert_eq!(pick(&a), 1);
        press(&mut a, KeyCode::Enter);
        assert!(a.settings_page().unwrap().edit.is_none());
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "F5, r *"
        );
        assert_eq!(
            a.take_setting_edits(),
            vec![SettingEdit::remove("keys.list", "f6")]
        );
        // d deletes too, and Esc leaves without a change.
        press(&mut a, KeyCode::Char('d'));
        press(&mut a, KeyCode::Esc);
        assert!(a.settings_page().unwrap().edit.is_none());
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Char('d'));
        press(&mut a, KeyCode::Right);
        press(&mut a, KeyCode::Char('d'));
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "F5 *"
        );
        assert_eq!(
            a.take_setting_edits(),
            vec![kset("keys.list", "r", "unbound")]
        );
    }

    #[test]
    fn a_locked_key_cannot_be_picked_away_and_a_keyless_action_says_so() {
        let mut a = on_key_row(Context::Global, Action::Quit);
        assert_eq!(
            value_of(&a, keys_row(Context::Global, Action::Quit)),
            "Ctrl+C"
        );
        press(&mut a, KeyCode::Char('d'));
        press(&mut a, KeyCode::Enter);
        let p = a.settings_page().unwrap();
        assert_eq!(p.error.as_deref(), Some("Ctrl+C always quits"));
        assert!(
            matches!(p.edit, Some(PageEdit::Pick { .. })),
            "still choosing"
        );
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Esc);
        goto(&mut a, keys_row(Context::Global, Action::TogglePreset));
        press(&mut a, KeyCode::Char('d'));
        let p = a.settings_page().unwrap();
        assert_eq!(
            p.error.as_deref(),
            Some("Toggle preset has no keys to delete")
        );
        assert!(p.edit.is_none());
    }

    #[test]
    fn ctrl_c_quits_while_adding_or_picking() {
        for start in ['a', 'd'] {
            let mut a = on_key_row(Context::List, Action::Refresh);
            press(&mut a, KeyCode::Char(start));
            press_mod(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
            assert!(a.quit, "{start}");
            assert!(a.take_setting_edits().is_empty());
        }
    }

    #[test]
    fn r_resets_one_action_to_the_preset() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::F(5));
        a.take_setting_edits();
        press(&mut a, KeyCode::Char('r'));
        assert_eq!(value_of(&a, keys_row(Context::List, Action::Refresh)), "r");
        assert!(a.settings.keys.bindings.is_empty());
        assert_eq!(
            a.take_setting_edits(),
            vec![
                SettingEdit::remove("keys.list", "f5"),
                SettingEdit::remove("keys.list", "r"),
            ]
        );
    }

    #[test]
    fn a_text_key_is_refused_where_you_type_and_esc_is_never_captured() {
        let mut a = on_key_row(Context::Composer, Action::Send);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('x'));
        let p = a.settings_page().unwrap();
        assert_eq!(
            p.error.as_deref(),
            Some("x types text here: use a Ctrl or Alt key")
        );
        assert!(
            matches!(p.edit, Some(PageEdit::Capture { .. })),
            "still listening"
        );
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Esc);
        assert!(
            a.settings_page().is_some(),
            "Esc cancels the capture, not the page"
        );
        assert!(a.settings_page().unwrap().edit.is_none());
        assert!(a.take_setting_edits().is_empty());
        assert_eq!(
            a.keys.lookup(Context::Composer, plain(KeyCode::Esc)),
            Some(Action::LeaveComposer)
        );
    }

    #[test]
    fn backspace_removes_the_first_key_and_a_locked_one_is_refused() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Backspace);
        assert!(a.settings_page().unwrap().edit.is_none());
        assert_eq!(
            a.take_setting_edits(),
            vec![kset("keys.list", "r", "unbound")]
        );
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "- *"
        );
        let mut b = on_key_row(Context::Help, Action::CloseHelp);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Backspace);
        let p = b.settings_page().unwrap();
        assert_eq!(p.error.as_deref(), Some("Esc always backs out here"));
        assert!(p.edit.is_some());
        assert!(b.take_setting_edits().is_empty());
        let mut c = on_key_row(Context::Global, Action::Quit);
        press(&mut c, KeyCode::Enter);
        press(&mut c, KeyCode::Backspace);
        assert_eq!(
            c.settings_page().unwrap().error.as_deref(),
            Some("Ctrl+C always quits")
        );
    }

    #[test]
    fn ctrl_c_during_a_capture_quits() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press_mod(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.should_quit());
        assert!(a.take_setting_edits().is_empty());
    }

    #[test]
    fn a_key_that_does_something_else_asks_before_swapping() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('p'));
        assert_eq!(
            capture_text(&a).as_deref(),
            Some("p is already Pin. Enter: Pin gets Refresh's old key (or none), Esc cancels")
        );
        // Other keys are ignored while the question is open.
        press(&mut a, KeyCode::Char('z'));
        assert!(capture_text(&a).is_some());
        press(&mut a, KeyCode::Esc);
        assert!(a.settings_page().unwrap().edit.is_none());
        assert!(a.take_setting_edits().is_empty());
        assert_eq!(
            a.keys.lookup(Context::List, plain(KeyCode::Char('p'))),
            Some(Action::Pin)
        );
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('p'));
        press(&mut a, KeyCode::Enter);
        let at = |c: char| a.keys.lookup(Context::List, plain(KeyCode::Char(c)));
        assert_eq!(at('p'), Some(Action::Refresh));
        assert_eq!(at('r'), Some(Action::Pin));
        assert!(a.settings_page().unwrap().edit.is_none());
        assert_eq!(a.take_setting_edits().len(), 2);
    }

    #[test]
    fn a_user_entry_written_in_capitals_is_replaced_not_duplicated() {
        let mut s = Settings::default();
        s.keys
            .bindings
            .entry("list".into())
            .or_default()
            .insert("Ctrl+N".into(), "refresh".into());
        let mut a = App::with_settings(
            Clock {
                now_ms: 1_800_000_000_000,
                tz: chrono::FixedOffset::east_opt(0),
            },
            &s,
            None,
        );
        a.open_settings(Section::Keys);
        press(&mut a, KeyCode::Enter);
        goto(&mut a, keys_row(Context::List, Action::Refresh));
        assert_eq!(
            value_of(&a, keys_row(Context::List, Action::Refresh)),
            "Ctrl+N, r *"
        );
        press(&mut a, KeyCode::Enter);
        press_mod(&mut a, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(
            a.settings_page().unwrap().edit.is_none(),
            "same key: nothing to do"
        );
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::F(6));
        assert_eq!(
            a.take_setting_edits(),
            vec![
                SettingEdit::remove("keys.list", "Ctrl+N"),
                kset("keys.list", "f6", "refresh")
            ]
        );
        assert_eq!(a.settings.keys.bindings["list"].len(), 1);
    }

    #[test]
    fn reset_all_asks_and_y_removes_every_binding_n_keeps_them() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::F(5));
        goto(&mut a, keys_row(Context::Select, Action::Reply));
        press(&mut a, KeyCode::Enter);
        press_mod(&mut a, KeyCode::Char('t'), KeyModifiers::CONTROL);
        a.take_setting_edits();
        goto(&mut a, RowId::ResetAllKeys);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.prompt(), Some(&Prompt::ResetKeys));
        press(&mut a, KeyCode::Char('n'));
        assert!(a.prompt().is_none());
        assert_eq!(a.settings.keys.bindings.len(), 2, "n keeps them");
        assert!(a.take_setting_edits().is_empty());
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y'));
        assert!(a.prompt().is_none());
        assert!(a.settings.keys.bindings.is_empty());
        assert_eq!(a.take_setting_edits().len(), 4);
        assert_eq!(value_of(&a, keys_row(Context::List, Action::Refresh)), "r");
        assert!(a.settings_page().is_some(), "still on the page");
    }

    #[test]
    fn the_preset_row_switches_the_keys_and_saves_the_preset() {
        use crate::keymap::{Mode, Preset};
        let mut a = in_section(Section::Keys);
        let out = press(&mut a, KeyCode::Right);
        assert!(out.is_empty());
        assert_eq!(a.keys.preset(), Preset::Vim);
        assert_eq!(a.settings.keys.preset, "vim");
        assert_eq!(a.take_setting_edits(), vec![kset("keys", "preset", "vim")]);
        assert_eq!(row_id(&a), RowId::KeyPreset, "the cursor stays on the row");
        assert_eq!(a.mode(), Some(Mode::Normal), "-- NORMAL -- shows");
        assert!(a
            .page_rows()
            .iter()
            .any(|r| r.id == RowId::KeyHeading(Context::Normal)));
        press(&mut a, KeyCode::Char('r'));
        assert_eq!(a.keys.preset(), Preset::Default);
        assert_eq!(
            a.take_setting_edits(),
            vec![SettingEdit::remove("keys", "preset")]
        );
    }

    #[test]
    fn a_key_edit_under_a_broken_file_still_applies_for_the_session() {
        let mut a = on_key_row(Context::List, Action::Refresh);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::F(5));
        a.settings_save_failed("NOT SAVED - settings.toml line 3 has a syntax error");
        assert_eq!(
            a.keys.lookup(Context::List, plain(KeyCode::F(5))),
            Some(Action::Refresh)
        );
        assert!(a.current_notice().unwrap().is_error);
    }

    #[test]
    fn slash_keys_opens_the_keys_page_ready_for_its_rows() {
        let mut a = app();
        press(&mut a, KeyCode::Char('/'));
        type_text(&mut a, "keys");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.settings_page().unwrap().section, Section::Keys);
        press(&mut a, KeyCode::Enter);
        assert_eq!(row_id(&a), RowId::KeyPreset);
    }
}
