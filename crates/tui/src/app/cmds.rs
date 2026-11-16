//! Commands: the `/` command bar, `/command` in the message box and `:command` in the vim
//! box all end in `run_cmd`. `App` stays pure: a command that needs a backend returns an `Out`.

use chatcli_core::model::{Command, Status};

use super::{App, Focus, Out, Prompt};
use crate::commands::{self, Cmd, CmdId, COMMANDS_TITLE};
use crate::keymap::{Action, Preset};
use crate::palette::{PalItem, Palette};
use crate::popup::{Popup, PopupItem};
use crate::settings_view::{Section, SettingsPage};
use crate::ui;

impl App {
    pub(crate) fn palette(&self) -> Option<&Palette> {
        self.palette.as_ref()
    }

    pub(crate) fn settings_page(&self) -> Option<&SettingsPage> {
        self.settings_page.as_ref()
    }

    pub(crate) fn demo(&self) -> bool {
        self.demo
    }

    /// Why a command cannot run right now, as the notice text. `None` = it can.
    pub(crate) fn cmd_unavailable(&self, id: CmdId) -> Option<String> {
        let ns = self.active();
        let label = ns.network.label();
        match id {
            CmdId::Refresh if ns.panel() => Some("REFRESH - sign in first".to_string()),
            CmdId::SignIn => match ns.status {
                Status::NeedsLogin { .. } => None,
                Status::LoggingIn | Status::Qr { .. } => {
                    Some(format!("Sign-in to {label} is already running"))
                }
                Status::Online { .. } => Some(format!("Already signed in to {label}")),
                _ => Some(format!("{label} is not ready to sign in")),
            },
            CmdId::SignOut if matches!(ns.status, Status::NeedsLogin { .. }) => {
                Some(format!("Not signed in to {label}"))
            }
            CmdId::Tour if !self.demo => Some("TOUR - run chatcli --demo --tour".to_string()),
            _ => None,
        }
    }

    /// Why `run_cmd` would not run: (notice, is it an error). None when it runs.
    pub(crate) fn cmd_refusal(&self, id: CmdId, arg: Option<&str>) -> Option<(String, bool)> {
        if let Some(reason) = self.cmd_unavailable(id) {
            return Some((reason, false));
        }
        let takes_arg = matches!(id, CmdId::Keys | CmdId::Theme);
        if arg.is_some() && !takes_arg {
            let name = commands::COMMANDS
                .iter()
                .find(|c| c.id == id)
                .map_or("", |c| c.name);
            return Some((format!("/{name} takes no arguments"), true));
        }
        None
    }

    /// Runs one command for the active network. `arg` is what followed the name, if anything.
    pub(crate) fn run_cmd(&mut self, id: CmdId, arg: Option<&str>) -> Out {
        let n = self.active;
        if let Some((text, is_error)) = self.cmd_refusal(id, arg) {
            self.push_global_notice(&text, is_error);
            return Vec::new();
        }
        match id {
            CmdId::Help => self.help = Some(0),
            CmdId::Settings => self.open_settings(Section::Appearance),
            CmdId::Theme => return self.cmd_theme(arg),
            CmdId::Keys => match arg {
                None => self.open_settings(Section::Keys),
                Some(name) => match Preset::from_name(name) {
                    Some(p) => self.set_preset(p),
                    None => self.push_global_notice(
                        &format!("Unknown key preset \"{name}\": use default or vim."),
                        true,
                    ),
                },
            },
            CmdId::Layout => self.open_settings(Section::Layout),
            CmdId::Notifications => self.open_settings(Section::Notifications),
            CmdId::About => self.open_settings(Section::About),
            CmdId::Search => self.open_search(),
            CmdId::Refresh => return self.refresh(n),
            CmdId::SignIn => return vec![(n, Command::Login)],
            CmdId::SignOut => self.prompt = Some(Prompt::SignOut(n)),
            CmdId::Tour => self.start_tour(),
            CmdId::Quit => self.quit = true,
        }
        Vec::new()
    }

    // ---- the command bar ----

    pub(super) fn open_palette(&mut self) {
        self.popup = None;
        self.palette = Some(Palette::default());
    }

    /// The bar's items: matching commands, then (once something is typed) chats of the active
    /// network whose title contains the text. Not in the sign-in panel: it has no chats.
    pub(crate) fn palette_items(&self) -> Vec<PalItem> {
        let Some(p) = &self.palette else {
            return Vec::new();
        };
        let query = p.query.text().trim_start_matches('/').trim();
        let word = query.split_whitespace().next().unwrap_or("");
        let mut items: Vec<PalItem> = commands::matching(word)
            .into_iter()
            .map(|c| PalItem::Cmd(c.id))
            .collect();
        let ns = self.active();
        if !query.is_empty() && !ns.panel() {
            items.extend(
                ns.chats
                    .iter()
                    .filter(|c| ns.show_hidden || !c.flags.hidden)
                    .filter(|c| crate::list::matches(&c.title, query))
                    .map(|c| PalItem::Chat(c.id.clone())),
            );
        }
        items
    }

    /// Rows of the bar that fit on screen (0 when it is too small to draw).
    fn palette_rows_shown(&self) -> usize {
        ui::palette_geometry(self.layout_in(self.size).body).map_or(1, |g| g.rows)
    }

    /// Puts the window back on the selection after the items or the selection changed.
    pub(super) fn palette_fit(&mut self) {
        let items = self.palette_items();
        let shown = self.palette_rows_shown();
        if let Some(p) = self.palette.as_mut() {
            p.sel = p.sel.min(items.len().saturating_sub(1));
            p.keep_visible(&items, shown);
        }
    }

    pub(super) fn palette_type(&mut self, text: &str) {
        if let Some(p) = self.palette.as_mut() {
            p.query.insert_str(text);
            p.sel = 0;
            p.top = 0;
        }
    }

    pub(super) fn palette_backspace(&mut self) {
        if let Some(p) = self.palette.as_mut() {
            p.query.delete_back();
            p.sel = 0;
            p.top = 0;
        }
    }

    /// Page keys: about a screenful, clamped.
    pub(super) fn palette_page(&mut self, up: bool) {
        let len = self.palette_items().len();
        let step = (self.palette_rows_shown() as isize - 1).max(1);
        if let Some(p) = self.palette.as_mut() {
            p.page(len, if up { -step } else { step });
        }
        self.palette_fit();
    }

    pub(super) fn palette_action(&mut self, action: Action) -> Out {
        let items = self.palette_items();
        match action {
            Action::PopupUp | Action::PopupDown => {
                let delta = if action == Action::PopupUp { -1 } else { 1 };
                if let Some(p) = self.palette.as_mut() {
                    p.step(items.len(), delta);
                }
                self.palette_fit();
            }
            Action::PopupAccept => {
                let Some(p) = self.palette.as_ref() else {
                    return Vec::new();
                };
                let Some(item) = items.get(p.sel).cloned() else {
                    // Nothing matches: the bar stays open.
                    return Vec::new();
                };
                let rest = p.query.text().trim_start_matches('/').trim();
                let arg = rest
                    .split_once(char::is_whitespace)
                    .map(|(_, a)| a.trim().to_string())
                    .filter(|a| !a.is_empty());
                self.palette = None;
                return match item {
                    PalItem::Cmd(id) => self.run_cmd(id, arg.as_deref()),
                    PalItem::Chat(id) => self.open_chat_by_id(&id),
                };
            }
            Action::PopupClose => self.palette = None,
            _ => {}
        }
        Vec::new()
    }

    /// Enter on a chat in the bar: select it and open it like Enter in the list.
    fn open_chat_by_id(&mut self, id: &str) -> Out {
        let n = self.active;
        let ns = self.ns_mut(n);
        ns.filter.clear();
        ns.focus = Focus::List;
        ns.selected = Some(id.to_string());
        self.fix_selection(n, true);
        self.open_selected(n)
    }

    // ---- `/` in the message box ----

    /// The Commands popup above the message box while the draft starts with `/word`.
    pub(super) fn refresh_command_popup(&mut self, query: &str) -> Out {
        if self
            .popup
            .as_ref()
            .is_some_and(|p| p.title == COMMANDS_TITLE && p.query == query)
        {
            return Vec::new();
        }
        let items: Vec<PopupItem> = commands::matching(query)
            .into_iter()
            .map(|c: &Cmd| PopupItem {
                // Padded so the docs line up in the list.
                label: format!("{:<14}", format!("/{}", c.name)),
                detail: c.doc.to_string(),
                insert: format!("/{}", c.name),
                person: None,
            })
            .collect();
        self.popup = if items.is_empty() {
            None
        } else {
            // The slash plus what was typed after it.
            let replace = 1 + query.chars().count();
            Some(Popup::new(items, query.to_string(), replace).titled(COMMANDS_TITLE))
        };
        Vec::new()
    }

    /// Enter on a command in the popup: the draft is a command, not a message, so it goes.
    pub(super) fn run_chosen_command(
        &mut self,
        n: chatcli_core::model::Network,
        chosen: Option<String>,
    ) -> Out {
        if let Some(id) = self.ns(n).open.clone() {
            self.ns_mut(n).drafts.remove(&id);
        }
        match chosen.as_deref().and_then(commands::find) {
            Some(c) => self.run_cmd(c.id, None),
            None => Vec::new(),
        }
    }
}
