//! Key bindings: what a key does (`Action`) is separate from which key it is (`KeyChord`).
//!
//! The keymap maps `(Context, KeyChord)` to an `Action`. chatcli has one set of built-in keys,
//! and `[keys]` in `settings.toml` layers the user's bindings over them. `Keymap::build` validates the user's entries and
//! reports every problem; a bad binding is skipped and the rest still load.
//!
//! Typed characters, Backspace and the mouse are not actions: the app handles them itself in
//! the contexts that take text (`Context::types_text`).
//!
//! Terminals: Windows consoles report Shift and Alt combinations differently from Unix ones.
//! A chord is normalised (see `KeyChord::new`) so `G`, `shift+g` and Shift+G arriving as
//! `Char('g')` + SHIFT all compare equal.

use chatcli_core::settings::{KeySettings, Settings, SettingsProblem, Severity};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Where a key is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    /// `[keys.common]`: a key bound here works in every context its action applies to (see
    /// `common_applies`), unless that context binds the key itself.
    Common,
    /// The sign-in panel that replaces the chat view.
    Panel,
    /// Chat list focus.
    List,
    /// The chat filter box.
    Filter,
    /// The message box.
    Composer,
    /// Message selection: one message in the open chat is highlighted and keys act on it.
    Select,
    /// The emoji picker, the file path prompt (Ctrl+O), the search box (Ctrl+F) and the command
    /// bar. (The `:shortcode` list above the message box reads only the four `popup_*` actions
    /// of this table, so every other key still edits the draft.)
    Popup,
    /// The "Open <host>? y/n" line for a link, "Delete for everyone? y/n", or the y/n before a
    /// file is sent.
    Confirm,
    /// The help overlay.
    Help,
}

impl Context {
    /// Every context, in help and keys-page order.
    pub const ALL: [Context; 9] = [
        Context::Common,
        Context::Panel,
        Context::List,
        Context::Filter,
        Context::Composer,
        Context::Select,
        Context::Popup,
        Context::Confirm,
        Context::Help,
    ];

    /// The name used for the `[keys.<name>]` table.
    pub fn name(self) -> &'static str {
        match self {
            Context::Common => "common",
            Context::Panel => "panel",
            Context::List => "list",
            Context::Filter => "filter",
            Context::Composer => "composer",
            Context::Select => "select",
            Context::Popup => "popup",
            Context::Confirm => "confirm",
            Context::Help => "help",
        }
    }

    pub fn from_name(s: &str) -> Option<Context> {
        Context::ALL.into_iter().find(|c| c.name() == s)
    }

    /// Heading in the help overlay.
    pub fn title(self) -> &'static str {
        match self {
            Context::Common => "Everywhere",
            Context::Panel => "Sign-in panel",
            Context::List => "Chat list",
            Context::Filter => "Filter box",
            Context::Composer => "Message box",
            Context::Select => "Message selection",
            Context::Popup => "Suggestions, pickers and search",
            Context::Confirm => "Confirm (link, delete, mark read)",
            Context::Help => "This help",
        }
    }

    /// A box you type in: plain characters and Backspace edit text here, so this context's own
    /// table refuses them as bindings, and a `[keys.common]` key that types a character is
    /// skipped here.
    pub fn types_text(self) -> bool {
        matches!(self, Context::Filter | Context::Composer | Context::Popup)
    }
}

macro_rules! actions {
    ($($variant:ident $name:literal $doc:literal,)*) => {
        /// What a key does. `name()` is the stable snake_case name used in `settings.toml`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Action {
            $(#[doc = $doc] $variant,)*
            /// Pseudo-action for `settings.toml`: remove the built-in binding of this key.
            /// Never stored in a keymap.
            Unbound,
        }

        impl Action {
            /// Every real action (not `Unbound`), in help order.
            pub const ALL: &'static [Action] = &[$(Action::$variant,)*];

            /// Stable name for `settings.toml` and the README.
            pub fn name(self) -> &'static str {
                match self {
                    $(Action::$variant => $name,)*
                    Action::Unbound => "unbound",
                }
            }

            /// One line for the help overlay.
            pub fn doc(self) -> &'static str {
                match self {
                    $(Action::$variant => $doc,)*
                    Action::Unbound => "remove the built-in binding of this key",
                }
            }
        }
    };
}

actions! {
    Quit "quit" "quit chatcli",
    NextNetwork "next_network" "switch to the next network tab",
    PrevNetwork "prev_network" "switch to the previous network tab",
    GotoNetwork1 "goto_network_1" "jump to the first network tab",
    GotoNetwork2 "goto_network_2" "jump to the second network tab",
    Help "help" "show this help",
    Up "up" "up one: the chat, the selected message, a list or the emoji grid; scrolls help",
    Down "down" "down one: the chat, the selected message, a list or the emoji grid; scrolls help",
    Top "top" "to the first chat, the oldest message held, or the top of help",
    Bottom "bottom" "to the last chat, the newest message, or the end of help",
    ScreenTop "screen_top" "to the first chat or message on screen; a page up in help",
    ScreenMiddle "screen_middle" "to the chat or message in the middle of the screen",
    ScreenBottom "screen_bottom" "to the last chat or message on screen; a page down in help",
    PageUp "page_up" "a page up: the chat list, the messages (from the message box), the selected message, a list, or help",
    PageDown "page_down" "a page down: the chat list, the messages (from the message box), the selected message, a list, or help",
    Accept "accept" "open the chosen chat and select its newest message, sign in, keep the filter, or take the chosen suggestion, command or emoji",
    Back "back" "back out: from the message box to selection, from selection to the chat list; close the open chat, help or a list; clear the filter; or answer no",
    Compose "compose" "go to the message box (from the chat list: of the chosen chat)",
    FilterStart "filter_start" "filter the chat list",
    Refresh "refresh" "ask the network for a fresh chat list",
    Send "send" "send the message",
    CursorLeft "cursor_left" "move the cursor one character left (emoji picker: one cell left)",
    CursorRight "cursor_right" "move the cursor one character right (emoji picker: one cell right)",
    WordLeft "word_left" "move the cursor to the start of the word before it",
    WordRight "word_right" "move the cursor to the start of the next word",
    LineStart "line_start" "move the cursor to the start of the line",
    LineEnd "line_end" "move the cursor to the end of the line",
    DeleteBack "delete_back" "delete the character before the cursor (a whole emoji at once)",
    DeleteForward "delete_forward" "delete the character under the cursor",
    DeleteWordBack "delete_word_back" "delete the word before the cursor",
    ClearLine "clear_line" "clear the line the cursor is on",
    InsertNewline "insert_newline" "add a line break instead of sending",
    EmojiPicker "emoji_picker" "open the emoji picker",
    OpenLink "open_link" "open a link from the selected message (chat list: from the newest message that has one)",
    ConfirmYes "confirm_yes" "yes: open the link, delete the message, send the file or mark all read",
    ConfirmNo "confirm_no" "no, do not open it, delete it or send it",
    JumpBack "jump_back" "back to where the last jump started (G, g, H, M, L, a hit, a quote)",
    JumpToQuoted "jump_to_quoted" "select the message the selected reply quotes (jump_back returns)",
    SelectMessages "select_messages" "select a message in the open chat",
    Reply "reply" "reply to the selected message (quote it)",
    CancelReply "cancel_reply" "drop the reply or edit in the message box (back does it too)",
    React "react" "react to the selected message with an emoji",
    Edit "edit" "edit the selected message of yours",
    Delete "delete" "delete the selected message of yours for everyone",
    OpenMedia "open_media" "open the selected message's attachment in the default app",
    Copy "copy" "copy the selected message's text (an attachment without a caption: its file name)",
    ViewImage "view_image" "show or hide a thumbnail of the selected image",
    Attach "attach" "choose a file to send: type or complete its path",
    PasteImage "paste_image" "attach the image on the clipboard",
    NextUnread "next_unread" "select the next chat with unread messages (wraps round)",
    PrevUnread "prev_unread" "select the previous chat with unread messages (wraps round)",
    MarkRead "mark_read" "mark the selected chat read",
    MarkAllRead "mark_all_read" "mark every chat of this network read, after a y/n",
    Pin "pin" "pin the selected chat to the top, or unpin it",
    Mute "mute" "mute the selected chat (no toasts, not counted), or unmute it",
    Hide "hide" "hide the selected chat from the list, or show it again",
    ShowHidden "show_hidden" "show or hide the chats you hid",
    Search "search" "search every message stored on this PC, both networks",
    SearchChat "search_chat" "search the messages of the open chat; Enter selects the one found",
    NextMatch "next_match" "select the next older message in this chat matching the last search",
    PrevMatch "prev_match" "select the next newer message in this chat matching the last search",
    CommandBar "command_bar" "open the command bar: commands and chats",
    Settings "settings" "open the settings screen",
}

impl Action {
    /// Plain words for the screens (`list_up` reads `List up`). `name()` stays the file's word.
    pub fn label(self) -> String {
        match self {
            Action::GotoNetwork1 => return "Jump to tab 1".to_string(),
            Action::GotoNetwork2 => return "Jump to tab 2".to_string(),
            _ => {}
        }
        let mut s = self.name().replace('_', " ");
        if let Some(first) = s.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        s
    }

    pub fn from_name(s: &str) -> Option<Action> {
        if s == "unbound" {
            return Some(Action::Unbound);
        }
        Action::ALL.iter().copied().find(|a| a.name() == s)
    }
}

/// One key press, normalised so equal keys compare equal on every terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// The default quit key, and the one that quits when the bindings leave a context without one.
pub(crate) const CTRL_C: KeyChord = KeyChord {
    code: KeyCode::Char('c'),
    mods: KeyModifiers::CONTROL,
};

/// The default back key, and the one that backs out when the bindings leave an overlay without
/// one.
pub(crate) const ESC: KeyChord = KeyChord {
    code: KeyCode::Esc,
    mods: KeyModifiers::NONE,
};

/// The character a key types, if it types one. Ctrl or Alt alone make a shortcut, not text.
/// Ctrl and Alt together are AltGr on Windows (it reports both): `@`, the euro sign, `{` and
/// the backslash on German, French, Spanish and Nordic layouts arrive that way, so they type,
/// unless the character is an ASCII letter (a real Ctrl+Alt chord). The keymap is asked first,
/// so a bound chord still wins.
pub fn typed_char(key: &KeyEvent) -> Option<char> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if c.is_control() {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match (ctrl, alt) {
        (false, false) => Some(c),
        (true, true) if !c.is_ascii_alphabetic() => Some(c),
        _ => None,
    }
}

impl KeyChord {
    /// Keeps Ctrl, Alt and Shift. A character already carries its Shift in its case (`G`, `?`),
    /// so Shift is folded into it; Shift+Tab is `BackTab`, which terminals send both ways.
    pub fn new(code: KeyCode, mods: KeyModifiers) -> KeyChord {
        let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let mut code = code;
        match code {
            // Caps Lock upper-cases the character on Windows but reports no Shift: Ctrl+C
            // arrives as Ctrl+'C'. A shortcut means the same key either way.
            KeyCode::Char(c)
                if c.is_ascii_uppercase()
                    && mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !mods.contains(KeyModifiers::SHIFT) =>
            {
                code = KeyCode::Char(c.to_ascii_lowercase());
            }
            KeyCode::Char(c) => {
                if mods.contains(KeyModifiers::SHIFT) {
                    let mut up = c.to_uppercase();
                    if let (Some(u), None) = (up.next(), up.next()) {
                        code = KeyCode::Char(u);
                    }
                }
                mods.remove(KeyModifiers::SHIFT);
            }
            KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
                code = KeyCode::BackTab;
                mods.remove(KeyModifiers::SHIFT);
            }
            KeyCode::BackTab => mods.remove(KeyModifiers::SHIFT),
            _ => {}
        }
        KeyChord { code, mods }
    }

    pub fn from_event(e: &KeyEvent) -> KeyChord {
        KeyChord::new(e.code, e.modifiers)
    }

    /// Text for the footer and help: `Ctrl+D`, `Esc`, `PgUp`, `G`.
    pub fn display(&self) -> String {
        let mut out = String::new();
        let mut mods = self.mods;
        // BackTab already says Shift.
        if self.code == KeyCode::BackTab {
            mods |= KeyModifiers::SHIFT;
        }
        for (flag, label) in [
            (KeyModifiers::CONTROL, "Ctrl+"),
            (KeyModifiers::ALT, "Alt+"),
            (KeyModifiers::SHIFT, "Shift+"),
        ] {
            if mods.contains(flag) {
                out.push_str(label);
            }
        }
        match self.code {
            KeyCode::Char(' ') => out.push_str("Space"),
            KeyCode::Char(c) if self.mods.contains(KeyModifiers::CONTROL) => {
                // A capital with Ctrl is Ctrl+Shift (the Shift was folded into the letter).
                if c.is_uppercase() {
                    out.push_str("Shift+");
                }
                out.extend(c.to_uppercase())
            }
            KeyCode::Char(c) => out.push(c),
            KeyCode::BackTab => out.push_str("Tab"),
            KeyCode::Enter => out.push_str("Enter"),
            KeyCode::Esc => out.push_str("Esc"),
            KeyCode::Tab => out.push_str("Tab"),
            KeyCode::Backspace => out.push_str("Backspace"),
            KeyCode::Delete => out.push_str("Delete"),
            KeyCode::Insert => out.push_str("Insert"),
            KeyCode::Up => out.push_str("Up"),
            KeyCode::Down => out.push_str("Down"),
            KeyCode::Left => out.push_str("Left"),
            KeyCode::Right => out.push_str("Right"),
            KeyCode::Home => out.push_str("Home"),
            KeyCode::End => out.push_str("End"),
            KeyCode::PageUp => out.push_str("PgUp"),
            KeyCode::PageDown => out.push_str("PgDn"),
            KeyCode::F(n) => out.push_str(&format!("F{n}")),
            other => out.push_str(&format!("{other:?}")),
        }
        out
    }

    /// The `settings.toml` spelling of this key: the inverse of `parse_key`. None when no
    /// spelling reads back as exactly this key.
    pub(crate) fn to_setting(self) -> Option<String> {
        let capital = |c: char| {
            c.is_uppercase()
                && self
                    .mods
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        };
        let (name, shift) = match self.code {
            KeyCode::Char(' ') => ("space".to_string(), false),
            // A capital with Ctrl or Alt means Shift was held: the file says so.
            KeyCode::Char(c) if capital(c) => (c.to_lowercase().collect(), true),
            KeyCode::Char(c) => (c.to_string(), false),
            KeyCode::BackTab => ("tab".to_string(), true),
            KeyCode::Tab => ("tab".to_string(), false),
            KeyCode::Enter => ("enter".to_string(), false),
            KeyCode::Esc => ("esc".to_string(), false),
            KeyCode::Backspace => ("backspace".to_string(), false),
            KeyCode::Delete => ("delete".to_string(), false),
            KeyCode::Insert => ("insert".to_string(), false),
            KeyCode::Up => ("up".to_string(), false),
            KeyCode::Down => ("down".to_string(), false),
            KeyCode::Left => ("left".to_string(), false),
            KeyCode::Right => ("right".to_string(), false),
            KeyCode::Home => ("home".to_string(), false),
            KeyCode::End => ("end".to_string(), false),
            KeyCode::PageUp => ("pgup".to_string(), false),
            KeyCode::PageDown => ("pgdn".to_string(), false),
            KeyCode::F(n) => (format!("f{n}"), false),
            _ => return None,
        };
        let mut out = String::new();
        for (on, prefix) in [
            (self.mods.contains(KeyModifiers::CONTROL), "ctrl+"),
            (self.mods.contains(KeyModifiers::ALT), "alt+"),
            (shift || self.mods.contains(KeyModifiers::SHIFT), "shift+"),
        ] {
            if on {
                out.push_str(prefix);
            }
        }
        out.push_str(&name);
        (parse_key(&out).ok() == Some(self)).then_some(out)
    }

    /// Shift+Enter or Ctrl+Enter: a Unix terminal without the kitty keyboard protocol sends
    /// these as a plain Enter.
    fn is_modified_enter(&self) -> bool {
        self.code == KeyCode::Enter
            && self
                .mods
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
    }

    /// A plain printable key or Backspace: what text boxes need for typing.
    pub(crate) fn types_text(&self) -> bool {
        let no_mods = !self
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        no_mods && matches!(self.code, KeyCode::Char(_) | KeyCode::Backspace)
    }

    /// A plain printable key: it types a character in a text box.
    pub(crate) fn prints(&self) -> bool {
        let no_mods = !self
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        no_mods && matches!(self.code, KeyCode::Char(_))
    }
}

/// True once the terminal reports Shift and Ctrl on Enter: the Windows console always does, a
/// Unix terminal only after chatcli turns on the kitty keyboard protocol. Off in tests.
static MODIFIED_ENTER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called once the terminal is set up: whether Shift+Enter reaches chatcli.
pub fn set_modified_enter(on: bool) {
    MODIFIED_ENTER.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The key a footer hint shows: the first one the terminal can send, else the first.
fn hint_key(chords: &[KeyChord], modified_enter: bool) -> Option<KeyChord> {
    chords
        .iter()
        .find(|k| modified_enter || !k.is_modified_enter())
        .or(chords.first())
        .copied()
}

/// Reads a key string from `settings.toml`: `a`, `G`, `enter`, `esc`, `tab`, `up`, `pgup`,
/// `f5`, with `ctrl+`, `alt+` and `shift+` prefixes. A single character keeps its case; names
/// and prefixes do not care. `+` alone is the plus key.
pub fn parse_key(s: &str) -> Result<KeyChord, String> {
    let s = s.trim();
    let bad = || format!("cannot read the key \"{s}\"");
    let mut mods = KeyModifiers::NONE;
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        let found = [
            ("ctrl+", KeyModifiers::CONTROL),
            ("control+", KeyModifiers::CONTROL),
            ("alt+", KeyModifiers::ALT),
            ("shift+", KeyModifiers::SHIFT),
        ]
        .into_iter()
        .find(|(p, _)| lower.starts_with(p) && rest.len() > p.len());
        match found {
            Some((p, m)) => {
                mods |= m;
                rest = &rest[p.len()..];
            }
            None => break,
        }
    }
    let mut chars = rest.chars();
    let mut code = match (chars.next(), chars.next()) {
        (None, _) => return Err(bad()),
        (Some(c), None) => KeyCode::Char(c),
        _ => match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" | "pageup" => KeyCode::PageUp,
            "pgdn" | "pgdown" | "pagedown" => KeyCode::PageDown,
            f => match f.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                Some(n @ 1..=24) => KeyCode::F(n),
                _ => return Err(bad()),
            },
        },
    };
    // Terminals send Ctrl+N as 'n' + CONTROL, so "Ctrl+N" in the file means that key. Shift
    // asked for explicitly keeps the capital.
    if let KeyCode::Char(c) = code {
        if mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::SHIFT) {
            code = KeyCode::Char(c.to_ascii_lowercase());
        }
    }
    Ok(KeyChord::new(code, mods))
}

/// Ctrl chords that many terminals cannot tell from another key.
fn ambiguous(chord: &KeyChord) -> Option<&'static str> {
    if chord.mods != KeyModifiers::CONTROL {
        return None;
    }
    match chord.code {
        KeyCode::Char('i') => Some("Tab"),
        KeyCode::Char('m') | KeyCode::Char('j') => Some("Enter"),
        KeyCode::Char('[') => Some("Esc"),
        KeyCode::Char('h') => Some("Backspace"),
        _ => None,
    }
}

// ---- built-in keys ----

type Row = (Context, &'static str, Action);

/// `[keys.common]`: the keys that mean the same thing wherever they work. The order is the order
/// footer hints prefer: `?` before F1, the arrows before `k` and `j`.
const COMMON: &[Row] = &[
    (Context::Common, "ctrl+c", Action::Quit),
    (Context::Common, "tab", Action::NextNetwork),
    (Context::Common, "shift+tab", Action::PrevNetwork),
    (Context::Common, "alt+1", Action::GotoNetwork1),
    (Context::Common, "alt+2", Action::GotoNetwork2),
    (Context::Common, "?", Action::Help),
    (Context::Common, "f1", Action::Help),
    (Context::Common, "f2", Action::Settings),
    (Context::Common, "ctrl+f", Action::Search),
    (Context::Common, ":", Action::CommandBar),
    (Context::Common, "/", Action::FilterStart),
    // Movement, accept and back: the chat list, message selection, lists and help.
    (Context::Common, "up", Action::Up),
    (Context::Common, "k", Action::Up),
    (Context::Common, "down", Action::Down),
    (Context::Common, "j", Action::Down),
    (Context::Common, "g", Action::Top),
    (Context::Common, "G", Action::Bottom),
    (Context::Common, "pgup", Action::PageUp),
    (Context::Common, "pgdn", Action::PageDown),
    (Context::Common, "H", Action::ScreenTop),
    (Context::Common, "L", Action::ScreenBottom),
    (Context::Common, "enter", Action::Accept),
    (Context::Common, "esc", Action::Back),
    // Editing, in every box you type in: the message box, the filter, the command bar, search,
    // the emoji search and the file prompt.
    (Context::Common, "left", Action::CursorLeft),
    (Context::Common, "right", Action::CursorRight),
    (Context::Common, "ctrl+left", Action::WordLeft),
    (Context::Common, "ctrl+right", Action::WordRight),
    (Context::Common, "home", Action::LineStart),
    (Context::Common, "end", Action::LineEnd),
    (Context::Common, "backspace", Action::DeleteBack),
    (Context::Common, "delete", Action::DeleteForward),
    (Context::Common, "ctrl+w", Action::DeleteWordBack),
    (Context::Common, "ctrl+backspace", Action::DeleteWordBack),
    (Context::Common, "ctrl+u", Action::ClearLine),
];

/// What each context adds to `common`. Home and End are the line keys in `common` (for the
/// boxes you type in), so the lists bind them to `top` and `bottom` themselves.
const OWN: &[Row] = &[
    // The chat list: Enter (or l, Right) goes into the chat with its newest message selected,
    // i goes straight to its message box.
    (Context::List, "l", Action::Accept),
    (Context::List, "right", Action::Accept),
    (Context::List, "i", Action::Compose),
    (Context::List, "home", Action::Top),
    (Context::List, "end", Action::Bottom),
    (Context::List, "r", Action::Refresh),
    (Context::List, "n", Action::NextUnread),
    (Context::List, "N", Action::PrevUnread),
    (Context::List, "R", Action::MarkRead),
    (Context::List, "p", Action::Pin),
    (Context::List, "m", Action::Mute),
    (Context::List, "z", Action::Hide),
    (Context::List, "Z", Action::ShowHidden),
    // Message selection. The per-message keys act on the highlighted message; ones that do not
    // apply to it are dimmed in the footer.
    (Context::Select, "h", Action::Back),
    (Context::Select, "left", Action::Back),
    (Context::Select, "i", Action::Compose),
    (Context::Select, "home", Action::Top),
    (Context::Select, "end", Action::Bottom),
    (Context::Select, "M", Action::ScreenMiddle),
    (Context::Select, "r", Action::Reply),
    (Context::Select, "e", Action::React),
    (Context::Select, "c", Action::Edit),
    (Context::Select, "x", Action::Delete),
    (Context::Select, "d", Action::Delete),
    (Context::Select, "u", Action::OpenLink),
    (Context::Select, "o", Action::OpenMedia),
    (Context::Select, "v", Action::ViewImage),
    (Context::Select, "y", Action::Copy),
    (Context::Select, "/", Action::SearchChat),
    (Context::Select, "n", Action::NextMatch),
    (Context::Select, "N", Action::PrevMatch),
    (Context::Select, "ctrl+o", Action::JumpBack),
    (Context::Select, "enter", Action::JumpToQuoted),
    // The message box (its editing keys are in `common`).
    (Context::Composer, "enter", Action::Send),
    // Shift+Enter first: the footer shows it where the terminal reports the Shift (the Windows
    // console, or a terminal with the kitty keyboard protocol), else Ctrl+J, which reaches
    // chatcli in every terminal. Windows Terminal keeps Alt+Enter for fullscreen.
    (Context::Composer, "shift+enter", Action::InsertNewline),
    (Context::Composer, "ctrl+j", Action::InsertNewline),
    (Context::Composer, "alt+enter", Action::InsertNewline),
    (Context::Composer, "ctrl+e", Action::EmojiPicker),
    (Context::Composer, "ctrl+o", Action::Attach),
    // Windows Terminal keeps Ctrl+Shift+V for its own paste (which cannot paste a picture), so
    // Alt+V reaches chatcli there.
    (Context::Composer, "ctrl+shift+v", Action::PasteImage),
    (Context::Composer, "alt+v", Action::PasteImage),
    (Context::Confirm, "y", Action::ConfirmYes),
    (Context::Confirm, "n", Action::ConfirmNo),
    (Context::Help, "home", Action::Top),
    (Context::Help, "end", Action::Bottom),
];
/// Two-key sequences, held until the second key; no timer. One in `common` works where its action
/// applies, like a single key there. None are bound by default (`g` alone goes to the top).
const SEQUENCES: &[(Context, &str, &str, Action)] = &[];

/// Every built-in single-key binding, in display order.
fn rows() -> Vec<Row> {
    COMMON.iter().chain(OWN.iter()).copied().collect()
}

fn builtin(key: &str) -> KeyChord {
    parse_key(key).expect("built-in key string")
}

/// Actions with no default key that can still be bound: the keys page lists them too.
const EXTRA: &[(Context, Action)] = &[
    (Context::List, Action::OpenLink),
    (Context::List, Action::MarkAllRead),
];

/// Where a key bound to `action` in `[keys.common]` works: quitting everywhere; the tabs, help,
/// settings, search and the command bar outside the overlays; the editing keys in every box you
/// type in; any other action where chatcli itself binds it.
pub(crate) fn common_applies(action: Action, ctx: Context) -> bool {
    use Action::*;
    use Context as C;
    match action {
        Quit => true,
        NextNetwork | PrevNetwork | GotoNetwork1 | GotoNetwork2 | Help | Settings | Search
        | CommandBar => matches!(
            ctx,
            C::Panel | C::List | C::Filter | C::Composer | C::Select
        ),
        CursorLeft | CursorRight | WordLeft | WordRight | LineStart | LineEnd | DeleteBack
        | DeleteForward | DeleteWordBack | ClearLine => ctx.types_text(),
        Up | Down => matches!(ctx, C::List | C::Filter | C::Select | C::Popup | C::Help),
        Top | Bottom | ScreenTop | ScreenBottom => matches!(ctx, C::List | C::Select | C::Help),
        ScreenMiddle | Compose => matches!(ctx, C::List | C::Select),
        FilterStart => ctx == C::List,
        PageUp | PageDown => {
            matches!(ctx, C::List | C::Composer | C::Select | C::Popup | C::Help)
        }
        Accept => matches!(ctx, C::Panel | C::List | C::Filter | C::Popup),
        Back => !matches!(ctx, C::Common | C::Panel),
        _ => {
            ctx != C::Common
                && (rows().iter().any(|r| r.0 == ctx && r.2 == action)
                    || EXTRA.contains(&(ctx, action)))
        }
    }
}

/// True when every context `action` works in is a box you type in (the cursor and delete keys):
/// a `[keys.common]` key that types a character would then do nothing anywhere.
pub(crate) fn only_where_you_type(action: Action) -> bool {
    let mut places = Context::ALL
        .into_iter()
        .filter(|c| *c != Context::Common && common_applies(action, *c))
        .peekable();
    places.peek().is_some() && places.all(Context::types_text)
}

/// The keys chatcli itself gives `action` in `ctx` (no user bindings).
pub(crate) fn default_keys(ctx: Context, action: Action) -> Vec<KeyChord> {
    rows()
        .into_iter()
        .filter(|r| r.0 == ctx && r.2 == action)
        .map(|r| builtin(r.1))
        .collect()
}

/// What the keys page lists for `ctx`: the built-in actions in order, then the bindable ones
/// with no default key, then any the user's `[keys.<ctx>]` names. No action twice.
pub(crate) fn actions_for(ctx: Context, user: &KeySettings) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    let mut add = |a: Action| {
        if a != Action::Unbound && !out.contains(&a) {
            out.push(a);
        }
    };
    for r in rows().into_iter().filter(|r| r.0 == ctx) {
        add(r.2);
    }
    for s in SEQUENCES.iter().filter(|s| s.0 == ctx) {
        add(s.3);
    }
    for (_, a) in EXTRA.iter().filter(|e| e.0 == ctx) {
        add(*a);
    }
    if let Some(table) = user.bindings.get(ctx.name()) {
        for name in table.values() {
            if let Some(a) = Action::from_name(name) {
                add(a);
            }
        }
    }
    out
}

/// One group of the help overlay: a context and its (action, keys) lines.
pub type HelpSection = (Context, Vec<(Action, Vec<String>)>);

/// What `Keymap::step` decided for one key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Run this action.
    Action(Action),
    /// First key of a sequence: hold it and wait for the next key.
    Pending,
    /// A held key got a key that completes nothing: drop both, do nothing.
    Cancel,
    /// Not bound: text boxes type it, everywhere else it is ignored.
    Unbound,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    /// In display order: the first key an action has in a context is the one hints show.
    single: Vec<(Context, KeyChord, Action)>,
    seq: Vec<(Context, KeyChord, KeyChord, Action)>,
    /// Keys a context's own table unbinds that `[keys.common]` binds: common does not reach
    /// them there.
    hidden: Vec<(Context, KeyChord)>,
    user: usize,
}

impl Default for Keymap {
    fn default() -> Keymap {
        Keymap::build(&KeySettings::default()).0
    }
}

impl Keymap {
    /// How many user bindings were applied (for the help header).
    pub fn user_bindings(&self) -> usize {
        self.user
    }

    pub(crate) fn find(&self, ctx: Context, chord: KeyChord) -> Option<Action> {
        self.single
            .iter()
            .find(|(c, k, _)| *c == ctx && *k == chord)
            .map(|e| e.2)
    }

    /// The action for a key: the context's own table, then `[keys.common]`.
    pub fn lookup(&self, ctx: Context, chord: KeyChord) -> Option<Action> {
        self.find(ctx, chord)
            .or_else(|| self.common_for(ctx, chord))
    }

    /// What `[keys.common]` gives `chord` in `ctx`: only an action that applies there, and never a
    /// key that types a character in a box you type in.
    pub(crate) fn common_for(&self, ctx: Context, chord: KeyChord) -> Option<Action> {
        if ctx == Context::Common
            || (ctx.types_text() && chord.prints())
            || self.hidden.contains(&(ctx, chord))
        {
            return None;
        }
        self.find(Context::Common, chord)
            .filter(|a| common_applies(*a, ctx))
    }

    /// The single keys `action` has in exactly `ctx` (not `[keys.common]`), the first one being the one
    /// hints show. Sequences are not listed.
    pub(crate) fn keys_in(&self, ctx: Context, action: Action) -> Vec<KeyChord> {
        self.single
            .iter()
            .filter(|(c, _, a)| *c == ctx && *a == action)
            .map(|(_, k, _)| *k)
            .collect()
    }

    /// The two-key sequences `action` has in `ctx`, as display text (`g t`).
    pub(crate) fn sequences_in(&self, ctx: Context, action: Action) -> Vec<String> {
        self.seq
            .iter()
            .filter(|s| s.0 == ctx && s.3 == action)
            .map(|s| format!("{} {}", s.1.display(), s.2.display()))
            .collect()
    }

    /// The two-key sequences live in `ctx` that start with `first` (its own and the `common` ones
    /// that work there), as display text. A single-key binding on `first` in `ctx` replaces them.
    pub(crate) fn sequences_from(&self, ctx: Context, first: KeyChord) -> Vec<String> {
        self.seqs_from(ctx, first)
            .map(|s| format!("{} {}", s.1.display(), s.2.display()))
            .collect()
    }

    /// The two-key sequences `ctx`'s own table starts with `first` (not the `common` ones).
    pub(crate) fn own_sequences_from(&self, ctx: Context, first: KeyChord) -> Vec<String> {
        self.seq
            .iter()
            .filter(|s| s.0 == ctx && s.1 == first)
            .map(|s| format!("{} {}", s.1.display(), s.2.display()))
            .collect()
    }

    /// The sequences live in `ctx` that start with `first`: its own, then the `common` ones whose
    /// action applies there (never in a box you type in when a key of the sequence types).
    fn seqs_from(
        &self,
        ctx: Context,
        first: KeyChord,
    ) -> impl Iterator<Item = &(Context, KeyChord, KeyChord, Action)> {
        let own = self.seq.iter().filter(move |s| s.0 == ctx && s.1 == first);
        let common = self.seq.iter().filter(move |s| {
            s.0 == Context::Common
                && !self.hidden.contains(&(ctx, first))
                && ctx != Context::Common
                && s.1 == first
                && common_applies(s.3, ctx)
                && !(ctx.types_text() && (s.1.prints() || s.2.prints()))
        });
        own.chain(common)
    }

    /// The pending-key state machine. `pending` is the held first key, if any. A held key is
    /// dropped by any key that does not complete a sequence, and the key does nothing.
    pub fn step(&self, ctx: Context, pending: Option<KeyChord>, chord: KeyChord) -> Step {
        if let Some(first) = pending {
            return match self.seqs_from(ctx, first).find(|s| s.2 == chord) {
                Some(s) => Step::Action(s.3),
                None => Step::Cancel,
            };
        }
        if let Some(a) = self.lookup(ctx, chord) {
            return Step::Action(a);
        }
        if self.seqs_from(ctx, chord).next().is_some() {
            return Step::Pending;
        }
        Step::Unbound
    }

    /// Keys of `action` in `ctx` (then `[keys.common]`), as display text, in priority order.
    pub fn keys_for(&self, ctx: Context, action: Action) -> Vec<String> {
        self.chords_for(ctx, action)
            .iter()
            .map(KeyChord::display)
            .collect()
    }

    /// In keymap order: the user's keys first, then the `common` ones, then the context's own.
    fn chords_for(&self, ctx: Context, action: Action) -> Vec<KeyChord> {
        self.single
            .iter()
            .filter(|(c, k, a)| {
                *a == action
                    && (*c == ctx
                        // A common key that reaches `action` here: not shadowed by this
                        // context's own table.
                        || (*c == Context::Common
                            && self.find(ctx, *k).is_none()
                            && self.common_for(ctx, *k) == Some(action)))
            })
            .map(|(_, k, _)| *k)
            .collect()
    }

    /// Footer text for a group of actions: the first key of each that this terminal can send,
    /// joined with `/` (`Up/Down`). Unbound actions are left out; `None` when none is bound.
    pub fn hint(&self, ctx: Context, actions: &[Action]) -> Option<String> {
        let keys: Vec<String> = actions
            .iter()
            .filter_map(|&a| {
                hint_key(
                    &self.chords_for(ctx, a),
                    MODIFIED_ENTER.load(std::sync::atomic::Ordering::Relaxed),
                )
                .map(|k| k.display())
            })
            .collect();
        (!keys.is_empty()).then(|| keys.join("/"))
    }

    /// Everything the keymap binds, grouped by context, for the help overlay:
    /// (context, [(action, keys)]). Sequences read as `g g`.
    pub fn help_sections(&self) -> Vec<HelpSection> {
        let mut out = Vec::new();
        for ctx in Context::ALL {
            let mut entries: Vec<(Action, Vec<String>)> = Vec::new();
            let mut add = |a: Action, key: String| match entries.iter_mut().find(|e| e.0 == a) {
                Some(e) => e.1.push(key),
                None => entries.push((a, vec![key])),
            };
            for (_, k, a) in self.single.iter().filter(|e| e.0 == ctx) {
                add(*a, k.display());
            }
            for (_, k1, k2, a) in self.seq.iter().filter(|e| e.0 == ctx) {
                add(*a, format!("{} {}", k1.display(), k2.display()));
            }
            if !entries.is_empty() {
                out.push((ctx, entries));
            }
        }
        out
    }

    /// The ways out the bindings have taken away: `(quit, context)` where no key quits, and
    /// `(back, overlay)` where no key closes help, a list or a y/n question. Each is worked out
    /// through `lookup`, so a key the context binds to something else does not count.
    pub(crate) fn missing_exits(&self) -> Vec<(Action, Context)> {
        let reaches = |ctx: Context, exits: &[Action]| {
            self.single.iter().any(|(_, k, _)| {
                self.lookup(ctx, *k)
                    .is_some_and(|action| exits.contains(&action))
            })
        };
        let mut out = Vec::new();
        for ctx in Context::ALL.into_iter().filter(|c| *c != Context::Common) {
            if !reaches(ctx, &[Action::Quit]) {
                out.push((Action::Quit, ctx));
            }
        }
        for ctx in [Context::Popup, Context::Confirm, Context::Help] {
            // "No" closes a y/n question as well as `back` does.
            let exits: &[Action] = if ctx == Context::Confirm {
                &[Action::Back, Action::ConfirmNo]
            } else {
                &[Action::Back]
            };
            if !reaches(ctx, exits) {
                out.push((Action::Back, ctx));
            }
        }
        out
    }

    /// The built-in keys plus the user's `[keys]` tables, and every problem found. Bad bindings
    /// are skipped; the rest still apply. Where the bindings leave no way out (`missing_exits`),
    /// Ctrl+C quits and Esc backs out there whatever they were bound to, and the problem says so.
    pub fn build(user: &KeySettings) -> (Keymap, Vec<SettingsProblem>) {
        let (mut map, mut problems) = Keymap::layer(user);
        // Putting Ctrl+C back can take a context's only back key, and Esc its only quit key: repair
        // until nothing is missing. Once a context has both, nothing takes them again, so this ends
        // within a few rounds.
        let mut repaired: Vec<(Action, Context)> = Vec::new();
        for _ in 0..4 {
            let missing = map.missing_exits();
            if missing.is_empty() {
                break;
            }
            for (action, ctx) in missing {
                let key = if action == Action::Quit { CTRL_C } else { ESC };
                map.single.retain(|(c, k, _)| !(*c == ctx && *k == key));
                map.single.push((ctx, key, action));
                if !repaired.contains(&(action, ctx)) {
                    repaired.push((action, ctx));
                }
            }
        }
        for (action, key, what) in [
            (Action::Quit, CTRL_C, "quits chatcli"),
            (Action::Back, ESC, "backs out"),
        ] {
            let ctxs: Vec<Context> = repaired
                .iter()
                .filter(|m| m.0 == action)
                .map(|m| m.1)
                .collect();
            if ctxs.is_empty() {
                continue;
            }
            let names: Vec<String> = ctxs
                .iter()
                .map(|c| format!("[keys.{}]", c.name()))
                .collect();
            problems.push(SettingsProblem {
                severity: Severity::Error,
                line: None,
                section: "keys".to_string(),
                key: None,
                message: format!(
                    "no key {what} in {}: bind one to \"{}\" there or in [keys.common]. Until then {} does it there.",
                    names.join(", "),
                    action.name(),
                    key.display()
                ),
            });
        }
        (map, problems)
    }

    /// The built-in keys with the user's `[keys]` tables over them, before the ways out are
    /// checked: `build` is the one to use.
    pub(crate) fn layer(user: &KeySettings) -> (Keymap, Vec<SettingsProblem>) {
        let mut map = Keymap {
            single: rows()
                .into_iter()
                .map(|r| (r.0, builtin(r.1), r.2))
                .collect(),
            seq: SEQUENCES
                .iter()
                .map(|s| (s.0, builtin(s.1), builtin(s.2), s.3))
                .collect(),
            hidden: Vec::new(),
            user: 0,
        };
        let mut problems = Vec::new();
        // User bindings go before the built-in ones, so the hint shows the key the user chose.
        let mut insert_at = 0;
        for (ctx_name, table) in &user.bindings {
            let problem = |sev: Severity, key: Option<&str>, msg: String| SettingsProblem {
                severity: sev,
                line: key.and_then(|k| user.lines.get(&(ctx_name.clone(), k.to_string())).copied()),
                section: format!("keys.{ctx_name}"),
                key: key.map(str::to_string),
                message: msg,
            };
            let Some(ctx) = Context::from_name(ctx_name) else {
                let names: Vec<&str> = Context::ALL.iter().map(|c| c.name()).collect();
                let hint = if ctx_name == "global" {
                    "[keys.global] is now [keys.common].".to_string()
                } else {
                    closest(ctx_name, names.iter().copied())
                        .map_or(format!("use one of {}", names.join(", ")), |c| {
                            format!("did you mean [keys.{c}]?")
                        })
                };
                problems.push(problem(
                    Severity::Error,
                    None,
                    format!("unknown table: {hint} Skipping its bindings."),
                ));
                continue;
            };
            // Entries in file order so "same key twice" blames the later one.
            let mut entries: Vec<(&String, &String)> = table.iter().collect();
            entries.sort_by_key(|(k, _)| {
                (
                    user.lines
                        .get(&(ctx_name.clone(), (*k).clone()))
                        .copied()
                        .unwrap_or(usize::MAX),
                    (*k).clone(),
                )
            });
            let mut seen: Vec<(KeyChord, &String)> = Vec::new();
            for (key_str, action_str) in entries {
                let key = Some(key_str.as_str());
                let chord = match parse_key(key_str) {
                    Ok(c) => c,
                    Err(e) => {
                        problems.push(problem(
                            Severity::Error,
                            key,
                            format!("{e}: write it like a, enter, esc, ctrl+n, alt+1 or f5. Skipping this binding."),
                        ));
                        continue;
                    }
                };
                if let Some((_, first)) = seen.iter().find(|(c, _)| *c == chord) {
                    problems.push(problem(
                        Severity::Error,
                        key,
                        format!("same key as \"{first}\" above: bind a key once per table. Skipping this binding."),
                    ));
                    continue;
                }
                seen.push((chord, key_str));
                let Some(action) = Action::from_name(action_str) else {
                    let hint = closest(action_str, Action::ALL.iter().map(|a| a.name()))
                        .map_or("see the action list in docs/guide.md".to_string(), |a| {
                            format!("did you mean \"{a}\"?")
                        });
                    problems.push(problem(
                        Severity::Error,
                        key,
                        format!("unknown action \"{action_str}\": {hint} Skipping this binding."),
                    ));
                    continue;
                };
                if action != Action::Unbound && ctx.types_text() && chord.types_text() {
                    problems.push(problem(
                        Severity::Error,
                        key,
                        "would stop typing in this box: bind a Ctrl or Alt key instead, or put it in [keys.common], which skips the boxes you type in. Skipping this binding.".to_string(),
                    ));
                    continue;
                }
                if ctx == Context::Common
                    && action != Action::Unbound
                    && chord.prints()
                    && only_where_you_type(action)
                {
                    problems.push(problem(
                        Severity::Warning,
                        key,
                        format!(
                            "\"{}\" works only in the boxes you type in, where this key types text: it does nothing. Use a Ctrl or Alt key.",
                            action.name()
                        ),
                    ));
                }
                if let Some(same) = ambiguous(&chord) {
                    problems.push(problem(
                        Severity::Warning,
                        key,
                        format!("many terminals send this key as {same}, so it may not work on its own."),
                    ));
                }
                if action == Action::Unbound {
                    // A key `common` binds stops reaching this context.
                    let shadows = ctx != Context::Common
                        && (map.find(Context::Common, chord).is_some()
                            || map
                                .seq
                                .iter()
                                .any(|s| s.0 == Context::Common && s.1 == chord));
                    if shadows {
                        map.hidden.push((ctx, chord));
                    }
                    let before = map.single.len();
                    map.single.retain(|(c, k, _)| !(*c == ctx && *k == chord));
                    let had_seq = map.seq.len();
                    map.seq.retain(|s| !(s.0 == ctx && s.1 == chord));
                    if !shadows
                        && map.single.len() == before
                        && map.seq.len() == had_seq
                        && !rows()
                            .into_iter()
                            .any(|r| r.0 == ctx && builtin(r.1) == chord)
                        && !SEQUENCES
                            .iter()
                            .any(|s| s.0 == ctx && builtin(s.1) == chord)
                    {
                        problems.push(problem(
                            Severity::Warning,
                            key,
                            "nothing is bound to this key, so there is nothing to unbind."
                                .to_string(),
                        ));
                    }
                    map.user += 1;
                    continue;
                }
                map.single.retain(|(c, k, _)| !(*c == ctx && *k == chord));
                if map.seq.iter().any(|s| s.0 == ctx && s.1 == chord) {
                    map.seq.retain(|s| !(s.0 == ctx && s.1 == chord));
                    problems.push(problem(
                        Severity::Warning,
                        key,
                        "this key starts two-key sequences here: your binding replaces them."
                            .to_string(),
                    ));
                }
                map.single
                    .insert(insert_at.min(map.single.len()), (ctx, chord, action));
                insert_at += 1;
                map.user += 1;
            }
        }
        (map, problems)
    }
}

/// Problems with every binding in `[keys]`. This is the TUI's half of `chatcli settings check`
/// (core only checks structure).
pub fn validate_keys(settings: &Settings) -> Vec<SettingsProblem> {
    Keymap::build(&settings.keys).1
}

/// The candidate closest to `word`, when it is close enough to be a typo.
pub(crate) fn closest<'a>(
    word: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    candidates
        .map(|c| (levenshtein(word, c), c))
        .min_by_key(|(d, _)| *d)
        .filter(|(d, _)| *d <= (word.chars().count() / 3).max(2) && *d < word.chars().count())
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(diag + usize::from(ca != *cb));
            diag = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn k(s: &str) -> KeyChord {
        parse_key(s).unwrap()
    }

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyChord {
        KeyChord::new(code, mods)
    }

    #[test]
    fn the_newline_hint_is_shift_enter_only_where_the_terminal_sends_it() {
        let map = Keymap::default();
        let chords = map.chords_for(Context::Composer, Action::InsertNewline);
        assert_eq!(chords, vec![k("shift+enter"), k("ctrl+j"), k("alt+enter")]);
        assert_eq!(hint_key(&chords, true), Some(k("shift+enter")));
        assert_eq!(
            hint_key(&chords, false),
            Some(k("ctrl+j")),
            "a terminal without the protocol sends Shift+Enter as Enter"
        );
        assert_eq!(hint_key(&[k("ctrl+enter")], false), Some(k("ctrl+enter")));
        assert_eq!(hint_key(&[k("alt+enter")], false), Some(k("alt+enter")));
        assert_eq!(hint_key(&[], true), None);
        // Tests never turn the protocol on, so the footer reads as in a plain terminal.
        assert_eq!(
            map.hint(Context::Composer, &[Action::InsertNewline])
                .as_deref(),
            Some("Ctrl+J")
        );
    }

    /// `[keys]` as the settings loader would hand it over: (context, key, action) and a line
    /// number equal to the entry's position.
    fn user(entries: &[(&str, &str, &str)]) -> KeySettings {
        let mut s = KeySettings::default();
        for (i, (c, key, a)) in entries.iter().enumerate() {
            s.bindings
                .entry((*c).to_string())
                .or_insert_with(BTreeMap::new)
                .insert((*key).to_string(), (*a).to_string());
            s.lines
                .insert(((*c).to_string(), (*key).to_string()), i + 1);
        }
        s
    }

    fn build(entries: &[(&str, &str, &str)]) -> (Keymap, Vec<SettingsProblem>) {
        Keymap::build(&user(entries))
    }

    fn texts(p: &[SettingsProblem]) -> Vec<String> {
        p.iter().map(|p| p.text()).collect()
    }

    #[test]
    fn parses_key_strings() {
        assert_eq!(k("a"), ev(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(k("G"), ev(KeyCode::Char('G'), KeyModifiers::NONE));
        assert_eq!(k("shift+g"), k("G"));
        assert_eq!(k("Ctrl+N"), ev(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(k("ctrl+n"), ev(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(
            k("ctrl+shift+v"),
            ev(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        );
        assert_eq!(k("alt+1"), ev(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(k("enter"), ev(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(k("PgUp"), ev(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(k("f5"), ev(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(k("shift+tab"), ev(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(k("shift+up"), ev(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(k("+"), ev(KeyCode::Char('+'), KeyModifiers::NONE));
        assert_eq!(k("ctrl++"), ev(KeyCode::Char('+'), KeyModifiers::CONTROL));
        assert_eq!(k("space"), ev(KeyCode::Char(' '), KeyModifiers::NONE));
        for bad in ["", "ctrl+", "f99", "ctrl+enterr", "banana"] {
            assert!(parse_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn terminal_variants_of_one_key_are_equal() {
        // Windows sends Shift+g as 'G' + SHIFT, Unix as 'G' alone, some as 'g' + SHIFT.
        let want = k("G");
        assert_eq!(ev(KeyCode::Char('G'), KeyModifiers::SHIFT), want);
        assert_eq!(ev(KeyCode::Char('G'), KeyModifiers::NONE), want);
        assert_eq!(ev(KeyCode::Char('g'), KeyModifiers::SHIFT), want);
        assert_eq!(ev(KeyCode::Char('?'), KeyModifiers::SHIFT), k("?"));
        assert_eq!(ev(KeyCode::BackTab, KeyModifiers::SHIFT), k("shift+tab"));
        assert_eq!(ev(KeyCode::Tab, KeyModifiers::SHIFT), k("shift+tab"));
        // Modifiers the keymap does not know about (Super) are ignored.
        assert_eq!(ev(KeyCode::Enter, KeyModifiers::SUPER), k("enter"));
    }

    #[test]
    fn display_text() {
        assert_eq!(k("ctrl+d").display(), "Ctrl+D");
        assert_eq!(k("shift+tab").display(), "Shift+Tab");
        assert_eq!(k("pgdn").display(), "PgDn");
        assert_eq!(k("G").display(), "G");
        assert_eq!(k("alt+1").display(), "Alt+1");
        assert_eq!(k("space").display(), "Space");
        assert_eq!(k("f2").display(), "F2");
        // Ctrl+Shift+V must not read like Ctrl+V.
        assert_eq!(k("ctrl+shift+v").display(), "Ctrl+Shift+V");
        assert_eq!(k("ctrl+v").display(), "Ctrl+V");
    }

    #[test]
    fn builtin_strings_parse_and_have_no_duplicates() {
        let map = Keymap::default();
        let mut seen = Vec::new();
        for (c, k, _) in &map.single {
            assert!(!seen.contains(&(*c, *k)), "binds {c:?} {k:?} twice");
            seen.push((*c, *k));
        }
        // Every action name round-trips, and no two share a name.
        for a in Action::ALL {
            assert_eq!(Action::from_name(a.name()), Some(*a));
            assert!(!a.doc().is_empty());
        }
        let mut names: Vec<_> = Action::ALL.iter().map(|a| a.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Action::ALL.len());
        for c in Context::ALL {
            assert_eq!(Context::from_name(c.name()), Some(c));
        }
    }

    /// The README key table, key by key, against the default keymap.
    #[test]
    fn readme_key_table_matches_the_default_keymap() {
        use Action::*;
        use Context::*;
        let map = Keymap::default();
        let table: &[(Context, &str, Action)] = &[
            (List, "tab", NextNetwork),
            (Composer, "tab", NextNetwork),
            (Filter, "tab", NextNetwork),
            (Panel, "tab", NextNetwork),
            (List, "shift+tab", PrevNetwork),
            (List, "alt+1", GotoNetwork1),
            (List, "alt+2", GotoNetwork2),
            (List, "up", Up),
            (List, "k", Up),
            (List, "down", Down),
            (List, "j", Down),
            (List, "home", Top),
            (List, "end", Bottom),
            (List, "G", Bottom),
            (List, "H", ScreenTop),
            (List, "L", ScreenBottom),
            (List, "enter", Accept),
            (List, "l", Accept),
            (List, "right", Accept),
            (List, "i", Compose),
            (Panel, "enter", Accept),
            (List, ":", CommandBar),
            (Panel, ":", CommandBar),
            (Select, ":", CommandBar),
            (List, "/", FilterStart),
            (Composer, "f1", Action::Help),
            (List, "?", Action::Help),
            (Select, "?", Action::Help),
            (Filter, "enter", Accept),
            (Filter, "esc", Back),
            (Filter, "up", Up),
            (Filter, "down", Down),
            (List, "esc", Back),
            (List, "r", Refresh),
            (List, "R", MarkRead),
            (List, "z", Hide),
            (List, "Z", ShowHidden),
            (List, "pgup", PageUp),
            (List, "pgdn", PageDown),
            (Composer, "pgup", PageUp),
            (Composer, "pgdn", PageDown),
            (Composer, "enter", Send),
            (Composer, "esc", Back),
            (List, "f2", Settings),
            (Composer, "f2", Settings),
            (Context::Help, "esc", Back),
            (Context::Help, "H", ScreenTop),
            (Context::Help, "home", Top),
            (Composer, "left", CursorLeft),
            (Composer, "right", CursorRight),
            (Composer, "ctrl+left", WordLeft),
            (Composer, "ctrl+right", WordRight),
            (Composer, "home", LineStart),
            (Composer, "end", LineEnd),
            (Composer, "backspace", DeleteBack),
            (Composer, "delete", DeleteForward),
            (Composer, "ctrl+w", DeleteWordBack),
            (Composer, "ctrl+backspace", DeleteWordBack),
            (Composer, "ctrl+u", ClearLine),
            (Composer, "alt+enter", InsertNewline),
            (Composer, "shift+enter", InsertNewline),
            (Composer, "ctrl+j", InsertNewline),
            (Composer, "ctrl+e", EmojiPicker),
            (Popup, "up", Up),
            (Popup, "down", Down),
            (Popup, "enter", Accept),
            (Popup, "esc", Back),
            (Popup, "left", CursorLeft),
            (Popup, "right", CursorRight),
            (Popup, "pgup", PageUp),
            (Popup, "pgdn", PageDown),
            (Popup, "backspace", DeleteBack),
            (Composer, "ctrl+o", Attach),
            (Composer, "ctrl+shift+v", PasteImage),
            (Composer, "alt+v", PasteImage),
            (Select, "esc", Back),
            (Select, "h", Back),
            (Select, "left", Back),
            (Select, "i", Compose),
            (Select, "M", ScreenMiddle),
            (Select, "c", Edit),
            (Select, "x", Delete),
            (Select, "d", Delete),
            (Select, "o", OpenMedia),
            (Select, "/", SearchChat),
            (Select, "n", NextMatch),
            (Select, "N", PrevMatch),
            (Confirm, "y", ConfirmYes),
            (Confirm, "n", ConfirmNo),
            (Confirm, "esc", Back),
        ];
        for (ctx, key, want) in table {
            assert_eq!(map.lookup(*ctx, k(key)), Some(*want), "{ctx:?} {key}");
        }
    }

    /// Keys the redesign took away: quit and help are in `common` only, Tab no longer accepts,
    /// and help closes on Esc alone.
    #[test]
    fn the_removed_keys_do_nothing() {
        let map = Keymap::default();
        for (ctx, key) in [
            (Context::List, "q"),
            (Context::Panel, "q"),
            (Context::List, "u"),
            (Context::List, "M"),
            (Context::List, "x"),
            (Context::Select, "E"),
            (Context::Popup, "tab"),
            (Context::Help, "q"),
            (Context::Help, "?"),
            (Context::Help, "f1"),
            (Context::Composer, "ctrl+up"),
        ] {
            assert_eq!(map.lookup(ctx, k(key)), None, "{ctx:?} {key}");
        }
        // The actions stay, and can be bound.
        let (map, problems) = build(&[("list", "u", "open_link")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.lookup(Context::List, k("u")), Some(Action::OpenLink));
        // v views a picture in message selection.
        assert_eq!(
            Keymap::default().lookup(Context::Select, k("v")),
            Some(Action::ViewImage)
        );
    }

    /// `g` and `G` are in `common`: top and bottom where they apply, typed text where you type.
    #[test]
    fn g_goes_to_the_top_and_never_where_you_type() {
        let map = Keymap::default();
        for ctx in [Context::List, Context::Select, Context::Help] {
            assert_eq!(
                map.step(ctx, None, k("g")),
                Step::Action(Action::Top),
                "{ctx:?}"
            );
            assert_eq!(
                map.step(ctx, None, k("G")),
                Step::Action(Action::Bottom),
                "{ctx:?}"
            );
        }
        for ctx in [Context::Filter, Context::Composer, Context::Popup] {
            assert_eq!(map.step(ctx, None, k("g")), Step::Unbound, "{ctx:?}");
        }
    }

    /// A sequence in `common` (none are built in now, so one is added by hand): it works where
    /// its action applies and never where its first key types.
    #[test]
    fn a_common_sequence_works_where_its_action_applies() {
        let mut map = Keymap::default();
        map.single
            .retain(|(_, k, _)| *k != super::parse_key("g").unwrap());
        map.seq.push((Context::Common, k("g"), k("g"), Action::Top));
        for ctx in [Context::List, Context::Select, Context::Help] {
            assert_eq!(map.step(ctx, None, k("g")), Step::Pending, "{ctx:?}");
            assert_eq!(
                map.step(ctx, Some(k("g")), k("g")),
                Step::Action(Action::Top),
                "{ctx:?}"
            );
            assert_eq!(map.step(ctx, Some(k("g")), k("x")), Step::Cancel);
        }
        for ctx in [
            Context::Panel,
            Context::Filter,
            Context::Composer,
            Context::Popup,
            Context::Confirm,
        ] {
            assert_eq!(map.step(ctx, None, k("g")), Step::Unbound, "{ctx:?}");
        }
        assert_eq!(
            map.sequences_in(Context::Common, Action::Top),
            vec!["g g".to_string()]
        );
        // A context's `unbound` for the first key hides it there.
        map.hidden.push((Context::Select, k("g")));
        assert_eq!(map.step(Context::Select, None, k("g")), Step::Unbound);
        assert_eq!(map.step(Context::List, None, k("g")), Step::Pending);
    }

    /// Typing must win in text contexts: `q` types in the filter and the message box.
    #[test]
    fn plain_letters_are_unbound_where_you_type() {
        let map = Keymap::default();
        for ctx in [Context::Filter, Context::Composer] {
            for c in ['q', 'j', 'k', 'r', '/', '?', ':', 'G', 'g', 'i'] {
                let key = ev(KeyCode::Char(c), KeyModifiers::NONE);
                assert_eq!(map.step(ctx, None, key), Step::Unbound, "{ctx:?} {c}");
            }
            // Backspace is an action (delete_back, from `common`) in every box, so a rebinding can
            // move it.
            assert_eq!(
                map.step(ctx, None, ev(KeyCode::Backspace, KeyModifiers::NONE)),
                Step::Action(Action::DeleteBack)
            );
        }
    }

    #[test]
    fn ctrl_c_quits_from_every_context_by_default() {
        let ctrl_c = ev(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let map = Keymap::default();
        assert!(map.missing_exits().is_empty());
        for ctx in Context::ALL {
            assert_eq!(map.lookup(ctx, ctrl_c), Some(Action::Quit), "{ctx:?}");
            assert_eq!(map.step(ctx, None, ctrl_c), Step::Action(Action::Quit));
            // It is a key like any other: it drops a held key and does nothing.
            assert_eq!(map.step(ctx, Some(k("g")), ctrl_c), Step::Cancel);
        }
    }

    /// Caps Lock upper-cases the letter on Windows without reporting Shift: Ctrl+C must still
    /// quit, and a real Ctrl+Shift+C is a different key.
    #[test]
    fn caps_lock_does_not_break_ctrl_shortcuts() {
        let caps = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL);
        assert_eq!(KeyChord::from_event(&caps), CTRL_C);
        let map = Keymap::default();
        for ctx in Context::ALL {
            assert_eq!(
                map.step(ctx, None, KeyChord::from_event(&caps)),
                Step::Action(Action::Quit),
                "{ctx:?}"
            );
        }
        let real = ev(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_ne!(real, CTRL_C);
        // Other Ctrl bindings fold the same way.
        assert_eq!(ev(KeyCode::Char('E'), KeyModifiers::CONTROL), k("ctrl+e"));
        // A bare capital (Shift or Caps Lock in a text box) is untouched.
        assert_eq!(
            ev(KeyCode::Char('G'), KeyModifiers::NONE),
            ev(KeyCode::Char('G'), KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn typed_char_takes_altgr_and_leaves_shortcuts() {
        let key = |c, m| KeyEvent::new(KeyCode::Char(c), m);
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(typed_char(&key('a', KeyModifiers::NONE)), Some('a'));
        assert_eq!(typed_char(&key('A', KeyModifiers::SHIFT)), Some('A'));
        // AltGr+Q on a German layout, AltGr+7 for a brace, AltGr+E for the euro sign.
        assert_eq!(typed_char(&key('@', altgr)), Some('@'));
        assert_eq!(typed_char(&key('{', altgr)), Some('{'));
        assert_eq!(typed_char(&key('€', altgr)), Some('€'));
        // Ctrl or Alt alone are shortcuts, and Ctrl+Alt+letter is a chord.
        assert_eq!(typed_char(&key('c', KeyModifiers::CONTROL)), None);
        assert_eq!(typed_char(&key('v', KeyModifiers::ALT)), None);
        assert_eq!(typed_char(&key('x', altgr)), None);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(typed_char(&enter), None);
    }

    /// The footer shows the first key of an action: Ctrl+J reaches chatcli in every terminal,
    /// Alt+Enter is Windows Terminal's fullscreen key.
    #[test]
    fn the_newline_hint_is_a_key_every_terminal_delivers() {
        let map = Keymap::default();
        let chords = map.chords_for(Context::Composer, Action::InsertNewline);
        assert_eq!(hint_key(&chords, false), Some(k("ctrl+j")));
    }

    /// The picker is modal: no `Global` key leaks into it, and a user cannot bind a plain letter
    /// there (the letters are the search box).
    #[test]
    fn popup_and_confirm_are_modal_and_popup_takes_typing() {
        let map = Keymap::default();
        assert_eq!(map.lookup(Context::Popup, k("tab")), None);
        assert_eq!(map.lookup(Context::Popup, k("f2")), None);
        assert_eq!(map.lookup(Context::Popup, k("alt+1")), None);
        assert_eq!(map.lookup(Context::Confirm, k("tab")), None);
        assert_eq!(map.lookup(Context::Confirm, k("q")), None);
        let (_, problems) = build(&[("popup", "x", "refresh")]);
        assert!(
            texts(&problems)
                .iter()
                .any(|t| t.contains("would stop typing")),
            "{problems:?}"
        );
        let (_, problems) = build(&[("confirm", "x", "confirm_yes")]);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn user_binding_overrides_adds_and_unbinds() {
        let (map, problems) = build(&[
            ("list", "ctrl+n", "down"),
            ("list", "j", "refresh"),
            ("list", "z", "unbound"),
            ("list", "q", "unbound"),
            ("composer", "ctrl+s", "send"),
        ]);
        assert!(map.lookup(Context::List, k("ctrl+n")) == Some(Action::Down));
        assert_eq!(
            map.lookup(Context::List, k("j")),
            Some(Action::Refresh),
            "override"
        );
        assert_eq!(
            map.lookup(Context::List, k("k")),
            Some(Action::Up),
            "rest of the preset stays"
        );
        assert_eq!(
            map.lookup(Context::List, k("q")),
            None,
            "unbound removes the key"
        );
        assert_eq!(
            map.lookup(Context::Composer, k("ctrl+s")),
            Some(Action::Send)
        );
        assert_eq!(
            map.lookup(Context::Composer, k("enter")),
            Some(Action::Send)
        );
        assert_eq!(map.user_bindings(), 5);
        // `x` was never bound: warned, not an error.
        assert!(
            problems.iter().all(|p| p.severity == Severity::Warning),
            "{problems:?}"
        );
        assert!(
            texts(&problems)
                .iter()
                .any(|t| t.contains("nothing to unbind")),
            "{problems:?}"
        );
        // The user's key leads the hint.
        assert_eq!(
            map.hint(Context::List, &[Action::Down]).as_deref(),
            Some("Ctrl+N")
        );
    }

    #[test]
    fn no_keys_section_gives_todays_keys() {
        let (map, problems) = build(&[]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.user_bindings(), 0);
        assert_eq!(
            map.hint(Context::List, &[Action::Up, Action::Down])
                .as_deref(),
            Some("Up/Down")
        );
        assert_eq!(
            map.hint(Context::List, &[Action::NextNetwork]).as_deref(),
            Some("Tab")
        );
        assert_eq!(
            map.hint(Context::Composer, &[Action::PageUp, Action::PageDown])
                .as_deref(),
            Some("PgUp/PgDn")
        );
    }

    #[test]
    fn unknown_action_suggests_the_closest() {
        let (map, problems) = build(&[("list", "ctrl+n", "pgae_down")]);
        assert_eq!(map.lookup(Context::List, k("ctrl+n")), None, "skipped");
        assert_eq!(
            texts(&problems),
            vec!["settings.toml line 1: [keys.list] ctrl+n: unknown action \"pgae_down\": did you mean \"page_down\"? Skipping this binding."]
        );
        let (_, p) = build(&[("list", "ctrl+n", "xyzzy-nothing-like-it")]);
        assert!(p[0].message.contains("docs/guide.md"), "{p:?}");
    }

    #[test]
    fn unparseable_key_is_reported() {
        let (_, problems) = build(&[("list", "ctrl+", "refresh"), ("list", "f99", "refresh")]);
        let t = texts(&problems);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t.iter().all(|t| t.contains("cannot read the key")), "{t:?}");
        assert!(t[0].contains("line"), "{t:?}");
    }

    #[test]
    fn same_key_twice_in_one_table_blames_the_later_line() {
        // Both spell Ctrl+N; TOML sees two different keys.
        let (map, problems) = build(&[("list", "ctrl+n", "down"), ("list", "Ctrl+N", "up")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].severity, Severity::Error);
        assert!(
            problems[0].message.contains("same key as \"ctrl+n\""),
            "{problems:?}"
        );
        assert_eq!(problems[0].line, Some(2));
        assert_eq!(map.lookup(Context::List, k("ctrl+n")), Some(Action::Down));
    }

    #[test]
    fn a_binding_that_stops_typing_is_refused() {
        for (ctx, key) in [
            ("filter", "x"),
            ("composer", "G"),
            ("composer", "space"),
            ("filter", "backspace"),
            ("popup", "a"),
        ] {
            let (map, problems) = build(&[(ctx, key, "refresh")]);
            assert_eq!(problems.len(), 1, "{ctx} {key}: {problems:?}");
            assert_eq!(problems[0].severity, Severity::Error);
            assert!(problems[0].message.contains("stop typing"), "{problems:?}");
            assert_eq!(map.user_bindings(), 0);
        }
        // A Ctrl or Alt key is fine, and a plain key in a non-text context is fine.
        let (_, p) = build(&[
            ("filter", "ctrl+x", "refresh"),
            ("composer", "alt+x", "refresh"),
            ("list", "x", "refresh"),
        ]);
        assert!(p.is_empty(), "{p:?}");
        // Unbinding a typed key is harmless: nothing to stop.
        let (_, p) = build(&[("filter", "x", "unbound")]);
        assert!(
            p.iter().all(|p| !p.message.contains("stop typing")),
            "{p:?}"
        );
    }

    #[test]
    fn a_common_key_works_where_its_action_applies_and_never_where_you_type() {
        // A context can unbind a common key for itself.
        let (map, problems) = build(&[("select", "j", "unbound")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.lookup(Context::Select, k("j")), None);
        assert_eq!(map.lookup(Context::List, k("j")), Some(Action::Down));
        let (map, _) = build(&[("select", "g", "unbound")]);
        assert_eq!(map.step(Context::Select, None, k("g")), Step::Unbound);
        assert_eq!(
            map.step(Context::List, None, k("g")),
            Step::Action(Action::Top)
        );
        let (map, problems) = build(&[("common", "w", "refresh"), ("common", "ctrl+y", "pin")]);
        assert!(problems.is_empty(), "{problems:?}");
        // Refresh applies in the chat list (it has a key there): z refreshes there.
        assert_eq!(map.lookup(Context::List, k("w")), Some(Action::Refresh));
        // In a box you type in, a printable common key types instead.
        assert_eq!(map.lookup(Context::Filter, k("w")), None);
        assert_eq!(map.lookup(Context::Composer, k("w")), None);
        // Where refresh means nothing (help), the key does nothing.
        assert_eq!(map.lookup(Context::Help, k("w")), None);
        // A context's own key wins over the common one.
        let (map, _) = build(&[("common", "p", "refresh")]);
        assert_eq!(map.lookup(Context::List, k("p")), Some(Action::Pin));
        // The tabs keys stay out of the overlays.
        let map = Keymap::default();
        assert_eq!(
            map.lookup(Context::List, k("tab")),
            Some(Action::NextNetwork)
        );
        for ctx in [Context::Help, Context::Popup, Context::Confirm] {
            assert_eq!(map.lookup(ctx, k("tab")), None, "{ctx:?}");
            assert_eq!(map.lookup(ctx, k("ctrl+f")), None, "{ctx:?}");
        }
    }

    #[test]
    fn the_editing_keys_work_in_every_box_and_nowhere_else() {
        let map = Keymap::default();
        for ctx in [Context::Filter, Context::Composer, Context::Popup] {
            for (key, want) in [
                ("ctrl+backspace", Action::DeleteWordBack),
                ("ctrl+w", Action::DeleteWordBack),
                ("ctrl+left", Action::WordLeft),
                ("delete", Action::DeleteForward),
                ("ctrl+u", Action::ClearLine),
            ] {
                assert_eq!(map.lookup(ctx, k(key)), Some(want), "{ctx:?} {key}");
            }
        }
        for ctx in [
            Context::List,
            Context::Select,
            Context::Help,
            Context::Panel,
        ] {
            assert_eq!(map.lookup(ctx, k("ctrl+w")), None, "{ctx:?}");
        }
        // The chat list and selection keep their own Home and End.
        assert_eq!(map.lookup(Context::List, k("home")), Some(Action::Top));
        assert_eq!(map.lookup(Context::Select, k("end")), Some(Action::Bottom));
        assert_eq!(
            map.hint(Context::Filter, &[Action::DeleteWordBack])
                .as_deref(),
            Some("Ctrl+W")
        );
    }

    #[test]
    fn the_old_global_table_names_its_new_name() {
        let (_, problems) = build(&[("global", "f6", "next_network")]);
        assert!(
            problems[0]
                .message
                .contains("[keys.global] is now [keys.common]"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_context_may_take_ctrl_c_while_another_key_quits_there() {
        // The chat list has another quit key here.
        let (map, problems) = build(&[("list", "ctrl+c", "refresh"), ("list", "q", "quit")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            map.lookup(Context::List, k("ctrl+c")),
            Some(Action::Refresh)
        );
        assert_eq!(map.lookup(Context::Select, k("ctrl+c")), Some(Action::Quit));
        // Message selection has no other quit key: Ctrl+C keeps quitting there, with an error.
        let (map, problems) = build(&[("select", "ctrl+c", "reply")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].severity, Severity::Error);
        assert!(
            problems[0].message.starts_with(
                "no key quits chatcli in [keys.select]: bind one to \"quit\" there or in [keys.common]."
            ) && problems[0].message.ends_with("Until then Ctrl+C does it there."),
            "{problems:?}"
        );
        assert_eq!(map.lookup(Context::Select, k("ctrl+c")), Some(Action::Quit));
        // Another quit key that reaches it settles that.
        let (map, problems) = build(&[("select", "ctrl+c", "reply"), ("common", "f10", "quit")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            map.lookup(Context::Select, k("ctrl+c")),
            Some(Action::Reply)
        );
        assert_eq!(map.lookup(Context::Select, k("f10")), Some(Action::Quit));
    }

    #[test]
    fn quit_must_reach_every_context() {
        // Unbound everywhere, with q left on the panel and the chat list.
        let (map, problems) = build(&[
            ("common", "ctrl+c", "unbound"),
            ("panel", "q", "quit"),
            ("list", "q", "quit"),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.starts_with(
                "no key quits chatcli in [keys.filter], [keys.composer], [keys.select], [keys.popup], [keys.confirm], [keys.help]:"
            ),
            "{problems:?}"
        );
        assert_eq!(map.lookup(Context::List, k("ctrl+c")), None);
        for ctx in [Context::Filter, Context::Composer, Context::Help] {
            assert_eq!(map.lookup(ctx, k("ctrl+c")), Some(Action::Quit), "{ctx:?}");
        }
        // A key that types a character does not reach the boxes you type in.
        let (map, problems) = build(&[("common", "ctrl+c", "unbound"), ("common", "Q", "quit")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.starts_with(
                "no key quits chatcli in [keys.filter], [keys.composer], [keys.popup]:"
            ),
            "{problems:?}"
        );
        assert_eq!(map.lookup(Context::Select, k("Q")), Some(Action::Quit));
        assert_eq!(
            map.lookup(Context::Composer, k("ctrl+c")),
            Some(Action::Quit)
        );
        // One that does not is enough on its own.
        let (_, problems) = build(&[("common", "ctrl+c", "unbound"), ("common", "f10", "quit")]);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn putting_one_way_out_back_never_takes_the_other() {
        // The popup's only back key is Ctrl+C and nothing quits there: Ctrl+C goes back to
        // quitting, so Esc must come back as back too.
        let (map, problems) = build(&[
            ("common", "ctrl+c", "unbound"),
            ("common", "esc", "unbound"),
            ("popup", "ctrl+c", "back"),
        ]);
        assert!(map.missing_exits().is_empty(), "{problems:?}");
        assert_eq!(map.lookup(Context::Popup, k("ctrl+c")), Some(Action::Quit));
        assert_eq!(map.lookup(Context::Popup, k("esc")), Some(Action::Back));
        // The other way round: Esc is the help's only quit key.
        let (map, problems) = build(&[
            ("common", "ctrl+c", "unbound"),
            ("common", "esc", "unbound"),
            ("help", "esc", "quit"),
        ]);
        assert!(map.missing_exits().is_empty(), "{problems:?}");
        assert_eq!(map.lookup(Context::Help, k("esc")), Some(Action::Back));
        assert_eq!(map.lookup(Context::Help, k("ctrl+c")), Some(Action::Quit));
        assert!(problems.iter().any(|p| p.message.contains("[keys.help]")));
    }

    #[test]
    fn back_must_keep_a_key_in_every_overlay() {
        // Esc is bound once, in common. Unbinding it in the popup table hides it there, which
        // leaves the popup with no way out.
        let (map, problems) = build(&[("popup", "esc", "unbound")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(map.lookup(Context::Popup, k("esc")), Some(Action::Back));
        let (map, problems) = build(&[("popup", "esc", "accept")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.starts_with(
                "no key backs out in [keys.popup]: bind one to \"back\" there or in [keys.common]."
            ) && problems[0]
                .message
                .ends_with("Until then Esc does it there."),
            "{problems:?}"
        );
        assert_eq!(map.lookup(Context::Popup, k("esc")), Some(Action::Back));
        // Esc may do something else there once another key backs out.
        let (map, problems) = build(&[("popup", "esc", "accept"), ("popup", "f9", "back")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.lookup(Context::Popup, k("esc")), Some(Action::Accept));
        // n answers a y/n question, so Esc may go there; help has only Esc.
        let (_, problems) = build(&[("confirm", "esc", "confirm_yes")]);
        assert!(problems.is_empty(), "{problems:?}");
        let (_, problems) = build(&[("help", "esc", "down")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let (map, problems) = build(&[
            ("confirm", "esc", "confirm_yes"),
            ("confirm", "n", "unbound"),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(map.lookup(Context::Confirm, k("esc")), Some(Action::Back));
        // A back key in common reaches the overlays too.
        let (_, problems) = build(&[("popup", "esc", "accept"), ("common", "f9", "back")]);
        assert!(problems.is_empty(), "{problems:?}");
        // Unbinding it everywhere leaves every overlay without one.
        let (map, problems) = build(&[("common", "esc", "unbound")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0]
                .message
                .starts_with("no key backs out in [keys.popup], [keys.help]:"),
            "{problems:?}"
        );
        assert_eq!(map.lookup(Context::Help, k("esc")), Some(Action::Back));
    }

    #[test]
    fn ambiguous_ctrl_keys_warn_but_apply() {
        let (map, problems) =
            build(&[("list", "ctrl+i", "refresh"), ("list", "ctrl+[", "refresh")]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|p| p.severity == Severity::Warning));
        assert_eq!(
            map.lookup(Context::List, k("ctrl+i")),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn unknown_table_is_reported_with_a_suggestion() {
        let (_, problems) = build(&[("lst", "x", "refresh")]);
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].message.contains("did you mean [keys.list]"),
            "{problems:?}"
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let (_, problems) = build(&[
            ("list", "ctrl+n", "lst_down"),
            ("filter", "x", "refresh"),
            ("list", "f99", "refresh"),
            ("nope", "x", "refresh"),
        ]);
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    #[test]
    fn validate_keys_reports_the_bindings_problems() {
        let mut s = Settings::default();
        assert!(validate_keys(&s).is_empty());
        s.keys = user(&[("list", "ctrl+n", "lst_down")]);
        let p = validate_keys(&s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].severity, Severity::Error);
        // The vim tables are gone with the vim preset.
        s.keys = user(&[("normal", "x", "refresh")]);
        assert!(validate_keys(&s)[0].message.starts_with("unknown table"));
    }

    #[test]
    fn help_is_modal_and_lists_every_context() {
        let map = Keymap::default();
        // Tab would switch network under the overlay if Global leaked through.
        assert_eq!(map.lookup(Context::Help, k("tab")), None);
        assert_eq!(map.lookup(Context::Help, k("?")), None);
        assert_eq!(map.lookup(Context::Help, k("esc")), Some(Action::Back));
        let sections = map.help_sections();
        let ctxs: Vec<_> = sections.iter().map(|s| s.0).collect();
        assert_eq!(
            ctxs,
            [
                Context::Common,
                Context::List,
                Context::Composer,
                Context::Select,
                Context::Confirm,
                Context::Help
            ],
            "a context with no keys of its own (they are all in common) has no section"
        );
        // Every built-in action is listed.
        let listed: Vec<Action> = sections
            .iter()
            .flat_map(|s| s.1.iter().map(|e| e.0))
            .collect();
        for (_, _, a) in &map.single {
            assert!(listed.contains(a), "{a:?} missing from help");
        }
    }

    #[test]
    fn rebinding_changes_help_sections() {
        let (map, _) = build(&[("list", "ctrl+r", "refresh"), ("list", "r", "unbound")]);
        let list = map
            .help_sections()
            .into_iter()
            .find(|s| s.0 == Context::List)
            .unwrap();
        let keys = &list.1.iter().find(|e| e.0 == Action::Refresh).unwrap().1;
        assert_eq!(keys, &vec!["Ctrl+R".to_string()]);
    }

    #[test]
    fn closest_and_levenshtein() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(closest("dowm", ["down", "quit"].into_iter()), Some("down"));
        assert_eq!(closest("zzzzzz", ["down", "quit"].into_iter()), None);
        assert_eq!(
            closest("q", ["quit"].into_iter()),
            None,
            "one letter is never a typo"
        );
    }

    /// Message selection keys: Esc from the message box (`back`; `select_messages` has no default
    /// key), and the per-message keys in the `select` table.
    #[test]
    fn selection_keys() {
        use Action::*;
        let def = Keymap::default();
        assert_eq!(def.lookup(Context::Composer, k("shift+up")), None);
        assert_eq!(def.lookup(Context::Composer, k("esc")), Some(Back));
        assert_eq!(def.lookup(Context::List, k("v")), None);
        for (key, want) in [
            ("up", Up),
            ("k", Up),
            ("down", Down),
            ("j", Down),
            ("pgup", PageUp),
            ("pgdn", PageDown),
            ("home", Top),
            ("end", Bottom),
            ("G", Bottom),
            ("esc", Back),
            ("r", Reply),
            ("e", React),
            ("c", Edit),
            ("d", Delete),
            ("u", OpenLink),
            ("?", Help),
        ] {
            assert_eq!(def.lookup(Context::Select, k(key)), Some(want), "{key}");
        }
        // Tab and Ctrl+C still work while selecting (Global fallback); help is modal.
        assert_eq!(def.lookup(Context::Select, k("tab")), Some(NextNetwork));
        assert_eq!(def.lookup(Context::Select, k("g")), Some(Top));
        // `cancel_reply` has no default key: Esc cancels through leave_composer.
        assert!(def.keys_for(Context::Composer, CancelReply).is_empty());
    }

    /// The `select` table takes user bindings, plain letters included (it is not a text box),
    /// and `select_messages` can be given a key in the message box.
    #[test]
    fn selection_keys_can_be_rebound() {
        let (map, problems) = build(&[
            ("select", "x", "react"),
            ("composer", "alt+up", "select_messages"),
        ]);
        assert!(problems.is_empty(), "{:?}", texts(&problems));
        assert_eq!(map.lookup(Context::Select, k("x")), Some(Action::React));
        assert_eq!(
            map.lookup(Context::Composer, k("alt+up")),
            Some(Action::SelectMessages)
        );
    }
}
