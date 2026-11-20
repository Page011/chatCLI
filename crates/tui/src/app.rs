//! All screen state and input handling. Pure: no terminal, no network. Every handler returns
//! the commands the caller must deliver to the backends, so tests drive the whole app by
//! calling methods and reading what comes back.

pub(crate) mod cmds;
pub(crate) mod everyday;
pub(crate) mod jump;
pub(crate) mod media;
pub(crate) mod mouse;
pub(crate) mod msg;
pub(crate) mod settings;
pub(crate) mod tour;

use std::collections::{HashMap, HashSet, VecDeque};

use chatcli_core::model::{
    Capabilities, ChatFlag, ChatKind, ChatMessage, ChatSummary, Command, Event, MessageState,
    Network, PersonRef, Status,
};
use chatcli_core::runtime::{StartupNotice, Toast, UiCtx};
use chatcli_core::settings::{
    ImageProtocol, KeySettings, LayoutSettings, NotificationSettings, PickerPlace, SettingEdit,
    Settings, Severity,
};
use chatcli_core::store::Store;
use chatcli_core::{limits, log, open};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::burst;
use crate::chats;
use crate::commands::{colon_query, CmdId};
use crate::composer::Composer;
use crate::emoji::{self, Picker, PickerFor};
use crate::input::TextInput;
use crate::keymap::{typed_char, Action, Context, KeyChord, Keymap, Step};
use crate::links;
use crate::media_view::MediaState;
use crate::notify::Fold;
use crate::older::Older;
use crate::palette::Palette;
use crate::popup::{Popup, PopupItem};
use crate::search::Search;
use crate::settings_view::SettingsPage;
use crate::text::{clean_line, clean_msg, clean_paste, Clock, Fmt};
use crate::theme::Theme;
use crate::tour::Tour;
use crate::typing::Typers;
use crate::ui;
use media::Pending;
use msg::Target;
use settings::about_lines;

/// Commands to deliver, each to the backend of the named network.
pub type Out = Vec<(Network, Command)>;

const MAX_NOTICES: usize = 50;
/// How long the newest notice stays in the footer. A fixed error must not stay red for hours
/// under a tab that is LIVE again; the log keeps the full history.
const NOTICE_TTL_MS: i64 = 30_000;
/// Width under which only one pane (list or conversation) fits on screen.
pub(crate) const NARROW: u16 = 60;
const WHEEL_LINES: isize = 3;
/// A draft or paste never grows past this: every edit walks the whole draft, and a send over the
/// network limit is refused anyway (Teams 80 KB, WhatsApp 60,000 characters).
const DRAFT_MAX_BYTES: usize = 1 << 20;
/// A previewed chat is requested from the backend only after the selection rests this long, so
/// holding Down through 50 chats sends none of them (a Teams open costs a Graph call).
pub(crate) const SETTLE_MS: i64 = 250;
/// A chat previewed this long counts as read.
pub(crate) const READ_AFTER_MS: i64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Filter,
    Composer,
    /// One message of the open chat is highlighted and keys act on it (issue 11).
    Select,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub network: Network,
    pub text: String,
    pub is_error: bool,
    /// Clock time when it arrived.
    pub at_ms: i64,
    /// About the app, not one network (settings problems, key preset): shown without a
    /// network prefix.
    pub global: bool,
}

/// What `Event::Receipts` last said about one chat: the `ts` of your newest message everyone
/// else has read, and has received. None = unknown (no tick, no crosses).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Receipts {
    pub read: Option<i64>,
    pub delivered: Option<i64>,
    /// Your messages at or before this carry no state (sent before delivery was tracked).
    pub since: Option<i64>,
}

pub(crate) struct NetState {
    pub network: Network,
    pub status: Status,
    /// Newest first.
    pub chats: Vec<ChatSummary>,
    /// Per chat id, oldest first.
    pub messages: HashMap<String, Vec<ChatMessage>>,
    pub selected: Option<String>,
    /// The chat shown in the message pane: previewed from the list or opened for replying.
    pub open: Option<String>,
    /// `open` is only previewed: not counted as read until Enter, a send, or `READ_AFTER_MS`.
    pub previewing: bool,
    /// Chat to request from the backend, and when (clock ms): the settle timer.
    pub settle: Option<(String, i64)>,
    /// When a previewed chat counts as read (clock ms).
    pub read_due: Option<i64>,
    /// The chat the backend was last asked to open, so the same one is never asked twice in a row.
    pub requested: Option<String>,
    pub focus: Focus,
    pub filter: TextInput,
    /// Unsent text and cursor per chat id, so switching chats never sends it to the wrong place.
    pub drafts: HashMap<String, Composer>,
    /// Message pane lines hidden below the view (0 = newest visible).
    pub scroll: usize,
    /// The highlighted message of the open chat while `focus` is `Select`. Kept by id, so an
    /// incoming message, a resize or a wheel scroll never moves it.
    pub sel_msg: Option<String>,
    /// A chat entered from the list before any of its messages were loaded: the message box got
    /// the focus for now, and the newest message is selected when they arrive. Any key, click or
    /// paste in between cancels it.
    pub(crate) select_on_load: Option<String>,
    /// Where the selection was before each big jump (`G`, `g`, `H`, `M`, `L`), newest last, as
    /// (chat id, message id). `jump_back` (Ctrl+O) walks back through it.
    pub(crate) jumps: Vec<(String, String)>,
    /// Per chat id: the held message after which newer messages are left out, because a jump far
    /// back (a search hit) loaded only a window around its message. Moving down to it reads the
    /// next page from the store (`app/jump.rs`).
    pub(crate) gaps: HashMap<String, (i64, String)>,
    /// A reply or an edit being composed, per chat id (shown above the message box).
    pub(crate) targets: HashMap<String, Target>,
    /// What the backend can do now. All false until its first `Capabilities` event.
    pub caps: Capabilities,
    /// Group members per chat id, for `@` completion.
    pub members: HashMap<String, Vec<PersonRef>>,
    /// Chats whose members were asked for this session (once each: a Graph call per ask).
    members_asked: HashSet<String>,
    /// Profile photos by person id: present = asked this session, None = pending or no photo.
    pub(crate) avatars: HashMap<String, Option<std::path::PathBuf>>,
    /// Downloads and previews the user asked for, per attachment.
    pub media: MediaState,
    /// A file chosen for the next message, per chat id (the chip above the message box).
    pub(crate) pending: HashMap<String, Pending>,
    /// Upload progress (done, total bytes) per client id of a send in flight.
    pub progress: HashMap<String, (u64, u64)>,
    /// Loading older history, per chat id (absent = idle).
    pub(crate) older: HashMap<String, Older>,
    /// Where the last older-history request started, per chat id: the oldest message held when it
    /// was asked. An answer for another start (a jump replaced the window meanwhile) is stale.
    pub(crate) older_from: HashMap<String, (i64, String)>,
    /// Who is typing now, per chat id (WhatsApp only).
    pub(crate) typing: HashMap<String, Typers>,
    /// How far everyone else has read and received your messages, per chat id (the last
    /// `Event::Receipts`; WhatsApp only). The message pane ticks your newest read message and
    /// crosses every sent message past `delivered`.
    pub(crate) receipts: HashMap<String, Receipts>,
    /// When "composing" was last sent for a chat (clock ms): the 5 s throttle.
    pub(crate) typed_at: HashMap<String, i64>,
    /// Chats where "composing" went out and `paused` has not: owed when the message is sent.
    pub(crate) composing: HashSet<String>,
    /// `show_hidden`: the chats you hid are listed too.
    pub show_hidden: bool,
    /// The clock as of the last `tick`: mutes that ended stop counting.
    pub(crate) now_ms: i64,
    /// First chat shown in the sidebar.
    pub list_top: usize,
    /// Sidebar rows `list_top` was last fitted to: the viewport follows the selection only when
    /// the selection or this changes, so a wheel-scrolled sidebar stays put on live updates.
    rows_seen: usize,
}

impl NetState {
    fn new(network: Network, now_ms: i64) -> NetState {
        NetState {
            network,
            status: Status::Starting,
            chats: Vec::new(),
            messages: HashMap::new(),
            selected: None,
            open: None,
            previewing: false,
            settle: None,
            read_due: None,
            requested: None,
            focus: Focus::List,
            filter: TextInput::default(),
            drafts: HashMap::new(),
            scroll: 0,
            sel_msg: None,
            select_on_load: None,
            jumps: Vec::new(),
            gaps: HashMap::new(),
            targets: HashMap::new(),
            caps: Capabilities::default(),
            members: HashMap::new(),
            members_asked: HashSet::new(),
            avatars: HashMap::new(),
            media: MediaState::default(),
            pending: HashMap::new(),
            progress: HashMap::new(),
            older: HashMap::new(),
            older_from: HashMap::new(),
            typing: HashMap::new(),
            receipts: HashMap::new(),
            typed_at: HashMap::new(),
            composing: HashSet::new(),
            show_hidden: false,
            now_ms,
            list_top: 0,
            rows_seen: 0,
        }
    }

    /// The open chat is a group, so `@` completion applies.
    pub fn open_is_group(&self) -> bool {
        self.open.as_ref().is_some_and(|id| {
            self.chats
                .iter()
                .any(|c| &c.id == id && c.kind == ChatKind::Group)
        })
    }

    /// The reply or edit being composed in the open chat.
    pub(crate) fn target(&self) -> Option<&Target> {
        self.open.as_ref().and_then(|id| self.targets.get(id))
    }

    /// The file chosen for the open chat's next message.
    pub(crate) fn attachment(&self) -> Option<&Pending> {
        self.open.as_ref().and_then(|id| self.pending.get(id))
    }

    /// Lines above the text in the message box: the reply or edit line and the file chip.
    pub(crate) fn extra_rows(&self) -> u16 {
        u16::from(self.target().is_some()) + u16::from(self.attachment().is_some())
    }

    /// The tab's unread total: muted and hidden chats are left out.
    pub fn unread(&self) -> u32 {
        self.chats
            .iter()
            .filter(|c| chats::counts_toward_total(c, self.now_ms))
            .map(|c| c.unread)
            .sum()
    }

    /// The sign-in panel replaces the chat view while the network is not signed in.
    pub fn panel(&self) -> bool {
        matches!(
            self.status,
            Status::NeedsLogin { .. } | Status::LoggingIn | Status::Qr { .. }
        )
    }

    /// Indexes into `chats` that pass the filter. Hidden chats only with `show_hidden`.
    pub fn visible(&self) -> Vec<usize> {
        let query = self.filter.text();
        (0..self.chats.len())
            .filter(|&i| self.show_hidden || !self.chats[i].flags.hidden)
            .filter(|&i| crate::list::matches(&self.chats[i].title, query))
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
        self.composer().map_or("", |c| c.text())
    }

    /// The open chat's draft with its cursor.
    pub(crate) fn composer(&self) -> Option<&Composer> {
        self.open.as_ref().and_then(|id| self.drafts.get(id))
    }

    /// Zeroes the badge and tells the backend.
    fn mark_read(&mut self, chat_id: &str, out: &mut Out) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.unread = 0;
        }
        out.push((
            self.network,
            Command::MarkRead {
                chat_id: chat_id.to_string(),
            },
        ));
    }

    fn composer_mut(&mut self) -> Option<&mut Composer> {
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
    /// Set when a pane may have come back on screen: `fire_timers` then refills the picture
    /// downloads that stalled while it was hidden.
    auto_kick: bool,
    next_client: u64,
    quit: bool,
    keys: Keymap,
    /// The live settings: the settings screen changes this copy and every setting follows it.
    /// Its `[keys]` is kept so `/keys vim` can rebuild the keymap for the other preset.
    settings: Settings,
    /// First key of a sequence (vim `g`), with the context it was typed in. No timer.
    pending: Option<(Context, KeyChord)>,
    /// Help overlay: `Some(first visible line)` while open.
    help: Option<usize>,
    /// The `:shortcode` suggestions above the message box.
    popup: Option<Popup>,
    /// Set by anything that edits or moves in the message box during one key press: the popup
    /// is then recomputed from the text before the cursor. A key that did neither closes it.
    popup_alive: bool,
    /// The last key closed the suggestion list (Esc, or any key that is not an edit): an event
    /// (members arriving) must not open it again until the draft is edited.
    popup_closed: bool,
    /// The emoji picker, modal while open.
    picker: Option<Picker>,
    /// Recent emoji, newest first (at most `emoji::RECENT_MAX`).
    recents: Vec<String>,
    /// The TUI's own store connection: recent emoji is its only write.
    store: Option<Store>,
    /// "Open this link?" or "Delete for everyone?" while it waits for an answer.
    prompt: Option<Prompt>,
    /// Links the user confirmed; the run loop opens them (App stays pure).
    open_urls: Vec<String>,
    /// When a click last raised a y/n question (a double-click must not answer it).
    prompt_opened_ms: i64,
    /// Text `y` asked the run loop to put on the clipboard (`take_copy`).
    copy: Option<(Network, String)>,
    /// Attachments: the path prompt, queues for the run loop, thumbnail settings (issues 16-37).
    media: media::Ctl,
    /// Colours by role (issue 22).
    theme: Theme,
    /// `[layout]` (issue 23): sidebar width, density, previews; `fmt` is its time and date part.
    layout: LayoutSettings,
    fmt: Fmt,
    /// The search box over the store, while it is open (issue 33).
    search: Option<Search>,
    /// The text of the last search a hit was taken from (Enter): `n` and `N` step through its
    /// matches in the open chat.
    last_query: Option<String>,
    /// `[notifications]` and what toasts need (issue 24).
    notif: NotificationSettings,
    /// The terminal window has focus. False until a focus event says so.
    focused: bool,
    /// Clock ms when the TUI started: older messages are catch-up, not news.
    started_ms: i64,
    /// Per-chat 10 s toast fold.
    folds: HashMap<(Network, String), Fold>,
    toasts: Vec<Toast>,
    /// `[whatsapp] send_typing`.
    send_typing: bool,
    /// The `/` command bar, while it is open.
    palette: Option<Palette>,
    /// The settings screen, while it is open.
    settings_page: Option<SettingsPage>,
    /// `chatcli --demo`: the sign-in panel says so, and `/tour` is available.
    demo: bool,
    /// NO_COLOR is set: a theme change keeps the colours off.
    no_colour: bool,
    /// The About page's lines (version, folders), filled by `with_ctx`.
    about: Vec<(&'static str, String)>,
    /// What is wrong with `settings.toml`, one line each: the About page lists them.
    file_problems: Vec<String>,
    /// `settings.toml` edits the run loop has not saved yet.
    setting_edits: Vec<SettingEdit>,
    /// The guided tour, while it plays or shows its last step.
    tour: Option<Tour>,
}

/// A question on the footer line, waiting for its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prompt {
    /// Several links in the message: press 1-9.
    Pick(Vec<String>),
    /// One link: its host is shown, y opens it.
    Confirm(String),
    /// `Delete for everyone? y/n` for one of the user's own messages.
    Delete { chat_id: String, message_id: String },
    /// `Mark 5 chats read? y/n` (the `mark_all_read` action).
    MarkAllRead { count: usize },
    /// `Sign out of Teams? y/n` (the `/signout` command).
    SignOut(Network),
    /// `Reset every key to its default? y/n` (the keys page).
    ResetKeys,
    /// Before a file goes out: an executable type (any network) and a Teams document (it goes
    /// to the sender's OneDrive and the chat may not have access).
    SendFile {
        chat_id: String,
        name: String,
        executable: bool,
        onedrive: bool,
    },
}

/// The notice for a draft over the network's text limit: what is wrong, then what to do.
/// The emoji whose shortcode matches `query` (typed after a colon), as popup lines.
pub(crate) fn emoji_items(query: &str) -> Vec<PopupItem> {
    emoji::suggest(query, emoji::SUGGESTIONS)
        .into_iter()
        .map(|s| PopupItem {
            label: s.emoji.to_string(),
            detail: format!(":{}:", s.shortcode),
            insert: s.emoji.to_string(),
            person: None,
            command: None,
        })
        .collect()
}

fn too_long(n: Network) -> String {
    match n {
        Network::Teams => format!(
            "MESSAGE TOO LONG for Teams (limit {} KB): shorten it or send it in parts.",
            limits::TEAMS_TEXT_MAX_BYTES / 1024
        ),
        Network::WhatsApp => format!(
            "MESSAGE TOO LONG for WhatsApp (limit {} characters): shorten it or send it in parts.",
            limits::WHATSAPP_TEXT_MAX_CHARS
        ),
    }
}

/// Store `meta` key: the network of the tab used last, so the next start opens on it.
const LAST_TAB: &str = "tui.last_tab";

fn idx(n: Network) -> usize {
    match n {
        Network::Teams => 0,
        Network::WhatsApp => 1,
    }
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
    if let Some(q) = m.reply_to.as_mut() {
        q.sender = clean_line(&q.sender);
        q.preview = clean_line(&q.preview);
    }
    for r in &mut m.reactions {
        r.emoji = clean_line(&r.emoji);
    }
    for p in &mut m.mentions {
        p.name = clean_line(&p.name);
    }
    // A file name is the sender's text: one clean line, whatever it holds.
    for a in &mut m.attachments {
        a.name = a.name.as_deref().map(clean_line);
        a.mime = a.mime.as_deref().map(clean_line);
    }
}

/// History from the backend replaces what the TUI holds from its oldest message onward (an
/// empty batch replaces nothing). Kept: older messages (loaded through `OlderMessages`), and local sends the backend has not
/// answered yet (Pending, or Failed with no copy in the batch), so reopening a chat never
/// drops a message or its NOT SENT reason.
fn merge_history(held: Vec<ChatMessage>, batch: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let oldest = batch.first().map(|m| (m.ts, m.id.clone()));
    let in_batch: std::collections::HashSet<String> = batch.iter().map(|m| m.id.clone()).collect();
    let mut out: Vec<ChatMessage> = held
        .into_iter()
        .filter(|m| {
            if in_batch.contains(&m.id) {
                return false;
            }
            let unresolved = crate::older::unconfirmed(m);
            let older = oldest
                .as_ref()
                .is_none_or(|(ts, id)| (m.ts, m.id.as_str()) < (*ts, id.as_str()));
            unresolved || older
        })
        .collect();
    out.extend(batch);
    crate::older::sort_messages(&mut out);
    out
}

/// Insert or replace by id, keeping the list ordered by time.
fn upsert_message(list: &mut Vec<ChatMessage>, m: ChatMessage) {
    match list.iter_mut().find(|x| x.id == m.id) {
        Some(slot) => *slot = m,
        None => list.push(m),
    }
    crate::older::sort_messages(list);
}

impl App {
    /// Default settings: today's keys.
    pub fn new(clock: Clock) -> App {
        App::with_keys(clock, KeySettings::default())
    }

    fn with_keys(clock: Clock, user_keys: KeySettings) -> App {
        let settings = Settings {
            keys: user_keys,
            ..Settings::default()
        };
        App {
            nets: [
                NetState::new(Network::Teams, clock.now_ms),
                NetState::new(Network::WhatsApp, clock.now_ms),
            ],
            active: Network::Teams,
            notices: VecDeque::new(),
            size: Rect::new(0, 0, 80, 24),
            clock,
            auto_kick: false,
            next_client: 0,
            quit: false,
            keys: Keymap::build(&settings.keys).0,
            settings,
            pending: None,
            help: None,
            popup: None,
            popup_alive: false,
            popup_closed: false,
            picker: None,
            recents: Vec::new(),
            store: None,
            prompt: None,
            open_urls: Vec::new(),
            prompt_opened_ms: i64::MIN / 2,
            copy: None,
            media: media::Ctl::default(),
            theme: Theme::default(),
            layout: LayoutSettings::default(),
            fmt: Fmt::default(),
            search: None,
            last_query: None,
            notif: NotificationSettings::default(),
            focused: false,
            started_ms: clock.now_ms,
            folds: HashMap::new(),
            toasts: Vec::new(),
            send_typing: false,
            palette: None,
            settings_page: None,
            demo: false,
            no_colour: false,
            about: vec![
                (
                    "Version",
                    concat!("chatcli ", env!("CARGO_PKG_VERSION")).to_string(),
                ),
                ("Data folder", "-".to_string()),
                ("Log file", "-".to_string()),
                ("Settings file", "-".to_string()),
            ],
            file_problems: Vec::new(),
            setting_edits: Vec::new(),
            tour: None,
        }
    }

    /// App for `settings`, its `[keys]` bindings over the built-in keys.
    pub fn with_settings(clock: Clock, settings: &Settings) -> App {
        let mut app = App::with_keys(clock, settings.keys.clone());
        app.settings = settings.clone();
        app.apply_view(settings);
        app
    }

    /// Everything a setting changes at once except the keymap and the backends: start-up and
    /// the settings screen both come through here.
    fn apply_view(&mut self, settings: &Settings) {
        // `image_protocol = "off"` is the same as no inline images: the marker only. NO_COLOR
        // removes the thumbnails too: they are pictures in colour, the chips stay.
        self.media.inline = settings.media.inline_images
            && settings.media.image_protocol != ImageProtocol::Off
            && !self.no_colour;
        self.media.max_file_bytes = settings.media.max_file_mb * 1024 * 1024;
        self.theme = Theme::from_settings(&settings.theme, self.no_colour).0;
        self.layout = settings.layout.clone();
        self.fmt = Fmt {
            time: settings.layout.time_format,
            date: settings.layout.date_order,
        };
        self.notif = settings.notifications.clone();
        self.send_typing = settings.whatsapp.send_typing;
        self.media.auto = settings.whatsapp.download_images;
    }

    /// The app as `run` builds it: settings, `--keys`, and one start-up notice that sums up
    /// the settings problems (the notice bar holds one line; `chatcli settings check` lists all).
    pub fn with_ctx(clock: Clock, ctx: &UiCtx) -> App {
        let mut lines: Vec<StartupNotice> = ctx.startup_notices.clone();
        lines.extend(
            crate::validate_settings(&ctx.settings)
                .into_iter()
                .map(|p| StartupNotice {
                    text: p.text(),
                    is_error: p.severity == Severity::Error,
                }),
        );
        let mut app = App::with_settings(clock, &ctx.settings);
        app.demo = ctx.demo;
        // NO_COLOR (any non-empty value) removes every colour.
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            app.no_colour = true;
            app.apply_view(&ctx.settings);
        }
        app.about = about_lines(ctx);
        app.file_problems = lines.iter().map(|l| l.text.clone()).collect();
        app.recents = ctx
            .store
            .recent_emoji(emoji::RECENT_MAX as u32)
            .unwrap_or_else(|e| {
                log::error("tui", &format!("could not read recent emoji: {e}"));
                Vec::new()
            });
        // Start on the tab used last.
        if let Some(n) = ctx
            .store
            .meta(LAST_TAB)
            .ok()
            .flatten()
            .and_then(|k| Network::from_key(&k))
        {
            app.active = n;
        }
        app.store = Some(ctx.store.clone());
        app.media.root = Some(ctx.media.root().to_path_buf());
        app.media.max_file_bytes = ctx.media.max_file_bytes();
        if let Some(first) = lines.first() {
            let more = lines.len() - 1;
            let text = if more == 0 {
                first.text.clone()
            } else {
                format!("{} (+{more} more: run chatcli settings check)", first.text)
            };
            let is_error = lines.iter().any(|l| l.is_error);
            app.push_global_notice(&text, is_error);
        }
        if ctx.tour {
            app.start_tour();
        }
        app
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub(crate) fn keys(&self) -> &Keymap {
        &self.keys
    }

    pub(crate) fn help_open(&self) -> Option<usize> {
        self.help
    }

    /// The `:shortcode` suggestions while they are open.
    pub(crate) fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    pub(crate) fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    pub(crate) fn recents(&self) -> &[String] {
        &self.recents
    }

    pub(crate) fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// Links the user confirmed since the last call. The run loop opens each one in the
    /// browser (core's `open_url` checks it again) and shows the error if that fails.
    pub fn take_open_urls(&mut self) -> Vec<String> {
        std::mem::take(&mut self.open_urls)
    }

    /// Text to put on the clipboard, once (`y` in message selection). The run loop copies it and
    /// answers with `copy_done`.
    pub fn take_copy(&mut self) -> Option<(Network, String)> {
        self.copy.take()
    }

    /// How the copy went: the notice line says so.
    pub fn copy_done(&mut self, n: Network, result: Result<&str, String>) {
        match result {
            Ok(text) => self.push_notice(n, text, false),
            Err(e) => self.push_notice(n, &e, true),
        }
    }

    /// True while something covers the message box, so it must not draw the terminal cursor.
    pub(crate) fn overlay_open(&self) -> bool {
        self.help.is_some()
            || self.picker.is_some()
            || self.prompt.is_some()
            || self.media.attach.is_some()
            || self.search.is_some()
            || self.palette.is_some()
            || self.settings_page.is_some()
    }

    /// The held first key of a sequence, as text for the footer.
    pub(crate) fn pending_text(&self) -> Option<String> {
        self.pending.map(|(_, k)| k.display())
    }

    /// Which key table applies right now.
    pub(crate) fn context(&self) -> Context {
        if self.help.is_some() {
            return Context::Help;
        }
        if self.prompt.is_some() {
            return Context::Confirm;
        }
        if self.picker.is_some()
            || self.media.attach.is_some()
            || self.search.is_some()
            || self.palette.is_some()
            || self.settings_page.is_some()
        {
            return Context::Popup;
        }
        let ns = self.active();
        if ns.panel() {
            return Context::Panel;
        }
        match ns.focus {
            Focus::List => Context::List,
            Focus::Filter => Context::Filter,
            Focus::Composer => Context::Composer,
            Focus::Select => Context::Select,
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.size = Rect::new(0, 0, width, height);
        self.relayout();
        self.sync_read_timers();
        self.scroll_help(0);
        if self.palette.is_some() {
            self.palette_fit();
        }
        for n in Network::ALL {
            // A resize rewraps the messages: keep the highlighted one in view.
            if self.ns(n).focus == Focus::Select {
                self.ensure_sel_visible(n);
            }
        }
    }

    /// Makes `n` the active tab. What is on screen changes, so read timers follow.
    /// The tab you chose: shown at once and remembered for the next start (`LAST_TAB`).
    fn set_active(&mut self, n: Network) {
        self.active = n;
        self.sync_read_timers();
        if let Some(store) = &self.store {
            if let Err(e) = store.set_meta(LAST_TAB, Some(n.key())) {
                log::error("tui", &format!("last tab not saved: {e}"));
            }
        }
    }

    /// The networks in tab order: the signed-in ones first, then those waiting for a sign-in,
    /// each group in the fixed order of `Network::ALL`. The tabs, Tab and Alt+1 / Alt+2 follow it.
    pub(crate) fn tab_order(&self) -> Vec<Network> {
        let mut order = Network::ALL.to_vec();
        order.sort_by_key(|n| {
            matches!(
                self.ns(*n).status,
                Status::NeedsLogin { .. } | Status::LoggingIn | Status::Qr { .. }
            )
        });
        order
    }

    /// True while network `n`'s message pane is on screen: its tab is active, no sign-in panel
    /// replaces the chats, and the terminal is wide enough for both panes (a narrow one shows the
    /// conversation only while the composer has focus). A chat counts as read only while this
    /// holds, because WhatsApp turns a read into read receipts the sender sees.
    pub(crate) fn convo_visible(&self, n: Network) -> bool {
        n == self.active
            && !self.ns(n).panel()
            && (self.size.width >= NARROW || self.wants_convo(n))
    }

    /// A previewed chat's read timer runs only while its pane is on screen: stopped when the tab
    /// or the pane goes away (the chat stays unread), restarted from zero when it comes back.
    fn sync_read_timers(&mut self) {
        let now = self.clock.now_ms;
        for n in Network::ALL {
            let visible = self.convo_visible(n);
            self.auto_kick |= visible;
            let ns = self.ns_mut(n);
            if !ns.previewing || ns.open.is_none() {
                continue;
            }
            if !visible {
                ns.read_due = None;
            } else if ns.read_due.is_none() {
                ns.read_due = Some(now + READ_AFTER_MS);
            }
        }
    }

    /// Advances the clock. True when the minute changed, so times on screen need a redraw.
    pub fn tick(&mut self, now_ms: i64) -> bool {
        let before = self.clock.minute();
        let notice_before = self.current_notice().map(|n| n.at_ms);
        self.clock.now_ms = now_ms;
        for ns in &mut self.nets {
            ns.now_ms = now_ms;
        }
        // "is typing" ends on this tick, no timer of its own.
        let typing_ended = self.prune_typing();
        let notice_changed = self.current_notice().map(|n| n.at_ms) != notice_before;
        if notice_changed {
            self.relayout();
        }
        self.clock.minute() != before || notice_changed || typing_ended
    }

    /// Earliest clock time (ms) a settle or read timer is due, so the run loop sleeps exactly
    /// that long and not at all while nothing is pending.
    pub fn next_deadline(&self) -> Option<i64> {
        self.nets
            .iter()
            .flat_map(|ns| [ns.settle.as_ref().map(|s| s.1), ns.read_due])
            .flatten()
            .chain(self.toast_deadline())
            .chain(self.tour_deadline())
            .min()
    }

    /// True when `fire_timers` has picture downloads to refill after a pane came back.
    pub fn refill_due(&self) -> bool {
        self.auto_kick
    }

    /// Runs the timers that are due at the clock time set by `tick`: requests a chat that has
    /// rested for `SETTLE_MS`, and marks one read after `READ_AFTER_MS` of preview.
    pub fn fire_timers(&mut self) -> Out {
        let now = self.clock.now_ms;
        self.fire_toasts();
        let mut out = self.tour_tick();
        let visible = Network::ALL.map(|n| self.convo_visible(n));
        let mut counted = Vec::new();
        for (ns, visible) in self.nets.iter_mut().zip(visible) {
            if ns.settle.as_ref().is_some_and(|s| now >= s.1) {
                if let Some((id, _)) = ns.settle.take() {
                    ns.requested = Some(id.clone());
                    out.push((ns.network, Command::OpenChat { chat_id: id }));
                }
            }
            if ns.read_due.is_some_and(|d| now >= d) {
                ns.read_due = None;
                // Not on screen (other tab, narrow list): never read. The timer restarts when
                // the pane is shown again (`sync_read_timers`).
                if let Some(id) = ns.open.clone().filter(|_| ns.previewing && visible) {
                    ns.previewing = false;
                    counted.push(ns.network);
                    // Only when the chat has something unread: browsing read chats costs no call.
                    if ns.chats.iter().any(|c| c.id == id && c.unread > 0) {
                        ns.mark_read(&id, &mut out);
                    }
                }
            }
        }
        for n in counted {
            out.extend(self.auto_images(n));
        }
        // A pane that came back on screen: repeating is safe, `auto_tried` and the cap hold.
        if std::mem::take(&mut self.auto_kick) {
            out.extend(self.auto_images(self.active));
        }
        out
    }

    /// The footer's height depends on what it shows, so the visible chat rows can change. The
    /// sidebar follows the selection only when the row count really changed; otherwise it is
    /// just kept in range, so a live update does not undo the user's wheel scrolling.
    fn relayout(&mut self) {
        for n in Network::ALL {
            self.refit(n);
        }
        // The picker's grid can change size too (a resize, a new `emoji_picker` place): keep its
        // selected cell on screen.
        if let Some(g) = self.picker_grid() {
            if let Some(p) = self.picker.as_mut() {
                p.keep_visible(g.cols, g.rows);
            }
        }
    }

    /// Fits the sidebar viewport after the geometry may have changed. A selection that was in
    /// view stays in view; a sidebar the user scrolled away from the selection keeps its place.
    fn refit(&mut self, n: Network) {
        let rows = self.list_rows(n);
        let ns = self.ns(n);
        if ns.rows_seen == rows {
            return self.clamp_top(n);
        }
        let pos = ns.sel_pos(&ns.visible());
        if pos >= ns.list_top && pos < ns.list_top + ns.rows_seen {
            self.ensure_visible(n);
        } else {
            self.ns_mut(n).rows_seen = rows;
            self.clamp_top(n);
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
            global: false,
        });
        self.relayout();
    }

    /// A notice about the app itself, shown without a network name.
    pub fn push_global_notice(&mut self, text: &str, is_error: bool) {
        self.push_notice(self.active, text, is_error);
        if let Some(n) = self.notices.back_mut() {
            n.global = true;
        }
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

    #[cfg(test)]
    pub(crate) fn clock(&self) -> &Clock {
        &self.clock
    }

    // ---- geometry shared with drawing ----

    /// On a narrow terminal the conversation replaces the list while the composer has focus.
    fn wants_convo(&self, n: Network) -> bool {
        let ns = self.ns(n);
        matches!(ns.focus, Focus::Composer | Focus::Select) && ns.open.is_some()
    }

    fn layout_for(&self, n: Network) -> ui::Layout {
        self.layout_in_for(n, self.size)
    }

    fn layout_in_for(&self, n: Network, area: Rect) -> ui::Layout {
        let ns = self.ns(n);
        // The tour's caption takes the top row.
        let area = tour::trim_top(area, u16::from(self.tour.is_some()));
        ui::layout(
            area,
            ui::footer_height(self, area),
            self.wants_convo(n),
            ns.focus == Focus::Filter || !ns.filter.is_empty(),
            ns.draft(),
            self.box_rows(n),
            self.layout.sidebar_width,
            self.picker_open_as(n, PickerPlace::Box),
        )
    }

    /// Rows the message box adds to the draft's: the reply or edit line, the file chip, and the
    /// inline emoji strip while it is open on this tab.
    fn box_rows(&self, n: Network) -> u16 {
        let strip = self.picker_open_as(n, PickerPlace::Inline);
        self.ns(n).extra_rows() + u16::from(strip)
    }

    /// True while tab `n` shows the picker at `place`: the layout makes room for `box` and
    /// `inline`.
    fn picker_open_as(&self, n: Network, place: PickerPlace) -> bool {
        n == self.active && self.picker_place() == Some(place)
    }

    /// Where the open picker goes: the `[layout] emoji_picker` place for the message box's
    /// picker, the middle for a reaction. None when no picker is open.
    pub(crate) fn picker_place(&self) -> Option<PickerPlace> {
        Some(match self.picker.as_ref()?.purpose {
            PickerFor::Compose => self.layout.emoji_picker,
            PickerFor::React { .. } => PickerPlace::Popup,
        })
    }

    /// Layout of the active tab for the frame being drawn.
    pub(crate) fn layout_in(&self, area: Rect) -> ui::Layout {
        self.layout_in_for(self.active, area)
    }

    /// Message pane (width, height) as it is when the conversation is on screen. The pane is
    /// shorter while the message box has grown for a long draft.
    fn convo_size(&self, n: Network) -> (usize, usize) {
        let area = tour::trim_top(self.size, u16::from(self.tour.is_some()));
        let m = ui::layout(
            area,
            ui::footer_height(self, area),
            true,
            false,
            self.ns(n).draft(),
            self.box_rows(n),
            self.layout.sidebar_width,
            self.picker_open_as(n, PickerPlace::Box),
        )
        .messages;
        (m.width as usize, m.height as usize)
    }

    fn convo_lines(&self, n: Network) -> usize {
        ui::message_lines(self.ns(n), &self.look(), self.convo_size(n).0).len()
    }

    fn scroll_by(&mut self, n: Network, delta: isize) {
        let max = self.convo_lines(n).saturating_sub(self.convo_size(n).1) as isize;
        let ns = self.ns_mut(n);
        ns.scroll = (ns.scroll as isize + delta).clamp(0, max) as usize;
        if delta != 0 {
            self.fill_gap_in_view(n, delta > 0);
        }
    }

    fn list_rows(&self, n: Network) -> usize {
        (self.layout_for(n).list.height as usize / self.row_h() as usize).max(1)
    }

    /// Keeps `list_top` in range without moving the view to the selection.
    fn clamp_top(&mut self, n: Network) {
        let rows = self.list_rows(n);
        let ns = self.ns_mut(n);
        let max = ns.visible().len().saturating_sub(rows);
        ns.list_top = ns.list_top.min(max);
    }

    /// Scrolls the sidebar just enough to show the selected chat.
    fn ensure_visible(&mut self, n: Network) {
        let rows = self.list_rows(n);
        let ns = self.ns_mut(n);
        ns.rows_seen = rows;
        let vis = ns.visible();
        let pos = ns.sel_pos(&vis);
        if pos < ns.list_top {
            ns.list_top = pos;
        } else if pos >= ns.list_top + rows {
            ns.list_top = pos + 1 - rows;
        }
        ns.list_top = ns.list_top.min(vis.len().saturating_sub(rows));
    }

    /// Keeps `selected` pointing at a visible chat. The sidebar scrolls to the selection when
    /// `follow` is set (the filter changed) or the selection had to move; a plain chat-list
    /// update leaves the viewport where the user put it.
    fn fix_selection(&mut self, n: Network, follow: bool) {
        let ns = self.ns_mut(n);
        let vis = ns.visible();
        let valid = ns
            .selected
            .as_ref()
            .is_some_and(|id| vis.iter().any(|&i| &ns.chats[i].id == id));
        if !valid {
            ns.selected = vis.first().map(|&i| ns.chats[i].id.clone());
        }
        if follow || !valid {
            self.ensure_visible(n);
        } else {
            self.clamp_top(n);
        }
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
                let ns = self.ns_mut(n);
                // Back online (first connect or after a drop): the next Enter asks the backend
                // for the chat again, which is the catch-up for anything missed meanwhile.
                if matches!(status, Status::Online { .. }) {
                    ns.requested = None;
                    // A request sent before the drop may never be answered: allow a new scroll
                    // to ask again.
                    ns.older.retain(|_, o| *o != Older::Loading);
                }
                // Signed out: the next sign-in may be another account, so photos are asked afresh
                // and no receipt state carries over (the backend sends it again per chat).
                if matches!(status, Status::NeedsLogin { .. }) {
                    ns.avatars.clear();
                    ns.receipts.clear();
                }
                ns.status = status;
                // A sign-in panel appearing or going away changes what is on screen.
                self.sync_read_timers();
            }
            Event::Chats(n, mut chats) => {
                chats.iter_mut().for_each(clean_chat);
                chats::sort_chats(&mut chats);
                self.ns_mut(n).chats = chats;
                self.read_open_chat(n, &mut out);
                self.fix_selection(n, false);
            }
            Event::ChatUpdated(mut chat) => {
                clean_chat(&mut chat);
                let n = chat.network;
                let ns = self.ns_mut(n);
                match ns.chats.iter_mut().find(|c| c.id == chat.id) {
                    Some(slot) => *slot = chat,
                    None => ns.chats.push(chat),
                }
                chats::sort_chats(&mut ns.chats);
                self.read_open_chat(n, &mut out);
                self.fix_selection(n, false);
            }
            Event::Messages {
                network,
                chat_id,
                mut messages,
            } => {
                messages.iter_mut().for_each(clean_message);
                crate::older::sort_messages(&mut messages);
                let follow = self.selecting_newest(network, &chat_id);
                // An older message highlighted: newer messages in the batch (a catch-up) must not
                // push it up the screen. Older history above does not move it.
                let is_open = self.ns(network).open.as_deref() == Some(chat_id.as_str());
                let below = (is_open && !follow)
                    .then(|| self.lines_below_selection(network))
                    .flatten();
                let ns = self.ns_mut(network);
                // A batch that starts at or before a gap fills it: the gap is gone.
                let covered = messages.first().is_some_and(|first| {
                    ns.gaps
                        .get(&chat_id)
                        .is_some_and(|(ts, id)| (first.ts, first.id.as_str()) <= (*ts, id.as_str()))
                });
                if covered {
                    ns.gaps.remove(&chat_id);
                }
                let held = ns.messages.entry(chat_id.clone()).or_default();
                *held = merge_history(std::mem::take(held), messages);
                if is_open {
                    out.extend(self.auto_images(network));
                }
                self.keep_selection(network, &chat_id);
                if follow {
                    self.follow_newest(network);
                } else if let (Some(before), Some(after)) =
                    (below, self.lines_below_selection(network))
                {
                    let ns = self.ns_mut(network);
                    ns.scroll = (ns.scroll + after).saturating_sub(before);
                    self.ensure_sel_visible(network);
                }
                self.select_when_loaded(network, &chat_id);
            }
            Event::Message(mut m) => {
                clean_message(&mut m);
                let n = m.network;
                let chat_id = m.chat_id.clone();
                let from_me = m.from_me;
                self.notify_arrival(&m);
                if !from_me {
                    self.typing_stops(n, &chat_id, &m.sender);
                }
                let is_open = self.ns(n).open.as_ref() == Some(&chat_id);
                let visible = self.convo_visible(n);
                // The newest message highlighted: the highlight moves on to the new one.
                let follow = self.selecting_newest(n, &chat_id);
                // A reader scrolled up keeps their place: grow the offset by the lines added.
                // The same while an older message is selected: it must not slide off screen.
                let anchored =
                    !follow && (self.ns(n).scroll > 0 || self.ns(n).focus == Focus::Select);
                let before = (is_open && anchored).then(|| self.convo_lines(n));
                // A message timed before what is held, or inside a jump's gap (an offline
                // redelivery), stays in the store until its stretch loads (`takes_live`).
                if self.holds(n, &chat_id, &m.id) || self.takes_live(n, &m) {
                    upsert_message(
                        self.ns_mut(n).messages.entry(chat_id.clone()).or_default(),
                        m,
                    );
                }
                if let Some(before) = before {
                    let added = self.convo_lines(n).saturating_sub(before);
                    self.ns_mut(n).scroll += added;
                }
                if follow {
                    self.follow_newest(n);
                }
                self.select_when_loaded(n, &chat_id);
                if is_open && !from_me {
                    out.extend(self.auto_images(n));
                }
                if is_open && !from_me && !self.ns(n).previewing {
                    let ns = self.ns_mut(n);
                    if !visible {
                        // The pane is not on screen (other tab, narrow list): nothing was read.
                        // Treat it as a preview, so it counts as read 2 s after it shows again.
                        ns.previewing = true;
                    } else if let Some(chat_id) = ns.open.clone() {
                        ns.mark_read(&chat_id, &mut out);
                    }
                }
            }
            Event::MessageConfirmed {
                client_id,
                mut message,
            } => {
                clean_message(&mut message);
                self.send_over(message.network, &client_id);
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
                self.send_over(network, &client_id);
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
            // An edit, a delete or a reaction: replaces the held copy by id, never raises a
            // notification or a read. A chat that was never loaded is skipped: opening it
            // brings the whole history, and a lone message here would hide that load.
            Event::MessageChanged(mut m) => {
                clean_message(&mut m);
                let (n, chat_id) = (m.network, m.chat_id.clone());
                // A held message is replaced. One not held goes in when it fits (a redelivery
                // of a stored message newer than what is held), not above what is loaded or in
                // a jump's gap, where the gap and older history would take it for an edge
                // (`takes_live`): it shows when its stretch loads.
                let fits = self.holds(n, &chat_id, &m.id) || self.takes_live(n, &m);
                if let Some(list) = self.ns_mut(n).messages.get_mut(&chat_id).filter(|_| fits) {
                    upsert_message(list, m);
                    self.keep_selection(n, &chat_id);
                }
            }
            Event::Capabilities { network, caps } => self.ns_mut(network).caps = caps,
            Event::Avatar {
                network,
                person_id,
                path,
            } => {
                // Only an answer to a pending ask: a late one after sign-out is dropped.
                if let Some(slot) = self.ns_mut(network).avatars.get_mut(&person_id) {
                    *slot = path;
                }
            }
            Event::Members {
                network,
                chat_id,
                members,
                error,
            } => {
                self.members_arrived(network, chat_id, members, error);
                // The `@name` list fills in as members arrive, but a list the user closed stays
                // closed until they edit the draft again.
                if !self.popup_closed {
                    out.extend(self.refresh_popup());
                }
            }
            Event::MediaReady {
                network,
                chat_id,
                message_id,
                index,
                path,
            } => {
                self.media_ready(network, chat_id, message_id, index, path);
                // A slot freed: a picture held back by the in-flight cap starts now.
                out.extend(self.auto_images(network));
            }
            Event::MediaFailed {
                network,
                chat_id,
                message_id,
                index,
                reason,
                fallback_url,
            } => {
                self.media_failed(network, chat_id, message_id, index, reason, fallback_url);
                out.extend(self.auto_images(network));
            }
            Event::SendProgress {
                network,
                chat_id,
                client_id,
                done,
                total,
            } => self.send_progress(network, &chat_id, client_id, done, total),
            ev @ (Event::OlderMessages { .. }
            | Event::OlderFailed { .. }
            | Event::Receipts { .. }
            | Event::Typing { .. }) => {
                self.handle_everyday(ev);
            }
        }
        self.relayout();
        out
    }

    /// The open chat is on screen, so anything unread in it is read: zero the badge and tell
    /// the backend once per update that carried an unread count.
    fn read_open_chat(&mut self, n: Network, out: &mut Out) {
        let visible = self.convo_visible(n);
        let ns = self.ns_mut(n);
        let Some(open) = ns.open.clone() else { return };
        // A previewed chat is read by Enter or the read timer, not by merely being on screen.
        if ns.previewing || !ns.chats.iter().any(|c| c.id == open && c.unread > 0) {
            return;
        }
        if visible {
            ns.mark_read(&open, out);
        } else {
            // Not on screen: back to a preview, read 2 s after the pane shows again.
            ns.previewing = true;
        }
    }

    // ---- keyboard ----

    /// True when a text box can take pasted text (the message box, a filter, the emoji picker,
    /// the path prompt, search or the `:` box).
    fn takes_text(&self) -> bool {
        matches!(
            self.context(),
            Context::Composer | Context::Filter | Context::Popup
        )
    }

    /// The key presses that were waiting together. Windows has no bracketed paste: pasted text
    /// and dropped file paths arrive as keys, with every line break as Enter (send). Such a block
    /// is a paste (never sends); anything else is handled one key at a time, in order.
    pub fn handle_key_burst(&mut self, keys: Vec<KeyEvent>) -> Out {
        match burst::classify(&keys, self.takes_text()) {
            burst::Burst::Paste(text) => self.handle_paste(&text),
            burst::Burst::Ignore => Vec::new(),
            burst::Burst::Keys => keys.into_iter().flat_map(|k| self.handle_key(k)).collect(),
        }
    }

    /// One key press: the keymap says which action it is for the current context, or that a
    /// sequence is under way; a key bound to nothing types into a text box or is ignored.
    pub fn handle_key(&mut self, key: KeyEvent) -> Out {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if self.tour.is_some() {
            return self.tour_key(key);
        }
        self.key_now(key)
    }

    /// A key press as the user would type it: the tour presses its keys through here too.
    pub(crate) fn key_now(&mut self, key: KeyEvent) -> Out {
        self.popup_alive = false;
        // A key is the user acting: a chat waiting to select its newest message stops waiting.
        self.ns_mut(self.active).select_on_load = None;
        let (net, chat) = (self.active, self.active().open.clone());
        let was_empty = self.active().draft().is_empty();
        let mut out = self.dispatch_key(key);
        out.extend(self.composing_after(net, chat, was_empty));
        self.popup_closed = !self.popup_alive;
        if self.popup_alive {
            out.extend(self.refresh_popup());
        } else {
            self.popup = None;
        }
        out
    }

    fn dispatch_key(&mut self, key: KeyEvent) -> Out {
        let ctx = self.context();
        let chord = KeyChord::from_event(&key);
        // The settings screen is modal and reads its own keys (help and a y/n over it still work).
        if self.settings_page.is_some() && self.help.is_none() && self.prompt.is_none() {
            return self.settings_key(&key);
        }
        // Several links in one message: a digit chooses one, then y/n confirms it.
        if let Some(Prompt::Pick(urls)) = &self.prompt {
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            if let (KeyCode::Char(d @ '1'..='9'), true) = (key.code, plain) {
                if let Some(url) = urls.get(d as usize - '1' as usize).cloned() {
                    self.prompt = Some(Prompt::Confirm(url));
                }
                return Vec::new();
            }
        }
        // In the path prompt Tab finishes the highlighted entry (not rebindable: it is the one
        // key everybody tries). Enter attaches, Up and Down choose, Esc cancels.
        if self.media.attach.is_some()
            && key.code == KeyCode::Tab
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.attach_complete();
            return Vec::new();
        }
        // The `:shortcode` list owns four keys; every other key goes on to the message box.
        if self.popup.is_some() && ctx == Context::Composer {
            let own = self
                .keys
                .lookup(Context::Popup, chord)
                .filter(|a| matches!(a, Action::Up | Action::Down | Action::Accept | Action::Back));
            if let Some(a) = own {
                return self.popup_action(a, ctx);
            }
        }
        // A held first key only counts in the context it was typed in.
        let held = self
            .pending
            .take()
            .filter(|(c, _)| *c == ctx)
            .map(|(_, k)| k);
        match self.keys.step(ctx, held, chord) {
            Step::Action(a) => self.run_action(a, ctx),
            Step::Pending => {
                self.pending = Some((ctx, chord));
                Vec::new()
            }
            Step::Cancel => Vec::new(),
            Step::Unbound => {
                self.type_key(ctx, &key);
                Vec::new()
            }
        }
    }

    /// Typed characters and Backspace: not actions, so they never go through the keymap. AltGr
    /// characters (`@`, braces, the euro sign on many layouts) arrive as Ctrl+Alt and type too.
    fn type_key(&mut self, ctx: Context, key: &KeyEvent) {
        let n = self.active;
        let typed = typed_char(key);
        match (ctx, key.code, typed) {
            (Context::Filter, _, Some(c)) => {
                self.ns_mut(n).filter.insert_char(c);
                self.fix_selection(n, true);
            }
            (Context::Composer, _, Some(c)) => {
                if let Some(d) = self.ns_mut(n).composer_mut() {
                    d.insert_char(c);
                }
                self.popup_alive = true;
            }
            (Context::Popup, _, Some(c)) => {
                if self.palette.is_some() {
                    let mut buf = [0u8; 4];
                    self.palette_type(c.encode_utf8(&mut buf));
                } else if self.search.is_some() {
                    let mut buf = [0u8; 4];
                    self.search_type(c.encode_utf8(&mut buf));
                } else if self.media.attach.is_some() {
                    let mut buf = [0u8; 4];
                    self.attach_type(c.encode_utf8(&mut buf));
                } else if let Some(p) = self.picker.as_mut() {
                    p.query.insert_char(c);
                    p.sel = 0;
                    p.top = 0;
                }
            }
            _ => {}
        }
    }

    /// Runs one action for the active network. An action that does not apply where it was
    /// pressed (a user bound `send` in the list, say) does nothing.
    fn run_action(&mut self, action: Action, ctx: Context) -> Out {
        let n = self.active;
        let panel = self.ns(n).panel();
        let focus = self.ns(n).focus;
        match action {
            Action::Quit => self.quit = true,
            Action::NextNetwork => return self.switch_by(1),
            Action::PrevNetwork => return self.switch_by(-1),
            Action::GotoNetwork1 | Action::GotoNetwork2 => {
                let i = usize::from(action == Action::GotoNetwork2);
                if let Some(&net) = self.tab_order().get(i) {
                    self.set_active(net);
                }
            }
            Action::CommandBar => self.open_palette(),
            Action::Settings => return self.run_cmd(CmdId::Settings, None),
            Action::Help => self.help = Some(0),
            Action::Up
            | Action::Down
            | Action::Top
            | Action::Bottom
            | Action::PageUp
            | Action::PageDown
            | Action::ScreenTop
            | Action::ScreenMiddle
            | Action::ScreenBottom
            | Action::Accept
            | Action::Back => return self.run_verb(action, ctx),
            Action::Compose if ctx == Context::List && !panel => return self.open_selected(n),
            Action::Compose if ctx == Context::Select => self.leave_select(n),
            Action::MarkRead if !panel => return self.mark_selected_read(n),
            // Left and Right move through the emoji grid; the other editing keys edit its search.
            Action::CursorLeft | Action::CursorRight
                if ctx == Context::Popup && self.line_box_is_picker() =>
            {
                self.picker_step(if action == Action::CursorLeft { -1 } else { 1 }, 0);
            }
            Action::CursorLeft
            | Action::CursorRight
            | Action::WordLeft
            | Action::WordRight
            | Action::LineStart
            | Action::LineEnd
            | Action::DeleteBack
            | Action::DeleteForward
            | Action::DeleteWordBack
            | Action::ClearLine
                if matches!(ctx, Context::Popup | Context::Filter) =>
            {
                self.line_edit(action);
            }
            Action::CursorLeft
            | Action::CursorRight
            | Action::WordLeft
            | Action::WordRight
            | Action::LineStart
            | Action::LineEnd
            | Action::DeleteBack
            | Action::DeleteForward
            | Action::DeleteWordBack
            | Action::ClearLine
            | Action::InsertNewline
                if ctx == Context::Composer =>
            {
                self.edit(n, action);
            }
            Action::EmojiPicker if ctx == Context::Composer && self.ns(n).open.is_some() => {
                self.picker = Some(Picker::new(PickerFor::Compose, !self.recents.is_empty()));
            }
            Action::Attach if ctx == Context::Composer => {
                self.start_attach(n);
            }
            Action::PasteImage if ctx == Context::Composer => {
                self.request_clipboard(n);
            }
            Action::OpenMedia if ctx == Context::Select => return self.open_media(n),
            Action::Copy if ctx == Context::Select => self.copy_selected(n),
            Action::JumpBack if ctx == Context::Select => self.jump_back(n),
            Action::ViewImage if ctx == Context::Select => return self.view_image(n),
            Action::SelectMessages if !panel => return self.enter_select(n),
            Action::Search => self.open_search(),
            Action::SearchChat if !panel => self.open_chat_search(n),
            Action::NextMatch if ctx == Context::Select => self.step_match(n, true),
            Action::PrevMatch if ctx == Context::Select => self.step_match(n, false),
            Action::NextUnread if !panel => self.jump_unread(n, 1),
            Action::PrevUnread if !panel => self.jump_unread(n, -1),
            Action::MarkAllRead if !panel => self.ask_mark_all_read(n),
            Action::Pin if !panel => return self.flag_action(n, ChatFlag::Pinned),
            Action::Mute if !panel => return self.flag_action(n, ChatFlag::Muted),
            Action::Hide if !panel => return self.flag_action(n, ChatFlag::Hidden),
            Action::ShowHidden if !panel => self.toggle_show_hidden(n),
            Action::Reply | Action::React | Action::Edit | Action::Delete
                if ctx == Context::Select =>
            {
                self.message_action(n, action);
            }
            Action::CancelReply if !panel => self.cancel_target(n),
            Action::OpenLink if ctx == Context::Select => self.link_selected(n),
            Action::OpenLink if !panel => self.start_link(n),
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::Confirm(_))) => {
                if let Some(Prompt::Confirm(url)) = self.prompt.take() {
                    self.open_urls.push(url);
                }
            }
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::Delete { .. })) => {
                if let Some(Prompt::Delete {
                    chat_id,
                    message_id,
                }) = self.prompt.take()
                {
                    return vec![(
                        n,
                        Command::Delete {
                            chat_id,
                            message_id,
                        },
                    )];
                }
            }
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::SendFile { .. })) => {
                if let Some(Prompt::SendFile { chat_id, .. }) = self.prompt.take() {
                    return self.confirm_send_file(n, &chat_id);
                }
            }
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::MarkAllRead { .. })) => {
                self.prompt = None;
                return self.mark_all_read(n);
            }
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::SignOut(_))) => {
                if let Some(Prompt::SignOut(net)) = self.prompt.take() {
                    return vec![(net, Command::Logout)];
                }
            }
            Action::ConfirmYes if matches!(self.prompt, Some(Prompt::ResetKeys)) => {
                self.prompt = None;
                return self.reset_all_keys();
            }
            Action::ConfirmNo => self.prompt = None,
            Action::FilterStart if !panel => self.ns_mut(n).focus = Focus::Filter,
            Action::Refresh if !panel => return self.refresh(n),
            Action::Send if focus == Focus::Composer && !panel => return self.send_and_pause(n),
            // Bound where it does not apply, or the `unbound` pseudo-action (never stored).
            _ => {}
        }
        Vec::new()
    }

    /// Up, Down, Top, Bottom, the page keys, Accept and Back: one action each, doing what it
    /// means where it is pressed. Bound where it means nothing, it does nothing.
    fn run_verb(&mut self, action: Action, ctx: Context) -> Out {
        use Action::{
            Accept, Back, Bottom, Down, PageDown, PageUp, ScreenBottom, ScreenMiddle, ScreenTop,
            Top, Up,
        };
        let n = self.active;
        match ctx {
            Context::Help => {
                let page = (ui::help_view_rows(self, self.size) as isize - 1).max(1);
                match action {
                    Up => self.scroll_help(-1),
                    Down => self.scroll_help(1),
                    Top => self.scroll_help(isize::MIN),
                    Bottom => self.scroll_help(isize::MAX),
                    PageUp | ScreenTop => self.scroll_help(-page),
                    PageDown | ScreenBottom => self.scroll_help(page),
                    Back => self.help = None,
                    _ => {}
                }
            }
            Context::Popup => match action {
                Up | Down | Accept | Back => return self.popup_action(action, ctx),
                PageUp | PageDown => {
                    let up = action == PageUp;
                    if self.palette.is_some() {
                        self.palette_page(up);
                    } else if self.search.is_some() {
                        self.search_page(up);
                    } else if let Some(p) = self.picker.as_mut() {
                        p.query.clear();
                        p.go_category(if up { -1 } else { 1 });
                    }
                }
                _ => {}
            },
            Context::Confirm if action == Back => self.prompt = None,
            Context::Panel if action == Accept => {
                if matches!(self.ns(n).status, Status::NeedsLogin { .. }) {
                    return vec![(n, Command::Login)];
                }
            }
            Context::Select => match action {
                Back => self.select_to_list(n),
                Accept => {}
                _ => return self.select_move(n, action),
            },
            Context::Filter => match action {
                Up => self.move_sel(n, -1),
                Down => self.move_sel(n, 1),
                Accept => self.ns_mut(n).focus = Focus::List,
                Back => {
                    let ns = self.ns_mut(n);
                    ns.filter.clear();
                    ns.focus = Focus::List;
                    self.fix_selection(n, true);
                }
                _ => {}
            },
            Context::List => {
                let rows = self.list_rows(n) as isize;
                match action {
                    Up => self.move_sel(n, -1),
                    Down => self.move_sel(n, 1),
                    Top => self.move_sel(n, isize::MIN),
                    Bottom => self.move_sel(n, isize::MAX),
                    PageUp => self.move_sel(n, -rows),
                    PageDown => self.move_sel(n, rows),
                    ScreenTop | ScreenMiddle | ScreenBottom => self.move_sel_on_screen(n, action),
                    Accept => return self.select_chat(n),
                    Back if !self.ns(n).filter.is_empty() => {
                        self.ns_mut(n).filter.clear();
                        self.fix_selection(n, true);
                    }
                    Back => self.close_chat(n),
                    _ => {}
                }
            }
            Context::Composer => match action {
                // The page keys scroll the messages; a page up from the very top asks for older
                // ones.
                PageUp | PageDown => {
                    let dir = if action == PageUp { 1 } else { -1 };
                    let at_top = dir > 0 && self.at_top(n);
                    self.page(n, dir);
                    if at_top {
                        return self.request_older(n);
                    }
                }
                // A reply or edit in progress goes first, then a chosen file; the next Back
                // leaves the box for message selection, or for the chat list when the chat has
                // no message to select.
                Back if self.ns(n).target().is_some() => self.cancel_target(n),
                Back if self.ns(n).attachment().is_some() => self.drop_attachment(n),
                // `try_select` moves to selection when it can; only when it cannot does Back go to
                // the chat list.
                Back if !self.try_select(n) => self.back_to_list(n),
                _ => {}
            },
            _ => {}
        }
        Vec::new()
    }

    // ---- message box editing, completion, emoji picker, links ----

    /// One editing key in the message box. Any of them keeps the `:shortcode` list in step with
    /// the text before the cursor.
    fn edit(&mut self, n: Network, action: Action) {
        // Backspace at the very start of the box takes the file chip off, not a letter.
        if action == Action::DeleteBack && self.backspace_hits_chip(n) {
            self.drop_attachment(n);
            return;
        }
        let Some(c) = self.ns_mut(n).composer_mut() else {
            return;
        };
        match action {
            Action::CursorLeft => c.left(),
            Action::CursorRight => c.right(),
            Action::WordLeft => c.word_left(),
            Action::WordRight => c.word_right(),
            Action::LineStart => c.line_start(),
            Action::LineEnd => c.line_end(),
            Action::DeleteBack => {
                if !c.delete_mention_back() {
                    c.delete_back();
                }
            }
            Action::DeleteForward => c.delete_forward(),
            Action::DeleteWordBack => c.delete_word_back(),
            Action::ClearLine => c.clear_line(),
            Action::InsertNewline => c.insert_char('\n'),
            _ => return,
        }
        self.popup_alive = true;
    }

    /// Opens, updates or closes the `:shortcode` list from the text before the cursor. The same
    /// text keeps the selection; new text starts at the top.
    fn refresh_popup(&mut self) -> Out {
        let n = self.active;
        let ns = self.ns(n);
        let eligible = ns.focus == Focus::Composer && !ns.panel() && !self.overlay_open();
        let texts = ns.composer().filter(|_| eligible).map(|c| {
            let before = c.before_cursor().to_string();
            let rest = c.text()[before.len()..].trim().is_empty();
            (before, rest)
        });
        let Some((before, nothing_after)) = texts else {
            self.popup = None;
            return Vec::new();
        };
        // Commands only when `:word` is the whole draft: choosing one drops the draft, so text
        // after the cursor (a message being edited at its start) must never be at stake.
        if let Some(query) = colon_query(&before).filter(|_| nothing_after) {
            return self.refresh_command_popup(query);
        }
        let Some(query) = emoji::shortcode_query(&before).map(str::to_string) else {
            return self.refresh_mention_popup(n, &before);
        };
        if self
            .popup
            .as_ref()
            .is_some_and(|p| p.title == "Emoji" && p.query == query)
        {
            return Vec::new();
        }
        let items = emoji_items(&query);
        self.popup = if items.is_empty() {
            None
        } else {
            // The colon plus what was typed after it: all ASCII, one cluster each.
            let replace = 1 + query.chars().count();
            Some(Popup::new(items, query, replace))
        };
        Vec::new()
    }

    /// A popup key, for the `:shortcode` list (`ctx` is the message box) or the picker (`Popup`).
    fn popup_action(&mut self, action: Action, ctx: Context) -> Out {
        let n = self.active;
        if ctx == Context::Popup && self.search.is_some() {
            return self.search_action(action);
        }
        if ctx == Context::Popup && self.palette.is_some() {
            return self.palette_action(action);
        }
        if ctx == Context::Popup && self.media.attach.is_some() {
            self.attach_action(n, action);
            return Vec::new();
        }
        if ctx == Context::Popup {
            match action {
                Action::Up => self.picker_step(0, -1),
                Action::Down => self.picker_step(0, 1),
                Action::Accept => return self.picker_accept(n),
                _ => self.picker = None,
            }
            return Vec::new();
        }
        let Some(p) = self.popup.as_mut() else {
            return Vec::new();
        };
        match action {
            Action::Up => p.up(),
            Action::Down => p.down(),
            Action::Accept => {
                let command = p.chosen().and_then(|i| i.command);
                let chosen = p.chosen().map(|i| (i.insert.clone(), i.person.clone()));
                let replace = p.replace;
                self.popup = None;
                if let Some(id) = command {
                    // A command runs at once; what was typed is not a message.
                    return self.run_chosen_command(n, id);
                }
                if let Some((text, person)) = chosen {
                    if let Some(c) = self.ns_mut(n).composer_mut() {
                        c.replace_before_cursor(replace, &text);
                        // A mention keeps who it stands for, so send can make it a real one.
                        c.mentions.extend(person.clone());
                    }
                    if person.is_none() {
                        self.remember_emoji(&text);
                    }
                }
                return Vec::new();
            }
            _ => {
                self.popup = None;
                return Vec::new();
            }
        }
        self.popup_alive = true;
        Vec::new()
    }

    /// Moves emoji to the front of the recents, in memory and (best effort) in the store.
    fn remember_emoji(&mut self, emoji: &str) {
        emoji::touch_recent(&mut self.recents, emoji);
        if let Some(store) = &self.store {
            if let Err(e) = store.touch_recent_emoji(emoji, self.clock.now_ms) {
                log::error("tui", &format!("recent emoji not saved: {e}"));
            }
        }
    }

    /// Where the picker's grid is on screen, or None when the terminal is too small for it.
    fn picker_grid(&self) -> Option<ui::PickerGeom> {
        ui::picker_geom(self, &self.layout_in(self.size))
    }

    /// Moves the picker's selection by cells and scrolls to keep it in view.
    fn picker_step(&mut self, dx: isize, dy: isize) {
        let Some(len) = self.picker.as_ref().map(|p| p.items(&self.recents).len()) else {
            return;
        };
        let (cols, rows) = self.picker_grid().map_or((1, 1), |g| (g.cols, g.rows));
        if let Some(p) = self.picker.as_mut() {
            p.step(len, cols, dx, dy);
            p.keep_visible(cols, rows);
        }
    }

    /// Left, right and Backspace inside the picker: move a cell, or edit the search text.
    /// The one-line box that takes the editing keys: the open overlay's text (the command bar,
    /// search, the file prompt or the emoji search), else the chat filter while it has focus. The
    /// message box edits through `edit`.
    fn line_box(&mut self) -> Option<&mut TextInput> {
        if self.palette.is_some() {
            return self.palette.as_mut().map(|p| &mut p.query);
        }
        if self.search.is_some() {
            return self.search.as_mut().map(|s| &mut s.query);
        }
        if self.media.attach.is_some() {
            return self.media.attach.as_mut().map(|a| &mut a.input);
        }
        if self.picker.is_some() {
            return self.picker.as_mut().map(|p| &mut p.query);
        }
        let n = self.active;
        let ns = self.ns_mut(n);
        (ns.focus == Focus::Filter).then_some(&mut ns.filter)
    }

    /// True when the emoji picker's search is the box `line_box` edits.
    fn line_box_is_picker(&self) -> bool {
        self.picker.is_some()
            && self.palette.is_none()
            && self.search.is_none()
            && self.media.attach.is_none()
    }

    /// An editing key in the one-line box. A change of text refreshes what follows it: the
    /// command bar's list, the search hits, the file candidates, the emoji grid or the chat list.
    fn line_edit(&mut self, action: Action) {
        let Some(b) = self.line_box() else {
            return;
        };
        let before = b.text().to_string();
        match action {
            Action::CursorLeft => b.left(),
            Action::CursorRight => b.right(),
            Action::WordLeft => b.word_left(),
            Action::WordRight => b.word_right(),
            Action::LineStart => b.line_start(),
            Action::LineEnd => b.line_end(),
            Action::DeleteBack => b.delete_back(),
            Action::DeleteForward => b.delete_forward(),
            Action::DeleteWordBack => b.delete_word_back(),
            Action::ClearLine => b.clear_line(),
            _ => return,
        }
        if b.text() == before {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.sel = 0;
            p.top = 0;
        } else if self.search.is_some() {
            self.search_rerun();
        } else if self.media.attach.is_some() {
            self.attach_refresh();
        } else if let Some(p) = self.picker.as_mut() {
            p.sel = 0;
            p.top = 0;
        } else {
            self.fix_selection(self.active, true);
        }
    }

    /// Enter or a click on a picker cell: insert the emoji into the draft, or send it as a
    /// reaction. An empty grid (a search with no hit) keeps the picker open.
    fn picker_accept(&mut self, n: Network) -> Out {
        let Some(p) = self.picker.take() else {
            return Vec::new();
        };
        let Some(chosen) = p
            .items(&self.recents)
            .get(p.sel)
            .map(|e| e.as_str().to_string())
        else {
            self.picker = Some(p);
            return Vec::new();
        };
        self.remember_emoji(&chosen);
        match p.purpose {
            PickerFor::Compose => {
                if let Some(c) = self.ns_mut(n).composer_mut() {
                    c.insert_str(&chosen);
                }
                Vec::new()
            }
            PickerFor::React {
                chat_id,
                message_id,
            } => {
                // Choosing the emoji you already reacted with takes it back.
                let remove = self.ns(n).messages.get(&chat_id).is_some_and(|l| {
                    l.iter().any(|m| {
                        m.id == message_id
                            && m.reactions.iter().any(|r| r.mine && r.emoji == chosen)
                    })
                });
                vec![(
                    n,
                    Command::React {
                        chat_id,
                        message_id,
                        emoji: chosen,
                        remove,
                    },
                )]
            }
        }
    }

    /// Opens the picker to react to a message; choosing sends `Command::React`. Selection mode
    /// (issue 11) calls this for its `react` key.
    pub fn open_reaction_picker(&mut self, chat_id: &str, message_id: &str) {
        self.picker = Some(Picker::new(
            PickerFor::React {
                chat_id: chat_id.to_string(),
                message_id: message_id.to_string(),
            },
            !self.recents.is_empty(),
        ));
    }

    /// `open_link`: the links of the newest message in the open chat that has any. Selection
    /// mode (issue 11) will offer the selected message's links through `offer_links` instead.
    fn start_link(&mut self, n: Network) {
        let ns = self.ns(n);
        let urls = ns
            .open
            .as_ref()
            .and_then(|id| ns.messages.get(id))
            .and_then(|list| {
                list.iter()
                    .rev()
                    .filter(|m| !matches!(m.state, MessageState::Deleted))
                    .map(|m| links::urls(&m.text))
                    .find(|u| !u.is_empty())
            })
            .unwrap_or_default();
        if urls.is_empty() {
            self.push_notice(n, "No link in this chat.", false);
        } else {
            self.offer_links(urls);
        }
    }

    /// Asks before opening: one link gets its host on a y/n line, several get a 1-9 choice
    /// first. Only http and https URLs reach here (`links::urls`), and `y` queues the URL for
    /// the run loop, never a command.
    pub(crate) fn offer_links(&mut self, mut urls: Vec<String>) {
        urls.retain(|u| open::web_url(u).is_some());
        urls.truncate(9);
        self.prompt = match urls.len() {
            0 => None,
            1 => Some(Prompt::Confirm(urls.remove(0))),
            _ => Some(Prompt::Pick(urls)),
        };
    }

    /// Focus back on the chat list, the chat still shown. The next Enter on it asks the backend
    /// again.
    pub(crate) fn back_to_list(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        ns.focus = Focus::List;
        ns.requested = None;
    }

    /// Back on the chat list with no filter: the message pane is emptied. Moving through the
    /// list previews a chat again.
    fn close_chat(&mut self, n: Network) {
        let ns = self.ns_mut(n);
        ns.open = None;
        ns.previewing = false;
        ns.settle = None;
        ns.read_due = None;
        ns.requested = None;
        ns.scroll = 0;
    }

    /// `screen_top`, `screen_middle`, `screen_bottom` on the chat list: the first, middle or last
    /// chat the sidebar shows.
    fn move_sel_on_screen(&mut self, n: Network, action: Action) {
        let rows = self.list_rows(n);
        let ns = self.ns(n);
        let vis = ns.visible();
        let shown = rows.min(vis.len().saturating_sub(ns.list_top));
        if shown == 0 {
            return;
        }
        let to = ns.list_top
            + match action {
                Action::ScreenTop => 0,
                Action::ScreenMiddle => (shown - 1) / 2,
                _ => shown - 1,
            };
        let delta = to as isize - ns.sel_pos(&vis) as isize;
        self.move_sel(n, delta);
    }

    /// Accept on the chat list: into the chat, its newest message selected. A chat with no
    /// message to select (empty, or not loaded yet) goes to its message box, and selects the
    /// newest message once one arrives (`select_on_load`).
    fn select_chat(&mut self, n: Network) -> Out {
        let out = self.open_selected(n);
        if self.ns(n).focus == Focus::Composer && !self.try_select(n) {
            let ns = self.ns_mut(n);
            ns.select_on_load = ns.open.clone();
        }
        out
    }

    /// The message box of the selected chat: show it, ask the backend, focus the box.
    fn open_selected(&mut self, n: Network) -> Out {
        let ns = self.ns(n);
        let vis = ns.visible();
        let Some(&i) = vis.get(ns.sel_pos(&vis)) else {
            return Vec::new();
        };
        let id = ns.chats[i].id.clone();
        self.show(n, id);
        self.commit(n)
    }

    fn refresh(&mut self, n: Network) -> Out {
        self.ns_mut(n).requested = None;
        vec![(n, Command::Refresh)]
    }

    /// Scrolls the help overlay, kept inside the text.
    fn scroll_help(&mut self, delta: isize) {
        let Some(top) = self.help else { return };
        let max = ui::help_rows(self)
            .len()
            .saturating_sub(ui::help_view_rows(self, self.size)) as isize;
        self.help = Some((top as isize).saturating_add(delta).clamp(0, max) as usize);
    }

    /// A command the run loop could not hand to its backend (channel closed, no such backend).
    /// A send must not stay pending for ever: it fails now, with the reason.
    pub fn command_undelivered(&mut self, net: Network, cmd: &Command) {
        const REASON: &str = "Backend stopped: restart chatcli.";
        // An older-history request that never left: the chat must not stay "loading older...".
        if let Command::LoadOlder { chat_id, .. } = cmd {
            self.ns_mut(net).older.remove(chat_id);
        }
        if let Command::Send {
            chat_id, client_id, ..
        }
        | Command::SendMedia {
            chat_id, client_id, ..
        } = cmd
        {
            self.handle_event(Event::MessageFailed {
                network: net,
                chat_id: chat_id.clone(),
                client_id: client_id.clone(),
                error: REASON.to_string(),
            });
        }
        self.push_notice(net, REASON, true);
    }

    fn switch_by(&mut self, step: isize) -> Out {
        let order = self.tab_order();
        let at = order.iter().position(|n| *n == self.active).unwrap_or(0);
        self.set_active(order[crate::list::step(at, order.len(), step)]);
        Vec::new()
    }

    /// Pasted text (bracketed paste): inserted at the cursor of the message box, line breaks
    /// kept, control characters dropped. A paste never sends, however many lines it has. The
    /// one-line boxes (filter, `:`, emoji search) take it flattened.
    pub fn handle_paste(&mut self, text: &str) -> Out {
        // The tour plays by itself: a paste would land in whatever it has open.
        if self.tour.is_some() {
            return Vec::new();
        }
        let n = self.active;
        self.ns_mut(n).select_on_load = None;
        let chat = self.active().open.clone();
        let was_empty = self.active().draft().is_empty();
        let before = self.active().draft().to_string();
        self.paste_into(text);
        // A paste that changed the draft is an edit: a list closed earlier may come back for it
        // (when members arrive). Nothing pasted is run.
        if self.active().draft() != before {
            self.popup_closed = false;
        }
        self.composing_after(n, chat, was_empty)
    }

    fn paste_into(&mut self, text: &str) {
        let n = self.active;
        let flat = clean_line(text);
        self.popup = None;
        if self.help.is_some() || self.prompt.is_some() {
            return;
        }
        if self.settings_page.is_some() {
            // Only a value being typed takes a paste; the rest of the screen drops it.
            self.settings_type(flat.trim());
            return;
        }
        if self.palette.is_some() {
            self.palette_type(flat.trim());
            return;
        }
        if self.search.is_some() {
            self.search_type(flat.trim());
            return;
        }
        if self.media.attach.is_some() {
            self.attach_type(flat.trim());
            return;
        }
        if let Some(p) = self.picker.as_mut() {
            p.query.insert_str(flat.trim());
            p.sel = 0;
            p.top = 0;
            return;
        }
        let focus = self.ns(n).focus;
        match focus {
            // A file dragged onto the terminal arrives as its path: it becomes the attachment.
            Focus::Composer if self.paste_as_attachment(n, text) => {}
            Focus::Composer => {
                let mut clean = clean_paste(text);
                let room = DRAFT_MAX_BYTES.saturating_sub(self.ns(n).draft().len());
                let cut = clean.len() > room;
                if cut {
                    let mut at = room;
                    while !clean.is_char_boundary(at) {
                        at -= 1;
                    }
                    clean.truncate(at);
                }
                if let Some(d) = self.ns_mut(n).composer_mut() {
                    d.insert_str(&clean);
                }
                if cut {
                    self.push_notice(
                        n,
                        "Paste cut: a message can hold 1 MB of text at most.",
                        true,
                    );
                }
            }
            Focus::Filter => {
                self.ns_mut(n).filter.insert_str(&flat);
                self.fix_selection(n, true);
            }
            Focus::List | Focus::Select => {}
        }
    }

    fn page(&mut self, n: Network, dir: isize) {
        let step = self.convo_size(n).1.saturating_sub(1).max(1) as isize;
        self.scroll_by(n, dir * step);
    }

    /// Moves the selection and previews the chat it lands on.
    fn move_sel(&mut self, n: Network, delta: isize) {
        let ns = self.ns_mut(n);
        let vis = ns.visible();
        if vis.is_empty() {
            return;
        }
        let pos = (ns.sel_pos(&vis) as isize)
            .saturating_add(delta)
            .clamp(0, vis.len() as isize - 1) as usize;
        let id = ns.chats[vis[pos]].id.clone();
        ns.selected = Some(id.clone());
        self.show(n, id);
        self.ensure_visible(n);
    }

    /// Puts a chat in the message pane from what is already held, with no command. Starts the
    /// settle timer (unless the backend was just asked for this chat) and the read timer.
    fn show(&mut self, n: Network, id: String) {
        let now = self.clock.now_ms;
        let visible = self.convo_visible(n);
        let ns = self.ns_mut(n);
        if ns.open.as_ref() == Some(&id) {
            return;
        }
        ns.open = Some(id.clone());
        ns.previewing = true;
        ns.scroll = 0;
        // No timer while the pane is hidden: a narrow terminal shows only the list, and
        // Enter (which marks read) is what brings the conversation up.
        ns.read_due = visible.then_some(now + READ_AFTER_MS);
        ns.settle = (ns.requested.as_ref() != Some(&id)).then(|| (id, now + SETTLE_MS));
    }

    /// Enter or a click in the composer: the chat in the pane becomes the one being replied
    /// to. Asks the backend for it unless that already happened, and marks it read.
    fn commit(&mut self, n: Network) -> Out {
        let ns = self.ns_mut(n);
        let Some(id) = ns.open.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        ns.focus = Focus::Composer;
        ns.scroll = 0;
        ns.settle = None;
        ns.read_due = None;
        if ns.requested.as_ref() != Some(&id) {
            ns.requested = Some(id.clone());
            out.push((
                n,
                Command::OpenChat {
                    chat_id: id.clone(),
                },
            ));
        }
        // Only when something is unread: Enter on a read chat costs no Graph call.
        if ns.chats.iter().any(|c| c.id == id && c.unread > 0) {
            ns.mark_read(&id, &mut out);
        }
        ns.previewing = false;
        self.ensure_visible(n);
        out.extend(self.auto_images(n));
        out
    }

    fn send(&mut self, n: Network) -> Out {
        let text = self.ns(n).draft().trim().to_string();
        let Some(chat_id) = self.ns(n).open.clone() else {
            return Vec::new();
        };
        if let Some(Target::Edit { .. }) = self.ns(n).targets.get(&chat_id) {
            return self.send_edit(n, chat_id, text);
        }
        // Text is always text: a command runs only from the list above the box (`:word`).
        // A chosen file goes first: the draft is its caption.
        if self.ns(n).pending.contains_key(&chat_id) {
            return self.send_media(n);
        }
        if text.is_empty() {
            return Vec::new();
        }
        // Refuse here, with the draft kept, instead of sending what the network would reject.
        if !limits::text_fits(n, &text) {
            self.push_notice(n, &too_long(n), true);
            return Vec::new();
        }
        let reply_to = match self.ns(n).targets.get(&chat_id) {
            Some(Target::Reply(q)) => Some(q.clone()),
            _ => None,
        };
        let mentions = self
            .ns(n)
            .composer()
            .map(|c| c.mentions_in(&text))
            .unwrap_or_default();
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
            reply_to: reply_to.clone(),
            mentions: mentions.clone(),
            ..ChatMessage::blank(n)
        };
        let ns = self.ns_mut(n);
        ns.drafts.remove(&chat_id);
        ns.targets.remove(&chat_id);
        ns.scroll = 0;
        upsert_message(ns.messages.entry(chat_id.clone()).or_default(), msg);
        vec![(
            n,
            Command::Send {
                chat_id,
                text,
                client_id,
                reply_to,
                mentions,
            },
        )]
    }

    // ---- mouse ----

    /// Mouse in the emoji picker: click a cell to choose it, the wheel scrolls the grid, a
    /// click outside closes it.
    fn picker_mouse(&mut self, m: MouseEvent) -> Out {
        let n = self.active;
        let Some(g) = self.picker_grid() else {
            return Vec::new();
        };
        let Some(p) = self.picker.as_ref() else {
            return Vec::new();
        };
        let len = p.items(&self.recents).len();
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if g.grid.contains(pos) {
                    let col = (pos.x - g.grid.x) as usize / ui::PICKER_CELL;
                    let idx = (p.top + (pos.y - g.grid.y) as usize) * g.cols + col;
                    if col < g.cols && idx < len {
                        if let Some(p) = self.picker.as_mut() {
                            p.sel = idx;
                        }
                        return self.picker_accept(n);
                    }
                } else if !g.outer.contains(pos) {
                    self.picker = None;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if m.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                if let Some(p) = self.picker.as_mut() {
                    p.scroll(len, g.cols, g.rows, delta);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> Out {
        let had_prompt = self.prompt.is_some();
        let out = self.mouse_now(m);
        if !had_prompt && self.prompt.is_some() {
            self.prompt_opened_ms = self.clock.now_ms;
        }
        out
    }

    fn mouse_now(&mut self, m: MouseEvent) -> Out {
        if self.tour.as_ref().is_some_and(|t| !t.done) {
            return Vec::new();
        }
        // Pointer moves, drags and releases do nothing: they must not close the completion
        // list or end a held `g` on the way to a click.
        if matches!(
            m.kind,
            MouseEventKind::Moved | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
        ) {
            return Vec::new();
        }
        // A click or the wheel is the user acting, as a key is (`select_on_load`).
        self.ns_mut(self.active).select_on_load = None;
        // Nothing is drawn but a notice: nothing can be clicked.
        if ui::too_small(self.size) {
            return Vec::new();
        }
        let foot = self.footer_click_hit(m);
        // A held vim `g` is about the keyboard; a click or wheel ends it. Something open over
        // the screen (help, a question, the attach prompt, search, the command bar, settings)
        // gets the mouse first, so the screen behind it stays put.
        self.pending = None;
        if self.help.is_some()
            || self.prompt.is_some()
            || self.media.attach.is_some()
            || self.search.is_some()
            || self.palette.is_some()
            || self.settings_page.is_some()
        {
            return self.overlay_mouse(m);
        }
        if self.picker.is_some() {
            return self.picker_mouse(m);
        }
        let pos = Position::new(m.column, m.row);
        let n = self.active;
        let lay = self.layout_for(n);
        // The completion list takes clicks on it; any other mouse event closes it.
        if let Some(out) = self.popup_mouse(m, &lay, pos) {
            return out;
        }
        // A click on a footer key hint runs that key's action (before the popup is closed: the
        // footer drawn with the list open is the one that was clicked).
        if let Some(action) = foot {
            return self.run_click_action(action);
        }
        self.popup = None;
        self.popup_closed = true;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if lay.header.contains(pos) {
                    let hit = ui::tabs(self, self.size.width)
                        .into_iter()
                        .find(|t| pos.x >= t.x && pos.x < t.x + t.w);
                    if let Some(t) = hit {
                        self.set_active(t.net);
                    }
                } else if !self.ns(n).panel() && lay.list.contains(pos) {
                    let ns = self.ns(n);
                    let row = ns.list_top + (pos.y - lay.list.y) as usize / self.row_h() as usize;
                    if let Some(&i) = ns.visible().get(row) {
                        // A click previews like a cursor move; a click on the chat already
                        // shown opens it, as Enter does.
                        let id = ns.chats[i].id.clone();
                        if ns.selected.as_ref() == Some(&id) && ns.open.as_ref() == Some(&id) {
                            return self.open_selected(n);
                        }
                        let ns = self.ns_mut(n);
                        ns.selected = Some(id.clone());
                        ns.focus = Focus::List;
                        self.show(n, id);
                        self.ensure_visible(n);
                    }
                } else if !self.ns(n).panel()
                    && lay.composer.contains(pos)
                    && self.ns(n).open.is_some()
                {
                    return self.click_composer(n, &lay, pos);
                } else if !self.ns(n).panel() && lay.messages.contains(pos) {
                    if let Some((id, kind)) = ui::convo_hit(self, &lay, pos) {
                        return self.click_message(n, id, kind);
                    }
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
                    // The wheel at the very top asks for older messages.
                    let at_top = up && self.at_top(n);
                    self.scroll_by(n, if up { WHEEL_LINES } else { -WHEEL_LINES });
                    if at_top {
                        return self.request_older(n);
                    }
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
    use crate::commands::{self, COMMANDS_TITLE};
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
            ..ChatSummary::blank(n)
        }
    }

    fn online_app() -> App {
        online(App::new(clock()))
    }

    /// Teams signed in with two chats: "a" (2 unread) and "b".
    fn online(mut app: App) -> App {
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
                    client_id: first_id.clone(),
                    reply_to: None,
                    mentions: vec![],
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
            ..ChatMessage::blank(Network::Teams)
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
        // q is not a quit key any more: :quit is.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit());
        colon(&mut app, "quit");
        assert!(app.should_quit());
        // q while typing is text.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit());
        assert_eq!(app.active().draft(), "q");
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
    fn paste_keeps_its_line_breaks_in_the_draft() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        app.handle_paste("one\ntwo");
        assert_eq!(app.active().draft(), "one\ntwo");
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
    fn mouse_click_tab() {
        let mut app = online_app();
        let tabs = ui::tabs(&app, 80);
        app.handle_mouse(click(tabs[1].x + 1, 0));
        assert_eq!(app.active, Network::WhatsApp);
        app.handle_mouse(click(tabs[0].x + 1, 0));
        assert_eq!(app.active, Network::Teams);
    }

    fn open_chat_cmd(id: &str) -> (Network, Command) {
        (Network::Teams, Command::OpenChat { chat_id: id.into() })
    }

    fn mark_read_cmd(id: &str) -> (Network, Command) {
        (Network::Teams, Command::MarkRead { chat_id: id.into() })
    }

    /// Advances the clock by `ms` and runs the timers, as the run loop does.
    fn advance(app: &mut App, ms: i64) -> Out {
        let now = app.clock().now_ms + ms;
        app.tick(now);
        app.fire_timers()
    }

    fn app_with_chats(count: usize) -> App {
        let mut app = App::with_settings(clock(), &settings_with(&[("list", "f", "filter_start")]));
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        let chats = (0..count)
            .map(|i| {
                chat(
                    Network::Teams,
                    &format!("c{i}"),
                    &format!("Chat {i}"),
                    10_000 - i as i64,
                    1,
                )
            })
            .collect();
        app.handle_event(Event::Chats(Network::Teams, chats));
        app
    }

    #[test]
    fn moving_previews_at_once_without_a_command_and_keeps_list_focus() {
        let mut app = online_app();
        assert!(app.handle_key(key(KeyCode::Down)).is_empty());
        assert_eq!(app.active().open.as_deref(), Some("b"));
        assert_eq!(app.active().focus, Focus::List);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.active().open.as_deref(), Some("a"));
    }

    #[test]
    fn open_chat_is_sent_only_after_the_selection_settles() {
        let mut app = app_with_chats(5);
        app.handle_key(key(KeyCode::Down));
        assert!(advance(&mut app, SETTLE_MS - 1).is_empty(), "too early");
        assert_eq!(advance(&mut app, 1), vec![open_chat_cmd("c1")]);
        assert!(advance(&mut app, 50).is_empty(), "sent once");
    }

    #[test]
    fn chats_passed_over_are_never_requested() {
        let mut app = app_with_chats(50);
        for _ in 0..49 {
            app.handle_key(key(KeyCode::Down));
            // Held key repeat: the next move comes well inside the settle time.
            assert!(advance(&mut app, 30).is_empty());
        }
        assert_eq!(app.active().open.as_deref(), Some("c49"));
        assert_eq!(advance(&mut app, SETTLE_MS), vec![open_chat_cmd("c49")]);
    }

    #[test]
    fn same_chat_is_not_requested_twice_in_a_row() {
        let mut app = app_with_chats(3);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(advance(&mut app, SETTLE_MS), vec![open_chat_cmd("c1")]);
        // Away and back before the other settles: nothing new happened to c1.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Up));
        let out = advance(&mut app, SETTLE_MS);
        assert!(!out.contains(&open_chat_cmd("c1")), "{out:?}");
        // Enter on the already requested chat does not ask again either.
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(!out.contains(&open_chat_cmd("c1")), "{out:?}");
    }

    #[test]
    fn read_timer_marks_read_after_two_seconds_and_clears_the_badge() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Up)); // preview "a", which has 2 unread
        assert_eq!(app.active().unread(), 2, "previewing alone keeps the badge");
        let out = advance(&mut app, READ_AFTER_MS - 1);
        assert!(!out.contains(&mark_read_cmd("a")));
        let out = advance(&mut app, 1);
        assert!(out.contains(&mark_read_cmd("a")), "{out:?}");
        assert_eq!(app.active().unread(), 0);
        assert!(!advance(&mut app, 5_000).contains(&mark_read_cmd("a")));
        assert_eq!(app.next_deadline(), None, "no timer left, no wakeups");
    }

    #[test]
    fn passing_a_chat_over_does_not_mark_it_read() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Up)); // a
        advance(&mut app, 1_000);
        app.handle_key(key(KeyCode::Down)); // b; the read timer restarts for b
        let out = advance(&mut app, READ_AFTER_MS - 1);
        assert!(!out.contains(&mark_read_cmd("a")));
        assert_eq!(app.active().chats[0].unread, 2);
    }

    #[test]
    fn incoming_message_in_a_previewed_chat_does_not_mark_read() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Up));
        let m = ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: "m1".into(),
            ts: 3000,
            sender: "Alex Example".into(),
            from_me: false,
            text: "ping".into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        };
        let out = app.handle_event(Event::Message(m));
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(app.active().unread(), 2);
    }

    #[test]
    fn enter_on_a_preview_replies_and_marks_read_once() {
        let mut app = app_with_chats(3);
        app.handle_key(key(KeyCode::Down));
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(out, vec![open_chat_cmd("c1"), mark_read_cmd("c1")]);
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(app.active().unread(), 2, "c0 and c2 stay unread");
        // The pending timers died with the commit.
        assert!(advance(&mut app, 10_000).is_empty());
        // Esc leaves the chat; Enter on it again asks the backend again (the manual catch-up
        // while NOT LIVE) but sends no MarkRead: nothing is unread, so no Graph call for it.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![open_chat_cmd("c1")]
        );
    }

    /// Under 60 columns the list hides the conversation, so a preview there is not on screen and
    /// must never count as read (WhatsApp would send read receipts for unseen messages).
    #[test]
    fn narrow_list_preview_is_never_marked_read() {
        let mut app = online_app();
        app.resize(50, 20);
        app.handle_key(key(KeyCode::Up)); // preview "a", 2 unread
        assert_eq!(app.active().read_due, None, "no timer for a hidden pane");
        let out = advance(&mut app, 10_000);
        assert!(!out.contains(&mark_read_cmd("a")), "{out:?}");
        assert_eq!(app.active().unread(), 2);
        // Enter brings the conversation up and reads it.
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(out.contains(&mark_read_cmd("a")), "{out:?}");
    }

    /// A terminal resized below the narrow limit stops the timer; widening restarts it.
    #[test]
    fn resize_stops_and_restarts_the_read_timer() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Up));
        assert!(app.active().read_due.is_some());
        advance(&mut app, 1_500);
        app.resize(50, 20);
        assert_eq!(app.active().read_due, None);
        assert!(!advance(&mut app, 10_000).contains(&mark_read_cmd("a")));
        app.resize(100, 30);
        assert!(app.active().read_due.is_some(), "restarted from zero");
        assert!(!advance(&mut app, READ_AFTER_MS - 1).contains(&mark_read_cmd("a")));
        assert!(advance(&mut app, 1).contains(&mark_read_cmd("a")));
    }

    /// Preview, then Tab to the other network inside 2 s: the hidden tab's timer must not fire.
    #[test]
    fn preview_then_tab_away_is_not_marked_read() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Up)); // preview "a" on Teams
        advance(&mut app, 1_000);
        app.handle_key(key(KeyCode::Tab)); // WhatsApp
        let out = advance(&mut app, 10_000);
        assert!(!out.contains(&mark_read_cmd("a")), "{out:?}");
        assert_eq!(app.ns(Network::Teams).unread(), 2);
        assert_eq!(app.next_deadline(), None, "no timer left running hidden");
        // Back on the tab the pane is shown again: the 2 s start over, then it is read.
        app.handle_key(key(KeyCode::BackTab));
        assert!(!advance(&mut app, READ_AFTER_MS - 1).contains(&mark_read_cmd("a")));
        assert!(advance(&mut app, 1).contains(&mark_read_cmd("a")));
    }

    /// The footer says "reply" only when a conversation is really on screen.
    #[test]
    fn convo_is_visible_only_on_the_active_tab_and_when_it_fits() {
        let mut app = online_app();
        assert!(app.convo_visible(Network::Teams));
        assert!(!app.convo_visible(Network::WhatsApp));
        app.resize(50, 20);
        assert!(
            !app.convo_visible(Network::Teams),
            "list only under 60 columns"
        );
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.convo_visible(Network::Teams), "composer focus shows it");
    }

    /// Esc from the chat, a refresh and a reconnect each make the next Enter ask the backend
    /// again; Enter on a chat with nothing unread sends no MarkRead.
    #[test]
    fn requested_is_cleared_when_leaving_refreshing_or_reconnecting() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Down)); // b, nothing unread
        assert_eq!(advance(&mut app, SETTLE_MS), vec![open_chat_cmd("b")]);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Vec::new());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![open_chat_cmd("b")]
        );
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![open_chat_cmd("b")]
        );
        // Refresh from the list.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            vec![(Network::Teams, Command::Refresh)]
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![open_chat_cmd("b")]
        );
        // A status change back to online.
        app.handle_key(key(KeyCode::Esc));
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![open_chat_cmd("b")]
        );
    }

    #[test]
    fn sending_marks_a_previewed_chat_read() {
        let mut app = app_with_chats(2);
        app.handle_key(key(KeyCode::Down));
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(out.contains(&mark_read_cmd("c1")));
        type_text(&mut app, "hi");
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(out[0].1, Command::Send { .. }));
        assert!(!app.active().previewing);
        assert_eq!(app.active().chats[1].unread, 0);
    }

    #[test]
    fn mouse_click_previews_and_keeps_list_focus() {
        let mut app = online_app();
        let tabs = ui::tabs(&app, 80);
        app.handle_mouse(click(tabs[1].x + 1, 0));
        assert_eq!(app.active, Network::WhatsApp);
        app.handle_mouse(click(tabs[0].x + 1, 0));
        assert_eq!(app.active, Network::Teams);
        // Sidebar rows are two lines tall and start below the header and the border.
        assert!(app.handle_mouse(click(5, 4)).is_empty());
        assert_eq!(app.active().open.as_deref(), Some("b"));
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        assert_eq!(app.active().focus, Focus::List);
        assert_eq!(advance(&mut app, SETTLE_MS), vec![open_chat_cmd("b")]);
        // A click in the message box replies to the chat in the pane.
        let lay = app.layout_for(Network::Teams);
        let out = app.handle_mouse(click(lay.composer.x + 2, lay.composer.y + 1));
        assert!(
            out.is_empty(),
            "b is read already: no MarkRead, got {out:?}"
        );
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn home_end_and_page_keys_move_and_preview() {
        let mut app = app_with_chats(30);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.active().open.as_deref(), Some("c29"));
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.active().open.as_deref(), Some("c0"));
        app.handle_key(key(KeyCode::PageDown));
        let rows = app.list_rows(Network::Teams);
        assert!(rows > 1);
        assert_eq!(app.active().open, Some(format!("c{rows}")));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.active().open.as_deref(), Some("c0"));
    }

    #[test]
    fn next_deadline_is_the_earliest_pending_timer() {
        let mut app = app_with_chats(3);
        assert_eq!(app.next_deadline(), None);
        let t0 = app.clock().now_ms;
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.next_deadline(), Some(t0 + SETTLE_MS));
    }

    #[test]
    fn mouse_wheel_scrolls_the_conversation() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        app.ns_mut(Network::Teams).select_on_load = None;
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
                ..ChatMessage::blank(Network::Teams)
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

    // ---- keymap: dispatch, vim, help, user bindings ----

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            modifiers: mods,
            ..key(code)
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        key_mod(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn settings_with(entries: &[(&str, &str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (i, (c, k, a)) in entries.iter().enumerate() {
            s.keys
                .bindings
                .entry((*c).to_string())
                .or_default()
                .insert((*k).to_string(), (*a).to_string());
            s.keys
                .lines
                .insert(((*c).to_string(), (*k).to_string()), i + 1);
        }
        s
    }

    /// `/`, the text, Enter: what a user types into the command bar.
    fn colon(app: &mut App, text: &str) -> Out {
        app.handle_key(key(KeyCode::Char(':')));
        type_text(app, text);
        app.handle_key(key(KeyCode::Enter))
    }

    fn notice_text(app: &App) -> Option<String> {
        app.current_notice().map(|n| n.text.clone())
    }

    #[test]
    fn q_types_in_the_filter_box_and_does_nothing_in_the_list() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "qjr");
        assert!(!app.should_quit());
        assert_eq!(app.active().filter, "qjr");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit());
    }

    #[test]
    fn the_editing_keys_work_in_the_filter_and_the_command_bar() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "sam exa");
        app.handle_key(ctrl('w'));
        assert_eq!(app.active().filter, "sam ");
        app.handle_key(key_mod(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(app.active().filter, "");
        type_text(&mut app, "am");
        app.handle_key(key(KeyCode::Home));
        type_text(&mut app, "s");
        assert_eq!(app.active().filter, "sam");
        assert_eq!(
            app.active().selected.as_deref(),
            Some("b"),
            "the list follows the edited filter"
        );
        app.handle_key(key(KeyCode::Esc));
        // The command bar: Left moves the cursor, typing goes in there, and the list follows.
        app.handle_key(key(KeyCode::Char(':')));
        type_text(&mut app, "seting");
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Left));
        }
        type_text(&mut app, "t");
        assert_eq!(app.palette().unwrap().query, "setting");
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.palette().unwrap().query, "settin");
        app.handle_key(ctrl('u'));
        assert_eq!(app.palette().unwrap().query, "");
        assert_eq!(app.palette_items().len(), commands::COMMANDS.len());
    }

    #[test]
    fn ctrl_c_quits_from_every_context() {
        // Panel
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        app.handle_key(ctrl('c'));
        assert!(app.should_quit(), "panel");
        // Filter, composer and help.
        let build = || {
            online(App::with_settings(
                clock(),
                &settings_with(&[("list", "f", "filter_start")]),
            ))
        };
        let mut app = build();
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.context(), Context::Filter);
        app.handle_key(ctrl('c'));
        assert!(app.should_quit(), "filter");
        let mut app = build();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('c'));
        assert!(app.should_quit(), "composer");
        let mut app = build();
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.context(), Context::Help);
        app.handle_key(ctrl('c'));
        assert!(app.should_quit(), "help");
    }

    #[test]
    fn ctrl_c_can_be_rebound_by_the_user() {
        // The chat list takes Ctrl+C for itself (with q to quit there); elsewhere it quits.
        let s = settings_with(&[("list", "ctrl+c", "refresh"), ("list", "q", "quit")]);
        let mut app = online(App::with_settings(clock(), &s));
        assert_eq!(app.handle_key(ctrl('c')).len(), 1, "refresh");
        assert!(!app.should_quit());
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.context(), Context::Composer);
        app.handle_key(ctrl('c'));
        assert!(app.should_quit());
    }

    #[test]
    fn user_bindings_override_add_and_unbind() {
        let s = settings_with(&[
            ("list", "ctrl+n", "down"),
            ("list", "ctrl+p", "up"),
            ("list", "r", "unbound"),
            ("list", "x", "refresh"),
            ("composer", "ctrl+s", "send"),
        ]);
        let mut app = online(App::with_settings(clock(), &s));
        app.handle_key(ctrl('n'));
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        app.handle_key(ctrl('p'));
        assert_eq!(app.active().selected.as_deref(), Some("a"));
        assert!(
            app.handle_key(key(KeyCode::Char('r'))).is_empty(),
            "r unbound"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            vec![(Network::Teams, Command::Refresh)]
        );
        // Untouched keys of the preset stay.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active().selected.as_deref(), Some("b"));
        // ctrl+s sends from the message box; Enter still does too.
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hello");
        let out = app.handle_key(ctrl('s'));
        assert!(matches!(out[0].1, Command::Send { .. }), "{out:?}");
    }

    #[test]
    fn a_binding_that_would_stop_typing_never_takes_effect() {
        let s = settings_with(&[
            ("filter", "x", "refresh"),
            ("composer", "z", "send"),
            ("list", "f", "filter_start"),
        ]);
        let mut app = online(App::with_settings(clock(), &s));
        app.handle_key(key(KeyCode::Char('f')));
        type_text(&mut app, "x");
        assert_eq!(app.active().filter, "x");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "zz");
        assert_eq!(app.active().draft(), "zz");
    }

    #[test]
    fn a_binding_applies_only_in_its_own_context() {
        // `w` refreshes in message selection, not in the chat list.
        let s = settings_with(&[("select", "w", "refresh")]);
        let mut app = online(App::with_settings(clock(), &s));
        assert!(app.handle_key(key(KeyCode::Char('w'))).is_empty());
        assert_eq!(
            app.keys.lookup(
                Context::Select,
                KeyChord::new(KeyCode::Char('w'), KeyModifiers::NONE)
            ),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn keys_command_opens_the_keys_page_and_takes_no_preset() {
        let mut app = online(App::new(clock()));
        colon(&mut app, "keys");
        assert_eq!(
            app.settings_page().map(|p| p.section),
            Some(crate::settings_view::Section::Keys)
        );
        let mut app = online(App::new(clock()));
        colon(&mut app, "keys vim");
        assert!(app.settings_page().is_none());
        assert_eq!(
            notice_text(&app).as_deref(),
            Some(":keys takes no arguments")
        );
    }

    #[test]
    fn help_opens_from_the_list_and_types_a_question_mark_elsewhere() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.help_open(), Some(0));
        assert_eq!(app.context(), Context::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.help_open(), None, "Esc closes");
        // In the filter box and the message box ? is a character.
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "a?");
        assert_eq!(app.active().filter, "a?");
        assert_eq!(app.help_open(), None);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "why?");
        assert_eq!(app.active().draft(), "why?");
        assert_eq!(app.help_open(), None);
    }

    #[test]
    fn help_closes_with_esc_only_and_focus_is_unchanged() {
        for close in [KeyCode::Char('q'), KeyCode::Char('?'), KeyCode::F(1)] {
            let mut app = online_app();
            app.handle_key(key(KeyCode::Char('?')));
            app.handle_key(key(close));
            assert_eq!(app.help_open(), Some(0), "{close:?} leaves help open");
        }
        {
            let mut app = online_app();
            app.handle_key(key(KeyCode::Down));
            let before = (
                app.active().focus,
                app.active().selected.clone(),
                app.active().open.clone(),
            );
            app.handle_key(key(KeyCode::Char('?')));
            app.handle_key(key(KeyCode::Esc));
            assert_eq!(app.help_open(), None);
            let after = (
                app.active().focus,
                app.active().selected.clone(),
                app.active().open.clone(),
            );
            assert_eq!(before, after);
        }
    }

    #[test]
    fn help_is_modal() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('?')));
        let before = app.active().selected.clone();
        // Tab, j, / and r do nothing while help is open.
        for c in [
            KeyCode::Tab,
            KeyCode::Char('j'),
            KeyCode::Char('/'),
            KeyCode::Char('r'),
            KeyCode::Enter,
        ] {
            assert!(app.handle_key(key(c)).is_empty(), "{c:?}");
        }
        assert_eq!(app.active, Network::Teams);
        assert_eq!(app.active().selected, before);
        assert_eq!(app.active().focus, Focus::List);
        assert_eq!(app.help_open().map(|_| ()), Some(()));
        // Mouse is ignored too.
        assert!(app.handle_mouse(click(5, 4)).is_empty());
        assert_eq!(app.active().selected, before);
        app.handle_paste("pasted");
        assert_eq!(app.active().filter, "");
    }

    #[test]
    fn help_scrolls_within_its_text_and_resize_keeps_it_in_range() {
        let mut app = online_app();
        app.resize(40, 12);
        let total = ui::help_rows(&app).len();
        let view = ui::help_view_rows(&app, Rect::new(0, 0, 40, 12));
        assert!(total > view, "the default keymap needs scrolling at 40x12");
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.help_open(), Some(0), "not above the top");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.help_open(), Some(1));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.help_open(), Some(1 + view - 1));
        for _ in 0..200 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.help_open(), Some(total - view), "stops at the end");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.help_open(),
            Some(total - view - 1),
            "one press scrolls back"
        );
        for _ in 0..200 {
            app.handle_key(key(KeyCode::PageUp));
        }
        assert_eq!(app.help_open(), Some(0));
        // A taller terminal needs less scrolling: the offset is clamped.
        for _ in 0..200 {
            app.handle_key(key(KeyCode::Down));
        }
        app.resize(100, 60);
        let tall = ui::help_view_rows(&app, Rect::new(0, 0, 100, 60));
        assert!(app.help_open().unwrap() <= ui::help_rows(&app).len().saturating_sub(tall));
    }

    #[test]
    fn help_lists_every_action_and_follows_rebinding() {
        let text = |app: &App| -> String {
            ui::help_rows(app)
                .into_iter()
                .map(|r| match r {
                    ui::HelpRow::Title(t) => t,
                    ui::HelpRow::Blank => String::new(),
                    ui::HelpRow::Entry { keys, name, .. } => format!("{keys} {name}"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let app = online_app();
        let t = text(&app);
        for name in [
            "Quit",
            "Next network",
            "Down",
            "Accept",
            "Send",
            "Settings",
            "Help",
            "Back",
            "Command bar",
        ] {
            assert!(t.contains(name), "{name} missing:\n{t}");
        }
        let s = settings_with(&[("list", "ctrl+n", "down"), ("list", "j", "unbound")]);
        let t = text(&online(App::with_settings(clock(), &s)));
        let list = &t[t.find("Chat list").unwrap()..t.find("Message box").unwrap()];
        assert!(list.contains("Ctrl+N Down"), "{list}");
        assert!(!list.contains("j Down"), "{list}");
    }

    // ---- CodeRabbit review comments on PR 1 ----

    /// Reopening a chat before the backend answered a send must keep the pending message, and a
    /// failed one must keep its NOT SENT reason (CodeRabbit 7).
    #[test]
    fn history_keeps_unresolved_local_sends() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "first");
        app.handle_key(key(KeyCode::Enter));
        let first = format!("local-{}-1", app.clock().now_ms);
        let history = |id: &str, ts: i64| ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: id.into(),
            ts,
            sender: "Alex Example".into(),
            text: "old".into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        };
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![history("h1", 1000), history("h2", 2000)],
        });
        let list = &app.ns(Network::Teams).messages["a"];
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["h1", "h2", first.as_str()],
            "pending stays, after the history"
        );
        assert_eq!(list[2].state, MessageState::Pending);
        // The failure callback still finds it.
        app.handle_event(Event::MessageFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            client_id: first.clone(),
            error: "boom".into(),
        });
        assert_eq!(
            app.ns(Network::Teams).messages["a"][2].state,
            MessageState::Failed("boom".into())
        );
        // History again: the failed local message and its reason survive.
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![history("h1", 1000), history("h2", 2000)],
        });
        assert_eq!(
            app.ns(Network::Teams).messages["a"][2].state,
            MessageState::Failed("boom".into())
        );
    }

    /// A message the backend already has (same id) comes from the batch, not twice.
    #[test]
    fn history_batch_wins_over_a_held_copy_and_older_messages_stay() {
        let mut app = online_app();
        let msg = |id: &str, ts: i64, text: &str| ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: id.into(),
            ts,
            sender: "Alex Example".into(),
            text: text.into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        };
        let send = |app: &mut App, m: Vec<ChatMessage>| {
            app.handle_event(Event::Messages {
                network: Network::Teams,
                chat_id: "a".into(),
                messages: m,
            })
        };
        send(
            &mut app,
            vec![
                msg("m1", 1000, "one"),
                msg("m2", 2000, "two"),
                msg("m3", 3000, "three"),
            ],
        );
        // A newer batch from m2 on: m1 (older, e.g. loaded earlier) stays, m2 is replaced.
        send(
            &mut app,
            vec![
                msg("m2", 2000, "two edited"),
                msg("m3", 3000, "three"),
                msg("m4", 4000, "four"),
            ],
        );
        let list = &app.ns(Network::Teams).messages["a"];
        let seen: Vec<(&str, &str)> = list
            .iter()
            .map(|m| (m.id.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("m1", "one"),
                ("m2", "two edited"),
                ("m3", "three"),
                ("m4", "four")
            ]
        );
        // An empty batch for a chat with nothing newer keeps what is held.
        send(&mut app, vec![]);
        assert_eq!(app.ns(Network::Teams).messages["a"].len(), 4);
    }

    /// Open Teams chat, switch to WhatsApp: a Teams message must not be read behind the user's
    /// back (CodeRabbit 8). It counts as read two seconds after the pane shows again.
    #[test]
    fn nothing_is_read_while_the_conversation_is_not_on_screen() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter)); // a: committed, read
        app.handle_key(key(KeyCode::Tab)); // WhatsApp is active now
        let m = ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: "m1".into(),
            ts: 3000,
            sender: "Alex Example".into(),
            text: "ping".into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        };
        let out = app.handle_event(Event::Message(m));
        assert!(out.is_empty(), "{out:?}");
        let out = app.handle_event(Event::ChatUpdated(chat(
            Network::Teams,
            "a",
            "Alex Example",
            3000,
            1,
        )));
        assert!(
            out.is_empty(),
            "chat-list update must not read it either: {out:?}"
        );
        assert_eq!(app.ns(Network::Teams).unread(), 1);
        assert!(
            !advance(&mut app, 10_000).contains(&mark_read_cmd("a")),
            "hidden tab: no timer"
        );
        // Back on the tab: the pane is on screen, so the read timer starts.
        app.handle_key(key(KeyCode::BackTab));
        assert!(!advance(&mut app, READ_AFTER_MS - 1).contains(&mark_read_cmd("a")));
        assert!(advance(&mut app, 1).contains(&mark_read_cmd("a")));
        assert_eq!(app.ns(Network::Teams).unread(), 0);
    }

    /// Narrow terminal: `open` is retained but the list hides the pane, so it is not read.
    #[test]
    fn narrow_list_does_not_read_the_retained_open_chat() {
        let mut app = online_app();
        app.resize(50, 20);
        app.handle_key(key(KeyCode::Enter)); // composer focus shows the conversation
        app.handle_key(key(KeyCode::Esc)); // back to the list: conversation hidden
        let m = ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: "m1".into(),
            ts: 3000,
            sender: "Alex Example".into(),
            text: "ping".into(),
            state: MessageState::Sent,
            ..ChatMessage::blank(Network::Teams)
        };
        assert!(app.handle_event(Event::Message(m)).is_empty());
        let out = app.handle_event(Event::ChatUpdated(chat(
            Network::Teams,
            "a",
            "Alex Example",
            3000,
            1,
        )));
        assert!(out.is_empty(), "{out:?}");
        assert!(!advance(&mut app, 10_000).contains(&mark_read_cmd("a")));
        assert_eq!(app.ns(Network::Teams).unread(), 1);
        // Enter shows it and reads it.
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(out.contains(&mark_read_cmd("a")), "{out:?}");
    }

    /// Live updates must not undo the user's wheel scrolling in the sidebar (CodeRabbit 9).
    #[test]
    fn live_updates_keep_the_wheel_scrolled_sidebar() {
        let mut app = app_with_chats(40);
        let wheel = |app: &mut App, kind| {
            app.handle_mouse(MouseEvent {
                kind,
                column: 5,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })
        };
        for _ in 0..6 {
            wheel(&mut app, MouseEventKind::ScrollDown);
        }
        assert_eq!(app.active().list_top, 6);
        assert_eq!(
            app.active().selected.as_deref(),
            Some("c0"),
            "selection stays on the first chat"
        );
        app.handle_event(Event::Notice {
            network: Network::Teams,
            text: "hello".into(),
            is_error: false,
        });
        assert_eq!(app.active().list_top, 6, "notice");
        app.handle_event(Event::ChatUpdated(chat(
            Network::Teams,
            "c9",
            "Chat 9",
            5_000,
            3,
        )));
        assert_eq!(app.active().list_top, 6, "chat update");
        let m = ChatMessage {
            network: Network::Teams,
            chat_id: "c9".into(),
            id: "m1".into(),
            ts: 1,
            ..ChatMessage::blank(Network::Teams)
        };
        app.handle_event(Event::Message(m));
        assert_eq!(app.active().list_top, 6, "message");
        // The user moving the selection still brings it into view.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active().selected.as_deref(), Some("c1"));
        assert!(app.active().list_top <= 1);
        // A list that got shorter pulls the viewport back into range, not to the selection.
        app.handle_event(Event::Chats(
            Network::Teams,
            (0..3)
                .map(|i| chat(Network::Teams, &format!("c{i}"), "x", 10_000 - i, 0))
                .collect(),
        ));
        assert_eq!(app.active().list_top, 0);
    }

    /// Typing in the filter changes what is listed, so that one does follow the selection.
    #[test]
    fn typing_in_the_filter_still_scrolls_to_the_selection() {
        let mut app = app_with_chats(40);
        for _ in 0..6 {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 4,
                modifiers: KeyModifiers::NONE,
            });
        }
        app.handle_key(key(KeyCode::Char('f')));
        type_text(&mut app, "chat 3");
        assert_eq!(app.active().selected.as_deref(), Some("c3"));
        let pos = app.active().sel_pos(&app.active().visible());
        let ns = app.active();
        assert!(pos >= ns.list_top && pos < ns.list_top + app.list_rows(Network::Teams));
    }

    /// A closed backend channel must not leave the message pending (CodeRabbit 10).
    #[test]
    fn undelivered_send_fails_the_pending_message() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hello");
        let out = app.handle_key(key(KeyCode::Enter));
        let (net, cmd) = out.into_iter().next().unwrap();
        assert_eq!(
            app.ns(Network::Teams).messages["a"][0].state,
            MessageState::Pending
        );
        app.command_undelivered(net, &cmd);
        assert_eq!(
            app.ns(Network::Teams).messages["a"][0].state,
            MessageState::Failed("Backend stopped: restart chatcli.".into())
        );
        let n = app.current_notice().unwrap();
        assert!(n.is_error && n.text.contains("Backend stopped"), "{n:?}");
        // Other commands only get the notice.
        let mut app = online_app();
        app.command_undelivered(Network::Teams, &Command::Refresh);
        assert_eq!(app.notices().len(), 1);
    }

    // ---- start-up notices ----

    #[test]
    fn with_ctx_sums_up_settings_problems_in_one_notice() {
        use chatcli_core::media::MediaCache;
        use chatcli_core::store::Store;
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("chatcli-tui-app-test-{}", std::process::id()));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let mut ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
        let clean = App::with_ctx(clock(), &ctx);
        assert!(clean.current_notice().is_none(), "no problems, no notice");
        ctx.settings = Arc::new(settings_with(&[
            ("list", "ctrl+n", "lst_down"),
            ("filter", "x", "refresh"),
        ]));
        ctx.startup_notices = vec![StartupNotice {
            text: "settings.toml line 3: [layout] density: bad".into(),
            is_error: false,
        }];
        let app = App::with_ctx(clock(), &ctx);
        let n = app.current_notice().expect("one notice");
        assert!(n.global && n.is_error, "{n:?}");
        assert!(
            n.text
                .starts_with("settings.toml line 3: [layout] density: bad (+2 more"),
            "{n:?}"
        );
        assert!(n.text.contains("chatcli settings check"), "{n:?}");
        assert_eq!(app.notices().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_start_up_problem_is_shown_whole() {
        use chatcli_core::media::MediaCache;
        use chatcli_core::store::Store;
        let dir =
            std::env::temp_dir().join(format!("chatcli-tui-app-test1-{}", std::process::id()));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let mut ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
        ctx.settings = std::sync::Arc::new(settings_with(&[("list", "ctrl+n", "lst_down")]));
        let app = App::with_ctx(clock(), &ctx);
        let n = app.current_notice().unwrap();
        assert!(
            n.text.contains("lst_down") && !n.text.contains("more"),
            "{n:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- composer editing, paste, limits ----

    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    const THUMB: &str = "\u{1F44D}\u{1F3FD}";

    /// Teams, chat "a" open with the message box focused.
    fn typing_app() -> App {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Enter));
        app
    }

    fn cursor_of(app: &App) -> usize {
        app.active().composer().map_or(0, |c| c.cursor())
    }

    #[test]
    fn arrow_keys_edit_in_the_middle_of_the_draft() {
        let mut app = typing_app();
        type_text(&mut app, "helo");
        app.handle_key(key(KeyCode::Left));
        type_text(&mut app, "l");
        assert_eq!(app.active().draft(), "hello");
        assert_eq!(cursor_of(&app), 4);
        app.handle_key(key(KeyCode::Home));
        type_text(&mut app, ">");
        assert_eq!(app.active().draft(), ">hello");
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.active().draft(), ">hell");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.active().draft(), ">ell");
        app.handle_key(ctrl('w'));
        assert_eq!(app.active().draft(), "ell", "word delete before the cursor");
        app.handle_key(key(KeyCode::End));
        app.handle_key(ctrl('u'));
        assert_eq!(app.active().draft(), "");
    }

    #[test]
    fn word_keys_move_by_word() {
        let mut app = typing_app();
        type_text(&mut app, "one two three");
        app.handle_key(key_mod(KeyCode::Left, KeyModifiers::CONTROL));
        app.handle_key(key_mod(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(cursor_of(&app), 4);
        app.handle_key(key_mod(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(cursor_of(&app), 8);
    }

    #[test]
    fn backspace_removes_a_whole_emoji_in_the_app() {
        for emoji in [FAMILY, THUMB, "\u{1F1FA}\u{1F1F3}", "\u{2764}\u{FE0F}"] {
            let mut app = typing_app();
            type_text(&mut app, "hi");
            for c in emoji.chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            assert_eq!(app.active().draft(), format!("hi{emoji}"));
            app.handle_key(key(KeyCode::Backspace));
            assert_eq!(app.active().draft(), "hi", "{emoji:?}");
        }
    }

    #[test]
    fn drafts_keep_their_cursor_per_chat() {
        let mut app = typing_app();
        type_text(&mut app, "abc");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "zz");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.active().draft(), "abc");
        assert_eq!(cursor_of(&app), 2, "the cursor stayed where it was");
    }

    fn keys_of(s: &str) -> Vec<KeyEvent> {
        s.chars()
            .flat_map(|c| {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                let down = key(code);
                [
                    down,
                    KeyEvent {
                        kind: KeyEventKind::Release,
                        ..down
                    },
                ]
            })
            .collect()
    }

    /// Windows reports a paste as keys, every line break as Enter. It must never send.
    #[test]
    fn a_paste_that_arrives_as_keys_never_sends() {
        let mut app = typing_app();
        let out = app.handle_key_burst(keys_of("one\ntwo\nthree"));
        assert!(out.is_empty(), "no command went out: {out:?}");
        assert_eq!(app.active().draft(), "one\ntwo\nthree");
        assert!(!app.ns(Network::Teams).messages.contains_key("a"));
        // Enter on its own still sends.
        let out = app.handle_key_burst(vec![key(KeyCode::Enter)]);
        assert!(out.iter().any(|(_, c)| matches!(c, Command::Send { .. })));
    }

    #[test]
    fn a_paste_as_keys_into_the_filter_is_flat_and_outside_a_box_it_is_dropped() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key_burst(keys_of("sam"));
        assert_eq!(app.active().filter, "sam");
        app.handle_key(key(KeyCode::Esc));
        // In the chat list, this would quit chatcli if it ran as keys.
        let out = app.handle_key_burst(keys_of("qq\nq"));
        assert!(out.is_empty());
        assert!(!app.should_quit());
        assert_eq!(app.active().visible().len(), 2, "nothing was filtered");
    }

    #[test]
    fn altgr_characters_type_in_the_message_box_and_the_filter() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let mut app = typing_app();
        for c in ['@', '€', '{', '\\'] {
            app.handle_key(key_mod(KeyCode::Char(c), altgr));
        }
        assert_eq!(app.active().draft(), "@€{\\");
        // The filter box.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key_mod(KeyCode::Char('€'), altgr));
        assert_eq!(app.active().filter, "€");
        // A plain Alt chord is still not text.
        let mut app = typing_app();
        app.handle_key(key_mod(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(app.active().draft(), "");
    }

    #[test]
    fn ctrl_c_quits_with_caps_lock_on() {
        // Windows reports Ctrl+C as an upper-case C, with no Shift, while Caps Lock is on.
        let mut app = online_app();
        app.handle_key(key_mod(KeyCode::Char('C'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn paste_inserts_at_the_cursor_and_never_sends() {
        let mut app = typing_app();
        type_text(&mut app, "ab");
        app.handle_key(key(KeyCode::Left));
        app.handle_paste("one\r\ntwo\nthree");
        assert_eq!(app.active().draft(), "aone\ntwo\nthreeb");
        assert_eq!(cursor_of(&app), 14);
        assert!(
            !app.ns(Network::Teams).messages.contains_key("a"),
            "nothing was sent or even queued"
        );
    }

    #[test]
    fn pasted_escape_sequences_never_reach_the_draft() {
        let mut app = typing_app();
        app.handle_paste("x\u{1b}[2Jy\u{7}\u{0}z\ttab");
        assert_eq!(app.active().draft(), "x[2Jyz    tab");
        assert!(!app.active().draft().contains('\u{1b}'));
        // The filter and the command box stay one line.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_paste("a\nb");
        assert_eq!(app.active().filter, "a b");
    }

    #[test]
    fn paste_keeps_emoji_whole() {
        let mut app = typing_app();
        app.handle_paste(&format!("{THUMB} and \u{1F1FA}\u{1F1F3}"));
        assert_eq!(
            app.active().draft(),
            format!("{THUMB} and \u{1F1FA}\u{1F1F3}")
        );
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.active().draft(), format!("{THUMB} and "));
    }

    #[test]
    fn an_oversized_paste_is_cut_with_a_notice() {
        let mut app = typing_app();
        app.handle_paste(&"x".repeat(DRAFT_MAX_BYTES + 500));
        assert_eq!(app.active().draft().len(), DRAFT_MAX_BYTES);
        assert!(notice_text(&app).unwrap().starts_with("Paste cut"));
    }

    #[test]
    fn alt_enter_adds_a_line_and_enter_sends_it_whole() {
        let mut app = typing_app();
        type_text(&mut app, "one");
        app.handle_key(key_mod(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut app, "two");
        app.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut app, "three");
        app.handle_key(ctrl('j'));
        type_text(&mut app, "four");
        assert_eq!(app.active().draft(), "one\ntwo\nthree\nfour");
        let out = app.handle_key(key(KeyCode::Enter));
        match &out[0].1 {
            Command::Send { text, .. } => assert_eq!(text, "one\ntwo\nthree\nfour"),
            other => panic!("{other:?}"),
        }
        assert_eq!(app.active().draft(), "");
    }

    #[test]
    fn a_draft_over_the_limit_is_refused_with_a_notice_and_kept() {
        let mut app = typing_app();
        app.handle_paste(&"a".repeat(limits::TEAMS_TEXT_MAX_BYTES + 1));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "not sent");
        let n = notice_text(&app).unwrap();
        assert!(
            n.contains("TOO LONG") && n.contains("Teams") && n.contains("shorten"),
            "{n}"
        );
        assert_eq!(
            app.active().draft().len(),
            limits::TEAMS_TEXT_MAX_BYTES + 1,
            "draft kept"
        );
        assert!(!app.ns(Network::Teams).messages.contains_key("a"));
        // Exactly at the limit still sends.
        let mut app = typing_app();
        app.handle_paste(&"a".repeat(limits::TEAMS_TEXT_MAX_BYTES));
        assert_eq!(app.handle_key(key(KeyCode::Enter)).len(), 1);
    }

    #[test]
    fn escaping_makes_the_teams_limit_bite_earlier() {
        // Every `<` becomes 4 bytes (`&lt;`) in the Teams body.
        let mut app = typing_app();
        app.handle_paste(&"<".repeat(limits::TEAMS_TEXT_MAX_BYTES / 4 + 1));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn whatsapp_limit_counts_characters() {
        let text = "\u{1F600}".repeat(limits::WHATSAPP_TEXT_MAX_CHARS + 1);
        assert!(too_long(Network::WhatsApp).contains("WhatsApp"));
        assert!(!limits::text_fits(Network::WhatsApp, &text));
        assert!(limits::text_fits(
            Network::WhatsApp,
            &"\u{1F600}".repeat(limits::WHATSAPP_TEXT_MAX_CHARS)
        ));
    }

    // ---- :shortcode completion ----

    fn popup_labels(app: &App) -> Vec<String> {
        app.popup()
            .map(|p| p.items.iter().map(|i| i.insert.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn colon_thu_opens_the_popup_and_enter_inserts_thumbs_up() {
        let mut app = typing_app();
        type_text(&mut app, "ok :thu");
        assert_eq!(popup_labels(&app)[0], "\u{1F44D}");
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(out.is_empty(), "Enter inserted, it did not send");
        assert_eq!(app.active().draft(), "ok \u{1F44D}");
        assert!(app.popup().is_none());
        assert_eq!(app.active, Network::Teams);
        // Typing on goes straight after the emoji.
        type_text(&mut app, "!");
        assert_eq!(app.active().draft(), "ok \u{1F44D}!");
    }

    #[test]
    fn enter_inserts_instead_of_sending_while_the_popup_is_open() {
        let mut app = typing_app();
        type_text(&mut app, ":thu");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.active().draft(), "\u{1F44D}");
        // With the popup closed Enter sends again.
        assert_eq!(app.handle_key(key(KeyCode::Enter)).len(), 1);
    }

    #[test]
    fn up_down_choose_and_esc_closes_without_leaving_the_box() {
        let mut app = typing_app();
        type_text(&mut app, ":thu");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.popup().unwrap().selected, 1);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.popup().unwrap().selected,
            app.popup().unwrap().items.len() - 1
        );
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.popup().unwrap().selected, 0);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup().is_none());
        assert_eq!(
            app.active().focus,
            Focus::Composer,
            "Esc only closed the popup"
        );
        assert_eq!(app.active().draft(), ":thu");
        // Escape again leaves the box as before.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.active().focus, Focus::List);
    }

    #[test]
    fn times_and_urls_do_not_open_the_popup() {
        for text in [
            "10:30",
            "see https://x",
            "at 9:45",
            "http://example.com:8080",
        ] {
            let mut app = typing_app();
            type_text(&mut app, text);
            assert!(app.popup().is_none(), "{text}");
            assert_eq!(app.active().draft(), text);
        }
    }

    #[test]
    fn popup_follows_the_text_and_closes_when_it_stops_matching() {
        let mut app = typing_app();
        type_text(&mut app, "hi :sm");
        assert!(app.popup().is_some());
        type_text(&mut app, "zzzz");
        assert!(app.popup().is_none(), "no emoji matches :smzzzz");
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.popup().is_some(), "back to :sm");
        assert_eq!(
            app.popup().unwrap().title,
            "Emoji",
            "not at the start: no commands"
        );
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        assert!(app.popup().is_none(), "the cursor is before the token");
    }

    #[test]
    fn completion_in_the_middle_of_a_draft_replaces_only_the_token() {
        let mut app = typing_app();
        type_text(&mut app, "a :thu b");
        for _ in 0..2 {
            app.handle_key(key(KeyCode::Left));
        }
        assert!(app.popup().is_some());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.active().draft(), "a \u{1F44D} b");
    }

    #[test]
    fn popup_never_opens_outside_the_message_box() {
        let mut app = online_app();
        type_text(&mut app, ":thu"); // list focus: these are commands, not text
        assert!(app.popup().is_none());
    }

    // ---- emoji picker ----

    /// An app with the store behind it, so recents persist.
    fn app_with_store() -> (App, Store) {
        use chatcli_core::media::MediaCache;
        let dir = std::env::temp_dir().join(format!(
            "chatcli-tui-picker-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let store = Store::open_in_memory().unwrap();
        let ctx = UiCtx::new(store.clone(), media);
        let _ = std::fs::remove_dir_all(&dir);
        (online(App::with_ctx(clock(), &ctx)), store)
    }

    #[test]
    fn signed_in_tabs_come_first_and_the_keys_follow_the_order() {
        let mut app = App::new(clock());
        assert_eq!(app.tab_order(), [Network::Teams, Network::WhatsApp]);
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        app.handle_event(Event::Status(
            Network::WhatsApp,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        assert_eq!(app.tab_order(), [Network::WhatsApp, Network::Teams]);
        // Alt+1 is the first tab shown, Alt+2 the second.
        app.handle_key(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.active, Network::WhatsApp);
        app.handle_key(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.active, Network::Teams);
        // Tab wraps through the shown order.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active, Network::WhatsApp);
        // Both signed in: the fixed order again.
        app.handle_event(Event::Status(Network::Teams, Status::Connecting));
        assert_eq!(app.tab_order(), [Network::Teams, Network::WhatsApp]);
    }

    #[test]
    fn the_next_start_opens_on_the_tab_used_last() {
        let (mut app, store) = app_with_store();
        assert_eq!(app.active, Network::Teams, "nothing remembered yet");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active, Network::WhatsApp);
        assert_eq!(store.meta(LAST_TAB).unwrap().as_deref(), Some("whatsapp"));
        let media = chatcli_core::media::MediaCache::open(
            &std::env::temp_dir().join(format!("chatcli-tui-lasttab-{}", std::process::id())),
            &Settings::default().media,
        )
        .unwrap();
        let again = App::with_ctx(clock(), &UiCtx::new(store.clone(), media));
        assert_eq!(again.active, Network::WhatsApp);
    }

    #[test]
    fn ctrl_e_opens_the_picker_and_enter_inserts_the_selected_emoji() {
        let (mut app, store) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hi ");
        app.handle_key(ctrl('e'));
        assert!(app.picker().is_some());
        assert_eq!(app.context(), Context::Popup);
        // The first category starts at the first emoji; arrows move, Enter chooses.
        app.handle_key(key(KeyCode::Right));
        let want = app.picker().unwrap().items(app.recents())[1]
            .as_str()
            .to_string();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), format!("hi {want}"));
        assert_eq!(store.recent_emoji(5).unwrap(), vec![want.clone()], "saved");
        assert_eq!(app.recents(), [want]);
    }

    #[test]
    fn picker_closes_with_esc_and_keeps_the_draft() {
        let (mut app, _) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "x");
        app.handle_key(ctrl('e'));
        type_text(&mut app, "q"); // search text, not a draft edit
        assert_eq!(app.picker().unwrap().query, "q");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), "x");
        assert_eq!(app.active().focus, Focus::Composer);
    }

    #[test]
    fn picker_search_narrows_the_grid_by_name() {
        let (mut app, _) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        let all = app.picker().unwrap().items(app.recents()).len();
        type_text(&mut app, "thumbs up");
        let hits = app.picker().unwrap().items(app.recents());
        assert!(!hits.is_empty() && hits.len() < all);
        assert_eq!(hits[0].as_str(), "\u{1F44D}");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.active().draft(), "\u{1F44D}");
        // Backspace edits the search.
        app.handle_key(ctrl('e'));
        type_text(&mut app, "abc");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.picker().unwrap().query, "ab");
    }

    #[test]
    fn picker_pages_through_categories_with_pgup_pgdn() {
        let (mut app, _) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        let start = app.picker().unwrap().cat;
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.picker().unwrap().cat, start + 1);
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.picker().unwrap().cat, 9, "wraps to the last");
    }

    #[test]
    fn recents_show_first_and_the_picker_opens_on_them() {
        let (app, store) = app_with_store();
        store.touch_recent_emoji("\u{1F389}", 1).unwrap();
        // A new app reads the recents from the store at start.
        use chatcli_core::media::MediaCache;
        let dir = std::env::temp_dir().join(format!("chatcli-tui-picker-r-{}", std::process::id()));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let ctx = UiCtx::new(store.clone(), media);
        let mut app2 = online(App::with_ctx(clock(), &ctx));
        let _ = std::fs::remove_dir_all(&dir);
        app2.handle_key(key(KeyCode::Enter));
        app2.handle_key(ctrl('e'));
        assert_eq!(app2.picker().unwrap().cat, 0, "recent category first");
        assert_eq!(
            app2.picker().unwrap().items(app2.recents())[0].as_str(),
            "\u{1F389}"
        );
        drop(app);
    }

    #[test]
    fn shortcode_completion_also_counts_as_recent() {
        let (mut app, store) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, ":thu");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            store.recent_emoji(5).unwrap(),
            vec!["\u{1F44D}".to_string()]
        );
    }

    #[test]
    fn reaction_picker_sends_react_with_the_chosen_emoji() {
        let mut app = typing_app();
        app.handle_key(key(KeyCode::Esc));
        app.open_reaction_picker("a", "m1");
        assert_eq!(app.context(), Context::Popup);
        type_text(&mut app, "thumbs up");
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            out,
            vec![(
                Network::Teams,
                Command::React {
                    chat_id: "a".into(),
                    message_id: "m1".into(),
                    emoji: "\u{1F44D}".into(),
                    remove: false,
                }
            )]
        );
        assert!(app.picker().is_none());
        assert_eq!(
            app.active().draft(),
            "",
            "a reaction never touches the draft"
        );
    }

    #[test]
    fn picker_swallows_other_keys_and_ctrl_c_still_quits() {
        let (mut app, _) = app_with_store();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        app.handle_key(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(
            app.active,
            Network::Teams,
            "Alt+2 does not switch network under the picker"
        );
        app.handle_key(key(KeyCode::F(2)));
        assert!(
            app.settings_page().is_none(),
            "F2 does not open settings under the picker"
        );
        app.handle_key(ctrl('c'));
        assert!(app.should_quit());
    }

    #[test]
    fn picker_mouse_click_chooses_and_wheel_scrolls() {
        let (mut app, _) = app_with_store();
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        let g = app.picker_grid().unwrap();
        let wheel = |kind| MouseEvent {
            kind,
            column: g.grid.x,
            row: g.grid.y,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.picker().unwrap().top, 1);
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.picker().unwrap().top, 0);
        // Click the third cell of the second row.
        let col = g.grid.x + 2 * ui::PICKER_CELL as u16;
        let row = g.grid.y + 1;
        let want = app.picker().unwrap().items(app.recents())[g.cols + 2]
            .as_str()
            .to_string();
        app.handle_mouse(click(col, row));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), want);
    }

    #[test]
    fn picker_click_outside_closes_it() {
        let (mut app, _) = app_with_store();
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        app.handle_mouse(click(0, 1));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), "");
    }

    #[test]
    fn picker_too_small_terminal_still_closes() {
        let (mut app, _) = app_with_store();
        app.resize(30, 7);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        assert!(app.picker_grid().is_none());
        app.handle_key(key(KeyCode::Down)); // nothing to move, must not panic
        app.handle_key(key(KeyCode::Esc));
        assert!(app.picker().is_none());
    }

    #[test]
    fn picker_view_follows_the_selection() {
        let (mut app, _) = app_with_store();
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        let g = app.picker_grid().unwrap();
        for _ in 0..g.rows + 2 {
            app.handle_key(key(KeyCode::Down));
        }
        let p = app.picker().unwrap();
        assert!(p.sel / g.cols >= p.top && p.sel / g.cols < p.top + g.rows);
        assert!(p.top > 0);
    }

    // ---- picker placement (#74) ----

    /// Alex Example open, the picker opened from the message box, `[layout] emoji_picker` set.
    fn placed_picker(place: PickerPlace) -> App {
        let mut s = Settings::default();
        s.layout.emoji_picker = place;
        let mut app = online(App::with_settings(clock(), &s));
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ctrl('e'));
        assert!(app.picker().is_some());
        app
    }

    #[test]
    fn box_picker_sits_above_the_message_box_and_never_covers_it() {
        let mut app = placed_picker(PickerPlace::Box);
        let lay = app.layout_in(app.size);
        let g = app.picker_grid().unwrap();
        assert!(!g.inline);
        // Docked between the conversation and the message box, lined up with the box: the
        // conversation ends where it starts (so its newest messages stay in view) and keeps
        // at least 4 rows.
        assert_eq!(g.outer.y + g.outer.height, lay.composer.y);
        assert_eq!(g.outer.x, lay.composer.x);
        assert_eq!(lay.convo.y + lay.convo.height, g.outer.y);
        assert!(lay.convo.height >= 4);
        assert!(
            !g.outer.intersects(lay.composer)
                && !g.outer.intersects(lay.footer)
                && !g.outer.intersects(lay.convo)
        );
        // A click on a cell chooses it, as in the middle.
        let want = app.picker().unwrap().items(app.recents())[1]
            .as_str()
            .to_string();
        app.handle_mouse(click(g.grid.x + ui::PICKER_CELL as u16, g.grid.y));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), want);
    }

    #[test]
    fn inline_strip_is_one_row_in_the_message_box_under_the_draft() {
        let mut app = placed_picker(PickerPlace::Inline);
        let lay = app.layout_in(app.size);
        let g = app.picker_grid().unwrap();
        assert!(g.inline && g.rows == 1 && g.cols >= 3);
        // The strip is the box's last row inside its border; the draft row is still above it.
        assert_eq!(g.outer.y, lay.composer.y + lay.composer.height - 2);
        let text = ui::composer_text_area(&app, &lay);
        assert!(text.height >= 1 && text.y + text.height <= g.outer.y);
        // Right moves along the strip, typing filters it, Enter inserts and closes.
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.picker().unwrap().sel, 1);
        for c in "heart".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let first = app.picker().unwrap().items(app.recents())[0]
            .as_str()
            .to_string();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.picker().is_none());
        assert_eq!(app.active().draft(), first);
    }

    #[test]
    fn the_message_box_grows_one_row_for_the_strip_and_shrinks_back() {
        let mut s = Settings::default();
        s.layout.emoji_picker = PickerPlace::Inline;
        let mut app = online(App::with_settings(clock(), &s));
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        let before = app.layout_in(app.size).composer.height;
        app.handle_key(ctrl('e'));
        assert_eq!(app.layout_in(app.size).composer.height, before + 1);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.picker().is_none());
        assert_eq!(app.layout_in(app.size).composer.height, before);
    }

    #[test]
    fn box_and_inline_fall_back_to_the_middle_when_they_do_not_fit() {
        for place in [PickerPlace::Box, PickerPlace::Inline] {
            let mut app = placed_picker(place);
            // 32 x 8: no room above the message box, no row to spare in it.
            app.resize(32, 8);
            let lay = app.layout_in(app.size);
            let g = app.picker_grid().expect("the middle still fits");
            let middle = ui::picker_geometry(lay.body).unwrap();
            assert_eq!((g.outer, g.inline), (middle.outer, false), "{place:?}");
            // Too small even for the middle: no geometry, keys and Esc still work.
            app.resize(30, 7);
            assert!(app.picker_grid().is_none(), "{place:?}");
            app.handle_key(key(KeyCode::Right));
            app.handle_key(key(KeyCode::Esc));
            assert!(app.picker().is_none(), "{place:?}");
        }
    }

    #[test]
    fn the_reaction_picker_stays_in_the_middle_whatever_the_setting() {
        for place in [PickerPlace::Box, PickerPlace::Inline] {
            let mut app = placed_picker(place);
            app.handle_key(key(KeyCode::Esc));
            app.open_reaction_picker("alex", "m4");
            assert_eq!(app.picker_place(), Some(PickerPlace::Popup), "{place:?}");
            let lay = app.layout_in(app.size);
            let middle = ui::picker_geometry(lay.body).unwrap();
            assert_eq!(app.picker_grid().unwrap().outer, middle.outer, "{place:?}");
        }
    }

    #[test]
    fn a_new_place_applies_at_once_to_an_open_picker() {
        let mut app = placed_picker(PickerPlace::Popup);
        let middle = app.picker_grid().unwrap().outer;
        let mut s = Settings::default();
        s.layout.emoji_picker = PickerPlace::Box;
        app.apply_settings(s);
        assert!(app.picker().is_some(), "still open");
        let lay = app.layout_in(app.size);
        let g = app.picker_grid().unwrap();
        assert_ne!(g.outer, middle);
        assert_eq!(g.outer.y + g.outer.height, lay.composer.y);
    }

    #[test]
    fn a_new_place_keeps_the_selected_emoji_on_screen() {
        let mut app = placed_picker(PickerPlace::Popup);
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.picker().unwrap().top, 0, "row 5 of 8 is in view");
        // One row: the strip must scroll to the selected cell's row, not show row 0.
        let mut s = Settings::default();
        s.layout.emoji_picker = PickerPlace::Inline;
        app.apply_settings(s);
        let g = app.picker_grid().unwrap();
        let p = app.picker().unwrap();
        assert!(g.inline);
        assert_eq!(p.top, p.sel / g.cols, "{} / {}", p.sel, g.cols);
    }

    #[test]
    fn the_conversation_gives_the_box_its_rows_back_when_it_closes() {
        let mut s = Settings::default();
        s.layout.emoji_picker = PickerPlace::Box;
        let mut app = online(App::with_settings(clock(), &s));
        app.resize(80, 24);
        app.handle_key(key(KeyCode::Enter));
        let before = app.layout_in(app.size).convo;
        app.handle_key(ctrl('e'));
        let open = app.layout_in(app.size);
        assert!(open.convo.height < before.height && open.picker_dock.height > 0);
        app.handle_key(key(KeyCode::Esc));
        let closed = app.layout_in(app.size);
        assert_eq!((closed.convo, closed.picker_dock.height), (before, 0));
    }

    #[test]
    fn the_shortcode_list_never_shows_under_an_open_picker() {
        for place in [PickerPlace::Popup, PickerPlace::Box, PickerPlace::Inline] {
            let mut app = placed_picker(place);
            app.handle_key(key(KeyCode::Esc));
            for c in ":sm".chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            let lay = app.layout_in(app.size);
            assert!(ui::popup_geometry(&app, &lay).is_some(), "list open first");
            app.handle_key(ctrl('e'));
            let lay = app.layout_in(app.size);
            assert!(app.picker().is_some());
            assert!(ui::popup_geometry(&app, &lay).is_none(), "{place:?}");
        }
    }

    // ---- links ----

    fn chat_with_text(texts: &[&str]) -> App {
        // `open_link` has no default key in the chat list: these tests bind u there.
        let mut app = online(App::with_settings(
            clock(),
            &settings_with(&[("list", "u", "open_link")]),
        ));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        let messages = texts
            .iter()
            .enumerate()
            .map(|(i, t)| ChatMessage {
                network: Network::Teams,
                chat_id: "a".into(),
                id: format!("m{i}"),
                ts: 1000 + i as i64,
                sender: "Alex Example".into(),
                text: (*t).into(),
                state: MessageState::Sent,
                ..ChatMessage::blank(Network::Teams)
            })
            .collect();
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages,
        });
        app
    }

    #[test]
    fn u_offers_the_host_then_y_queues_the_link() {
        let mut app = chat_with_text(&[
            "old https://old.example/x",
            "see https://docs.example/a?b=1 please",
        ]);
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(
            app.prompt(),
            Some(&Prompt::Confirm("https://docs.example/a?b=1".into()))
        );
        assert_eq!(app.context(), Context::Confirm);
        assert!(app.take_open_urls().is_empty(), "nothing opens before y");
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.take_open_urls(),
            vec!["https://docs.example/a?b=1".to_string()]
        );
        assert!(app.prompt().is_none());
        assert!(app.take_open_urls().is_empty(), "taken once");
    }

    #[test]
    fn n_and_esc_cancel_the_link() {
        for cancel in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = chat_with_text(&["https://docs.example/a"]);
            app.handle_key(key(KeyCode::Char('u')));
            app.handle_key(key(cancel));
            assert!(app.prompt().is_none());
            assert!(app.take_open_urls().is_empty());
        }
    }

    #[test]
    fn other_keys_do_nothing_while_a_link_waits() {
        let mut app = chat_with_text(&["https://docs.example/a"]);
        app.handle_key(key(KeyCode::Char('u')));
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(!app.should_quit());
        assert_eq!(app.active, Network::Teams);
        assert!(app.prompt().is_some());
        app.handle_key(ctrl('c'));
        assert!(app.should_quit());
    }

    #[test]
    fn several_links_ask_for_a_number_first() {
        let mut app = chat_with_text(&[
            "a https://one.example/x and https://two.example/y and https://one.example/x",
        ]);
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(
            app.prompt(),
            Some(&Prompt::Pick(vec![
                "https://one.example/x".into(),
                "https://two.example/y".into()
            ]))
        );
        app.handle_key(key(KeyCode::Char('y'))); // y before choosing does nothing
        assert!(app.take_open_urls().is_empty());
        app.handle_key(key(KeyCode::Char('3'))); // out of range: ignored
        assert!(matches!(app.prompt(), Some(Prompt::Pick(_))));
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(
            app.prompt(),
            Some(&Prompt::Confirm("https://two.example/y".into()))
        );
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.take_open_urls(),
            vec!["https://two.example/y".to_string()]
        );
    }

    #[test]
    fn at_most_nine_links_are_offered() {
        let text: String = (1..=12)
            .map(|i| format!("https://h{i}.example/ "))
            .collect();
        let mut app = chat_with_text(&[&text]);
        app.handle_key(key(KeyCode::Char('u')));
        match app.prompt() {
            Some(Prompt::Pick(u)) => assert_eq!(u.len(), 9),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unsafe_schemes_are_never_offered() {
        let mut app = chat_with_text(&["javascript:alert(1) file:///C:/x ftp://a.example/b"]);
        app.handle_key(key(KeyCode::Char('u')));
        assert!(app.prompt().is_none());
        assert_eq!(notice_text(&app).as_deref(), Some("No link in this chat."));
        // Even a hand-built offer refuses them.
        app.offer_links(vec![
            "javascript:alert(1)".into(),
            "file:///C:/x".into(),
            "data:text/html,x".into(),
        ]);
        assert!(app.prompt().is_none());
    }

    #[test]
    fn deleted_messages_offer_no_links() {
        let mut app = chat_with_text(&["https://docs.example/a"]);
        let mut m = app.ns(Network::Teams).messages["a"][0].clone();
        m.state = MessageState::Deleted;
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages: vec![m],
        });
        app.handle_key(key(KeyCode::Char('u')));
        assert!(app.prompt().is_none());
    }

    #[test]
    fn composer_grows_with_the_draft_up_to_six_rows() {
        let mut app = typing_app();
        app.resize(80, 30);
        let h = |app: &App| app.layout_in(Rect::new(0, 0, 80, 30)).composer.height;
        assert_eq!(h(&app), 3);
        for i in 0..3 {
            app.handle_key(key_mod(KeyCode::Enter, KeyModifiers::ALT));
            type_text(&mut app, &format!("line {i}"));
        }
        assert_eq!(h(&app), 6, "4 text rows");
        for _ in 0..10 {
            app.handle_key(key_mod(KeyCode::Enter, KeyModifiers::ALT));
        }
        assert_eq!(h(&app), 8, "capped at 6 text rows");
        // A short terminal never lets the box swallow the conversation.
        app.resize(80, 12);
        let lay = app.layout_in(Rect::new(0, 0, 80, 12));
        assert!(lay.convo.height >= 4, "{}", lay.convo.height);
    }

    // ---- commands: the : bar, :word in the message box, and F1/F2 ----

    fn sent_text(out: &Out) -> Option<String> {
        out.iter().find_map(|(_, c)| match c {
            Command::Send { text, .. } => Some(text.clone()),
            _ => None,
        })
    }

    #[test]
    fn colon_in_the_list_opens_the_bar_with_every_command() {
        let mut app = online_app();
        assert!(app.handle_key(key(KeyCode::Char(':'))).is_empty());
        assert_eq!(app.context(), Context::Popup);
        // Nothing typed: every command, no chats.
        assert_eq!(app.palette_items().len(), commands::COMMANDS.len());
        assert!(app.overlay_open());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.palette().is_none());
        assert_eq!(app.context(), Context::List);
    }

    #[test]
    fn typing_in_the_bar_filters_commands_and_chats() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char(':')));
        type_text(&mut app, "se");
        let items = app.palette_items();
        assert_eq!(items[0], crate::palette::PalItem::Cmd(CmdId::Settings));
        assert_eq!(items[1], crate::palette::PalItem::Cmd(CmdId::Search));
        // Backspace widens the list again.
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.palette_items().len(), commands::COMMANDS.len());
        // A leading / is ignored when matching.
        type_text(&mut app, "/sam");
        // No command mentions "sam": the one chat is the only item.
        assert_eq!(
            app.palette_items(),
            vec![crate::palette::PalItem::Chat("b".into())]
        );
        // Enter goes into that chat like Enter in the list: it holds no message yet, so its
        // message box.
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(app.palette().is_none());
        assert_eq!(app.active().open.as_deref(), Some("b"));
        assert_eq!(app.active().focus, Focus::Composer);
        assert!(
            out.iter()
                .any(|(_, c)| matches!(c, Command::OpenChat { chat_id } if chat_id == "b")),
            "{out:?}"
        );
    }

    #[test]
    fn a_chat_chosen_in_the_bar_opens_like_enter_in_the_list() {
        // Chat "a" holds messages: the bar goes into it with the newest one selected.
        let mut app = chat_with_text(&["one", "two"]);
        colon(&mut app, "alex");
        assert!(app.palette().is_none());
        assert_eq!(app.active().open.as_deref(), Some("a"));
        assert_eq!(app.active().focus, Focus::Select);
        assert_eq!(app.active().sel_msg.as_deref(), Some("m1"));
    }

    #[test]
    fn the_bar_keys_choose_wrap_and_run() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char(':')));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.palette().unwrap().sel,
            commands::COMMANDS.len() - 1,
            "wraps up"
        );
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.palette().unwrap().sel, 0, "wraps down");
        // Enter on /help opens the help.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.palette().is_none());
        assert_eq!(app.help_open(), Some(0));
        // No match: Enter keeps the bar open, Esc closes it.
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char(':')));
        type_text(&mut app, "zzzz");
        assert!(app.palette_items().is_empty());
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.palette().is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.palette().is_none());
    }

    #[test]
    fn the_bar_takes_a_paste_and_ctrl_c_quits_from_it() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char(':')));
        app.handle_paste("sa\nm");
        assert_eq!(app.palette().unwrap().query, "sa m");
        app.handle_key(ctrl('c'));
        assert!(app.should_quit());
    }

    #[test]
    fn an_unavailable_command_gives_its_reason_and_runs_nothing() {
        let mut app = online_app();
        let out = colon(&mut app, "signin");
        assert!(out.is_empty());
        assert!(app.palette().is_none());
        assert_eq!(
            notice_text(&app).as_deref(),
            Some("Already signed in to Teams")
        );
        // Not signed in: signout has nothing to sign out of, signin sends Login.
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        assert!(colon(&mut app, "signout").is_empty());
        assert_eq!(notice_text(&app).as_deref(), Some("Not signed in to Teams"));
        assert!(colon(&mut app, "refresh").is_empty());
        assert_eq!(
            notice_text(&app).as_deref(),
            Some("REFRESH - sign in first")
        );
        assert_eq!(
            colon(&mut app, "signin"),
            vec![(Network::Teams, Command::Login)]
        );
        // The panel and message selection open the bar too.
        let mut app = App::new(clock());
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        app.handle_key(key(KeyCode::Char(':')));
        assert!(app.palette().is_some(), "panel");
        assert!(app
            .palette_items()
            .iter()
            .all(|i| matches!(i, crate::palette::PalItem::Cmd(_))));
        let mut app = online_app();
        app.ns_mut(Network::Teams).focus = Focus::Select;
        app.handle_key(key(KeyCode::Char(':')));
        assert!(app.palette().is_some(), "select");
    }

    #[test]
    fn signout_asks_first_and_y_sends_logout() {
        let mut app = online_app();
        assert!(colon(&mut app, "signout").is_empty());
        assert_eq!(app.prompt(), Some(&Prompt::SignOut(Network::Teams)));
        assert_eq!(app.context(), Context::Confirm);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            vec![(Network::Teams, Command::Logout)]
        );
        assert!(app.prompt().is_none());
        colon(&mut app, "logout");
        assert!(app.handle_key(key(KeyCode::Char('n'))).is_empty());
        assert!(app.prompt().is_none());
    }

    #[test]
    fn colon_tour_in_the_demo_starts_the_tour_and_takes_the_top_row() {
        let mut app = online_app();
        app.demo = true;
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.settings_page().is_some());
        let body_before = app.layout_in(app.size).body;
        app.handle_key(key(KeyCode::F(2)));
        colon(&mut app, "tour");
        assert!(app.tour_running() && app.tour_step() == Some(1));
        assert!(app.settings_page().is_none(), "start_tour closes the page");
        assert_eq!(app.layout_in(app.size).body.y, body_before.y + 1);
        assert_eq!(app.layout_in(app.size).header.y, 1);
        assert!(app.tour_bar().unwrap().text.starts_with("TOUR 1/26 - "));
    }

    #[test]
    fn while_the_tour_plays_only_its_own_keys_count() {
        let mut app = online_app();
        app.start_tour();
        // A paste, a click and an ordinary key change nothing.
        assert!(app.handle_paste("hello").is_empty());
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit());
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.tour_running() && app.tour_step() == Some(1));
        // Space pauses and resumes; Esc ends; the quit key (Ctrl+C) quits.
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.tour_paused() && app.next_deadline().is_none());
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.tour_paused());
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.tour_running());
        app.start_tour();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn n_runs_the_rest_of_a_step_at_once_and_starts_the_next() {
        let mut app = online_app();
        app.start_tour();
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.tour_step(), Some(2));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.tour_step(), Some(3));
    }

    #[test]
    fn tour_outside_the_demo_says_how_to_start_it() {
        let mut app = online_app();
        colon(&mut app, "tour");
        assert_eq!(
            notice_text(&app).as_deref(),
            Some("TOUR - run chatcli --demo --tour")
        );
    }

    #[test]
    fn a_command_that_takes_no_arguments_says_so() {
        let mut app = online_app();
        colon(&mut app, "quit now");
        assert!(!app.should_quit());
        assert_eq!(
            notice_text(&app).as_deref(),
            Some(":quit takes no arguments")
        );
        colon(&mut app, "keys emacs");
        assert_eq!(
            notice_text(&app).as_deref(),
            Some(":keys takes no arguments")
        );
    }

    #[test]
    fn search_runs_from_the_bar() {
        let (mut app, _) = app_with_store();
        colon(&mut app, "search");
        assert!(app.search().is_some());
    }

    #[test]
    fn colon_in_the_message_box_lists_commands_then_emoji_and_enter_runs_one() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::Char('i')));
        type_text(&mut app, ":se");
        let p = app.popup().expect("commands popup");
        assert_eq!(p.title, COMMANDS_TITLE);
        let labels: Vec<&str> = p.items.iter().map(|i| i.label.trim_end()).collect();
        assert_eq!(&labels[..3], [":settings", ":search", ":searchchat"]);
        assert!(
            p.items[3..].iter().all(|i| i.command.is_none()) && p.items.len() > 3,
            "emoji follow the commands: {labels:?}"
        );
        assert_eq!(p.items[0].label.trim_end(), ":settings");
        assert_eq!(p.items[0].detail, "theme, layout, keys and more");
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(out.is_empty());
        assert!(app.settings_page().is_some());
        assert_eq!(app.active().draft(), "", "the command is not a draft");
        assert!(app.popup().is_none());
    }

    #[test]
    fn no_commands_when_text_follows_the_cursor() {
        // `:) see you`, the cursor just after the colon: choosing a command would drop the rest.
        let mut app = typing_app();
        type_text(&mut app, ":) see you");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Right));
        assert!(app
            .popup()
            .is_none_or(|p| p.items.iter().all(|i| i.command.is_none())));
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(sent_text(&out).as_deref(), Some(":) see you"));
        // `:he` typed in front of a message.
        let mut app = typing_app();
        type_text(&mut app, "hello");
        app.handle_key(key(KeyCode::Home));
        type_text(&mut app, ":he");
        assert!(app
            .popup()
            .is_none_or(|p| p.items.iter().all(|i| i.command.is_none())));
        assert!(app.help_open().is_none());
    }

    #[test]
    fn the_commands_list_closes_on_a_space_or_a_second_colon() {
        let commands = |app: &App| {
            app.popup()
                .is_some_and(|p| p.items.iter().any(|i| i.command.is_some()))
        };
        for (text, why) in [
            (":keys ", "a space ends the command word"),
            ("::", "a second colon"),
            ("hi :se", "only at the start of the draft"),
            (":zzz", "no command matches"),
            (":P", "an emoticon"),
        ] {
            let mut app = typing_app();
            type_text(&mut app, text);
            assert!(!commands(&app), "{why}");
        }
        let mut app = typing_app();
        type_text(&mut app, ":");
        assert!(commands(&app), "a lone colon lists them all");
    }

    #[test]
    fn a_message_that_starts_with_a_command_word_is_text() {
        for text in [
            ":help me with the report please",
            ":about the meeting",
            ":q what?",
            ":theme light",
            "/help",
            "//usr/bin",
            ":zzq",
            ":P",
            ":D",
        ] {
            let mut app = typing_app();
            type_text(&mut app, text);
            let out = app.handle_key(key(KeyCode::Enter));
            assert_eq!(sent_text(&out).as_deref(), Some(text), "{text}");
            assert!(app.settings_page().is_none() && app.help_open().is_none());
        }
    }

    #[test]
    fn a_space_or_esc_closes_the_list_and_enter_then_sends_the_word_as_text() {
        let mut app = typing_app();
        type_text(&mut app, ":help ");
        assert!(app.popup().is_none());
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(sent_text(&out).as_deref(), Some(":help"));
        let mut app = typing_app();
        type_text(&mut app, ":help");
        assert!(app.popup().is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup().is_none());
        assert_eq!(
            app.active().focus,
            Focus::Composer,
            "Esc closed the list only"
        );
        let out = app.handle_key(key(KeyCode::Enter));
        assert_eq!(sent_text(&out).as_deref(), Some(":help"));
        assert!(app.help_open().is_none());
    }

    #[test]
    fn an_edit_is_never_read_as_a_command() {
        let mut app = typing_app();
        app.ns_mut(Network::Teams).targets.insert(
            "a".into(),
            Target::Edit {
                message_id: "m1".into(),
                original: "old".into(),
                saved: Composer::default(),
            },
        );
        type_text(&mut app, ":help");
        assert!(
            app.popup()
                .is_none_or(|p| p.items.iter().all(|i| i.command.is_none())),
            "no commands while editing"
        );
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&out[..], [(_, Command::Edit { text, .. })] if text == ":help"),
            "{out:?}"
        );
        assert!(app.settings_page().is_none());
    }

    #[test]
    fn f1_opens_help_and_f2_the_settings_screen() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.help_open(), Some(0), "F1 list");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.help_open(), None, "Esc closes help");
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.settings_page().is_some(), "F2 list");
        assert!(app.overlay_open());
        // F1 over the page opens help; Esc returns to the page; F2 closes it.
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.help_open(), Some(0));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.help_open().is_none() && app.settings_page().is_some());
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.settings_page().is_none());
        // The message box too.
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.help_open(), Some(0), "F1 box");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.settings_page().is_some(), "F2 box");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.settings_page().is_none(), "Esc closes the page");
    }

    #[test]
    fn the_settings_page_is_modal_and_ctrl_c_still_quits() {
        let mut app = online_app();
        app.handle_key(key(KeyCode::F(2)));
        let before = app.active().selected.clone();
        for c in [
            KeyCode::Tab,
            KeyCode::Char('j'),
            KeyCode::Char('/'),
            KeyCode::Enter,
        ] {
            assert!(app.handle_key(key(c)).is_empty(), "{c:?}");
        }
        assert_eq!(app.active, Network::Teams);
        assert_eq!(app.active().selected, before);
        assert!(app.palette().is_none());
        assert!(app.handle_mouse(click(5, 4)).is_empty());
        app.handle_paste("pasted");
        assert!(app.settings_page().is_some());
        app.handle_key(ctrl('c'));
        assert!(app.should_quit());
    }

    #[test]
    fn the_help_starts_with_the_commands() {
        let app = online_app();
        let rows = ui::help_rows(&app);
        assert!(matches!(&rows[0], ui::HelpRow::Title(t) if t == "Commands"));
        let entries: Vec<(&String, &String)> = rows[1..]
            .iter()
            .take_while(|r| !matches!(r, ui::HelpRow::Blank))
            .filter_map(|r| match r {
                ui::HelpRow::Entry { keys, name, .. } => Some((keys, name)),
                _ => None,
            })
            .collect();
        assert_eq!(entries.len(), commands::COMMANDS.len());
        assert_eq!(entries[0], (&":help".to_string(), &"?".to_string()));
        assert_eq!(entries[1], (&":settings".to_string(), &"F2".to_string()));
    }

    #[test]
    fn clicking_the_chat_already_shown_opens_it_like_enter() {
        let mut app = online_app();
        // First click previews and keeps the list focus.
        assert!(app.handle_mouse(click(5, 4)).is_empty());
        assert_eq!(app.active().focus, Focus::List);
        // The second click on the same row is Enter: the message box takes focus.
        let out = app.handle_mouse(click(5, 4));
        assert_eq!(app.active().focus, Focus::Composer);
        assert_eq!(app.active().open.as_deref(), Some("b"));
        assert!(out.contains(&open_chat_cmd("b")), "{out:?}");
    }
}
