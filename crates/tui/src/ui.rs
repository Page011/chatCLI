//! Drawing. `draw` reads `App` and never changes it. Colours are the ANSI 16 named ones only,
//! so the terminal's own light or dark theme decides how they look; every colour also has a
//! text or shape signal (status words, reverse video, double border on the focused pane).

use chatcli_core::model::{ChatMessage, MessageState, Network, Status};
use chatcli_core::settings::{Avatars, SidebarWidth};
use qrcode::{Color as QrColor, EcLevel, QrCode};
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::msg::Target;
use crate::app::{App, Focus, NetState, Prompt, NARROW};
use crate::chats;
use crate::composer::{self, Composer};
use crate::emoji::{PickerFor, CATEGORIES};
use crate::keymap::{Action, Context, Keymap, Mode};
use crate::links::find_links;
use crate::media_view::{
    human_size, kind_label, percent, strip_markers, Fetch, MediaKey, MediaState, View,
};
use crate::older::Older;
use crate::settings_view::RowId;
use crate::text::{clean_line, fit, sw, tail, truncate, wrap, wrap_pieces, Clock, Fmt, Piece};
use crate::theme::{Outline, Panel, Theme};
use std::path::PathBuf;

const MIN_W: u16 = 30;
const MIN_H: u16 = 8;
const BRAND: &str = " chatcli ";
const SENDER_MAX: usize = 14;

/// Avatar size in cells: 4 by 2 is about square in a terminal font.
const AVATAR_W: usize = 4;
/// Columns between the avatar and the name and text.
const AVATAR_GAP: usize = 2;
/// Messages from the same person this close together share one name row and avatar.
const GROUP_MS: i64 = 5 * 60 * 1000;
/// Narrower than this, the avatar layout leaves too little room for text.
const AVATAR_MIN_WIDTH: usize = 24;
const QR_HELP: &str = "Linked devices (Android: menu, iPhone: Settings) > Link a device";
/// Modules of white border around the QR code (the QR standard asks for 4).
const QUIET: usize = 4;
/// Longest a status or notice may get in the footer: two rows, then it is cut.
const FOOTER_WRAP_LINES: usize = 2;
/// Columns before a chat's name in the list: the unread count (3), the mention mark and a gap.
const ROW_PREFIX: usize = 5;

pub(crate) fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// What the drawing code needs besides the app: styles, the clock and the time format.
pub(crate) struct Look<'a> {
    pub theme: &'a Theme,
    pub clock: &'a Clock,
    pub fmt: Fmt,
    /// Where messages get an avatar and a name row (`[layout] avatars`).
    pub avatars: Avatars,
}

/// Smallest terminal height whose body is `body` rows tall: header, the key-hint row and one
/// row of status or notice around it.
fn height_for_body(body: usize) -> usize {
    body + 3
}

/// Footer height from what it shows. Tall terminals (20+ rows) give a status or notice up to
/// two wrapped rows, so the action half of "what failed: what to do" is not cut off; mid-size
/// ones share one row; short ones show the key hints or the problem, never both.
pub(crate) fn footer_height(app: &App, area: Rect) -> u16 {
    let base = match area.height {
        20.. => 1 + problem_lines(app, area.width as usize).len() as u16,
        14..=19 => 1 + u16::from(status_part(app).is_some() || notice_part(app).is_some()),
        _ => 1,
    };
    // The `:` command box is one more row, directly above the rest of the footer.
    base + u16::from(app.command_text().is_some())
}

pub(crate) struct Layout {
    pub header: Rect,
    pub footer: Rect,
    pub body: Rect,
    pub filter_row: Option<Rect>,
    pub list: Rect,
    pub sidebar: Rect,
    pub convo: Rect,
    pub messages: Rect,
    pub composer: Rect,
}

/// Columns the draft can use in a message box `width` wide: the border and the `> ` prompt take 4.
fn composer_text_width(width: u16) -> usize {
    (width as usize).saturating_sub(4)
}

/// Width of the chat list. The default is 30% clamped to 28..=40: 28 keeps about 12 columns of
/// title once the unread count and the date are drawn. `[layout] sidebar_width` replaces that,
/// clamped so the message pane keeps `MIN_PANE` columns (this is only called from `NARROW` up,
/// where there is room for both).
pub(crate) fn sidebar_cols(width: u16, setting: Option<SidebarWidth>) -> u16 {
    const MIN_LIST: u16 = 16;
    const MIN_PANE: u16 = 30;
    let wanted = match setting {
        None => return (width * 30 / 100).clamp(28, 40),
        Some(SidebarWidth::Columns(c)) => c,
        Some(SidebarWidth::Percent(p)) => (u32::from(width) * u32::from(p) / 100) as u16,
    };
    wanted.clamp(MIN_LIST, width.saturating_sub(MIN_PANE).max(MIN_LIST))
}

/// Screen regions for a terminal of `area`. Under `NARROW` columns only one pane shows:
/// the conversation when `show_convo`, else the chat list. A hidden pane is an empty rect,
/// so mouse hit tests against it never match.
pub(crate) fn layout(
    area: Rect,
    footer_h: u16,
    show_convo: bool,
    filter_row: bool,
    draft: &str,
    extra_rows: u16,
    sidebar_width: Option<SidebarWidth>,
) -> Layout {
    let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let footer_h = footer_h.min(area.height.saturating_sub(1));
    let footer = Rect::new(
        area.x,
        area.y + area.height - footer_h,
        area.width,
        footer_h,
    );
    let body = Rect::new(
        area.x,
        area.y + header.height,
        area.width,
        area.height.saturating_sub(header.height + footer_h),
    );
    let (sidebar, main) = if area.width < NARROW {
        if show_convo {
            (Rect::default(), body)
        } else {
            (body, Rect::default())
        }
    } else {
        let w = sidebar_cols(area.width, sidebar_width);
        (
            Rect::new(body.x, body.y, w, body.height),
            Rect::new(body.x + w, body.y, area.width - w, body.height),
        )
    };
    // The message box is 3 rows (one line of text) and grows with the draft up to 6 text rows,
    // always leaving the conversation 4 rows. `extra_rows` is the reply or edit line above the
    // text.
    let wanted =
        composer::rows_needed(draft, composer_text_width(main.width)) as u16 + 2 + extra_rows;
    let composer_h = wanted
        .min(main.height.saturating_sub(4))
        .max(main.height.min(3));
    let convo = Rect::new(main.x, main.y, main.width, main.height - composer_h);
    let composer = Rect::new(main.x, main.y + convo.height, main.width, composer_h);
    let inner = sidebar.inner(Margin::new(1, 1));
    let (filter_row, list) = if filter_row && inner.height > 0 {
        (
            Some(Rect::new(inner.x, inner.y, inner.width, 1)),
            Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
        )
    } else {
        (None, inner)
    };
    Layout {
        header,
        footer,
        body,
        filter_row,
        list,
        sidebar,
        convo,
        messages: convo.inner(Margin::new(1, 1)),
        composer,
    }
}

// ---- header ----

pub(crate) struct Tab {
    pub net: Network,
    pub x: u16,
    pub w: u16,
    pub spans: Vec<Span<'static>>,
}

fn chip(status: &Status, short: bool, t: &Theme) -> (&'static str, Style) {
    match status {
        Status::Starting | Status::Connecting => {
            (if short { "CONN…" } else { "CONNECTING" }, t.hint)
        }
        // The short forms read as states, not commands, and never look alike.
        Status::NeedsLogin { .. } => (
            if short { "NO LOGIN" } else { "SIGN-IN NEEDED" },
            t.chip_attention,
        ),
        Status::LoggingIn => (if short { "SIGNING" } else { "SIGNING IN" }, t.accent),
        Status::Qr { .. } => (if short { "QR" } else { "SCAN QR" }, t.accent),
        Status::Online { live: true, .. } => ("LIVE", t.chip_live),
        Status::Online { live: false, .. } => ("NOT LIVE", t.chip_attention),
        Status::Error { .. } => ("ERROR", t.error),
    }
}

/// Tab cells with their screen position. Shared by drawing and mouse hit testing.
pub(crate) fn tabs(app: &App, width: u16) -> Vec<Tab> {
    let t = app.theme();
    let short = width < NARROW;
    let mut x = if short { 0 } else { BRAND.len() as u16 };
    let mut out = Vec::new();
    for net in Network::ALL {
        let ns = app.ns(net);
        let active = net == app.active().network;
        let unread = ns.unread();
        let label = if unread > 0 {
            format!("{} {}", net.label(), unread)
        } else {
            net.label().to_string()
        };
        let (text, chip_style) = chip(&ns.status, short, t);
        let base = if active { t.selected } else { Style::new() };
        // The network's own colour, except on the reversed active tab, where it would tint the
        // block instead of the name.
        let name_style = match (active, net) {
            (true, _) => Style::new(),
            (false, Network::Teams) => t.network_teams,
            (false, Network::WhatsApp) => t.network_whatsapp,
        };
        let spans = vec![
            Span::styled(" ", base),
            Span::styled(label, base.patch(bold()).patch(name_style)),
            Span::styled(" ", base),
            Span::styled(
                text,
                if active {
                    base
                } else {
                    chip_style.patch(bold())
                },
            ),
            Span::styled(" ", base),
        ];
        let w = spans.iter().map(|s| s.width() as u16).sum();
        out.push(Tab { net, x, w, spans });
        x += w + 1;
    }
    out
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    if area.height == 0 {
        return;
    }
    bar_background(f, t, Panel::Header, area);
    if area.width >= NARROW {
        f.render_widget(Paragraph::new(Span::styled(BRAND, bold())), area);
    }
    let mut end = 0;
    for t in tabs(app, area.width) {
        let r = Rect::new(area.x + t.x, area.y, t.w, 1).intersection(area);
        f.render_widget(Paragraph::new(Line::from(t.spans)), r);
        end = t.x + t.w;
    }
    let quit = "Ctrl+C quit";
    if area.width >= end + 2 + quit.len() as u16 {
        f.render_widget(
            Paragraph::new(Span::styled(quit, t.hint)).alignment(Alignment::Right),
            area,
        );
    }
}

// ---- footer ----

/// Why the active network is not healthy: the ERROR message, or why it is NOT LIVE.
fn status_part(app: &App) -> Option<Span<'static>> {
    let t = app.theme();
    match &app.active().status {
        Status::Error { message } => Some(Span::styled(
            format!("ERROR: {message}"),
            t.error.patch(bold()),
        )),
        Status::Online {
            live: false,
            detail: Some(d),
        } => Some(Span::styled(format!("NOT LIVE: {d}"), t.warn)),
        _ => progress_part(app),
    }
}

/// An upload in flight (a file being sent): how far it is, or just that it is going.
fn progress_part(app: &App) -> Option<Span<'static>> {
    let ns = app.active();
    // Several at once are rare; the oldest client id is a stable pick.
    let (_, &(done, total)) = ns.progress.iter().min_by_key(|(id, _)| id.as_str())?;
    let text = match percent(done, total) {
        Some(p) => format!(
            "SENDING {p}% ({} of {})",
            human_size(done.min(total)),
            human_size(total)
        ),
        None => "SENDING...".to_string(),
    };
    Some(Span::styled(text, app.theme().ok))
}

/// The newest fresh notice, red when it is an error. A notice about the tab on screen needs no
/// network name; one about the other network names it after its key word.
fn notice_part(app: &App) -> Option<Span<'static>> {
    let t = app.theme();
    app.current_notice().map(|n| {
        let style = if n.is_error { t.error } else { t.text };
        let text = if n.global || n.network == app.active().network {
            n.text.clone()
        } else {
            tag_network(&n.text, n.network.label())
        };
        Span::styled(text, style)
    })
}

/// `BLOCKED: executable` becomes `BLOCKED (Teams): executable`: the uppercase key word stays
/// first. A notice without one starts with the network instead.
fn tag_network(text: &str, label: &str) -> String {
    match text.split_once(':') {
        Some((head, rest))
            if head.chars().any(|c| c.is_ascii_uppercase())
                && !head.chars().any(|c| c.is_lowercase()) =>
        {
            format!("{head} ({label}):{rest}")
        }
        _ => format!("{label}: {text}"),
    }
}

/// Footer hints: (actions, label). The key shown is the first key each action has in the
/// active keymap, so rebinding changes the footer. Several actions read `Up/Down`; an empty
/// action list is literal text; an action with no key is left out.
type Hint<'a> = (&'a [Action], &'a str);

/// The footer as whole items (text, applies), most important first: when the line is too short
/// the least important ones are dropped, never cut in half.
fn join_hints(keys: &Keymap, ctx: Context, items: &[Hint]) -> Vec<(String, bool)> {
    items
        .iter()
        .filter_map(|(actions, label)| {
            if actions.is_empty() {
                Some(((*label).to_string(), true))
            } else {
                keys.hint(ctx, actions)
                    .map(|k| (format!("{k} {label}"), true))
            }
        })
        .collect()
}

fn hints(app: &App) -> Vec<(String, bool)> {
    use Action::*;
    let ns = app.active();
    let keys = app.keys();
    let ctx = app.context();
    let vim = app.mode().is_some();
    if let Some(tour) = app.tour_bar() {
        return vec![(tour.hint.to_string(), true)];
    }
    if app.help_open().is_some() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[CloseHelp], "close"),
                (&[ListUp, ListDown], "scroll"),
                (&[ScrollUp, ScrollDown], "page"),
            ],
        );
    }
    if app.command_text().is_some() {
        return join_hints(
            keys,
            ctx,
            &[(&[FilterAccept], "run"), (&[FilterCancel], "cancel")],
        );
    }
    if let Some(prompt) = app.prompt() {
        return vec![(link_line(prompt, keys, ctx), true)];
    }
    if app.palette().is_some() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[PopupUp, PopupDown], "choose"),
                (&[PopupClose], "close"),
                (&[PopupAccept], "run"),
                (&[], "Type to filter"),
            ],
        );
    }
    if let Some(page) = app.settings_page() {
        if let Some(line) = crate::settings_view::capture_line(page.edit.as_ref()) {
            return vec![(line, true)];
        }
        let row = app.current_row().map(|r| r.id);
        let items: &[&str] = if page.edit.is_some() {
            &["Enter save", "Esc cancel", "Type a value"]
        } else if page.in_rows && matches!(row, Some(RowId::KeyAction(..))) {
            &[
                "Up/Down move",
                "Esc close",
                "Enter set key",
                "r reset",
                "Tab sections",
            ]
        } else if page.in_rows && row == Some(RowId::ResetAllKeys) {
            &[
                "Up/Down move",
                "Esc close",
                "Enter reset all",
                "Tab sections",
            ]
        } else if page.in_rows {
            &[
                "Up/Down move",
                "Esc close",
                "Left/Right change",
                "Enter edit",
                "r reset",
                "Tab sections",
            ]
        } else {
            &["Up/Down section", "Esc close", "Enter open", "F1 help"]
        };
        return items.iter().map(|i| (i.to_string(), true)).collect();
    }
    if app.search().is_some() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[], "Type to search"),
                (&[PopupUp, PopupDown], "choose"),
                (&[PopupAccept], "open chat"),
                (&[PopupClose], "close"),
            ],
        );
    }
    if app.attach_prompt().is_some() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[], "Type a path"),
                (&[], "Tab complete"),
                (&[PopupUp, PopupDown], "choose"),
                (&[PopupAccept], "attach"),
                (&[PopupClose], "cancel"),
            ],
        );
    }
    if app.picker().is_some() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[], "Type to search"),
                (&[PopupAccept], "choose"),
                (&[ScrollUp, ScrollDown], "category"),
                (&[PopupClose], "close"),
            ],
        );
    }
    if let Some(p) = app.popup() {
        // The commands list runs the chosen command; the emoji and mention lists insert.
        let accept = if p.title == crate::commands::COMMANDS_TITLE {
            "run"
        } else {
            "insert"
        };
        return join_hints(
            keys,
            Context::Popup,
            &[
                (&[PopupAccept], accept),
                (&[PopupUp, PopupDown], "choose"),
                (&[PopupClose], "close"),
            ],
        );
    }
    // The selected chat is already on screen as a preview: Enter moves to its message box.
    // On a narrow terminal the list hides the pane, so nothing is previewed yet: Enter opens.
    // "type", not "reply": reply is the quote-reply key in selection mode.
    let verb = if ns.open.is_some() && ns.open == ns.selected && app.convo_visible(ns.network) {
        "type"
    } else {
        "open"
    };
    let chat: &[Action] = if vim {
        &[ListDown, ListUp]
    } else {
        &[ListUp, ListDown]
    };
    if !ns.panel() && ns.focus == Focus::List && !ns.filter.is_empty() {
        return join_hints(
            keys,
            ctx,
            &[
                (&[ClearFilter], "clear filter"),
                (&[NextNetwork], "network"),
                (chat, "chat"),
                (&[OpenChat], verb),
                (&[FilterStart], "filter"),
                (&[Quit], "quit"),
            ],
        );
    }
    if ns.panel() {
        return if matches!(ns.status, Status::NeedsLogin { .. }) {
            join_hints(
                keys,
                ctx,
                &[
                    (&[SignIn], "sign in"),
                    (&[CommandBar], "commands"),
                    (&[Help], "help"),
                    (&[NextNetwork], "network"),
                    (&[Quit], "quit"),
                ],
            )
        } else {
            join_hints(
                keys,
                ctx,
                &[
                    (&[CommandBar], "commands"),
                    (&[Help], "help"),
                    (&[NextNetwork], "network"),
                    (&[Quit], "quit"),
                ],
            )
        };
    }
    match ns.focus {
        Focus::List if vim => join_hints(
            keys,
            ctx,
            &[
                (chat, "chat"),
                (&[EnterInsert], "insert"),
                (&[CommandBar], "commands"),
                (&[CommandBox], "command"),
                (&[Help], "help"),
                (&[Quit], "quit"),
            ],
        ),
        Focus::List => join_hints(
            keys,
            ctx,
            &[
                (chat, "chat"),
                (&[OpenChat], verb),
                (&[CommandBar], "commands"),
                (&[Help], "help"),
                (&[NextNetwork], "network"),
                (&[Refresh], "refresh"),
                (&[Quit], "quit"),
            ],
        ),
        Focus::Filter => join_hints(
            keys,
            ctx,
            &[
                (&[], "Type to filter"),
                (chat, "chat"),
                (&[FilterAccept], "keep filter"),
                (&[FilterCancel], "clear"),
            ],
        ),
        Focus::Select => select_items(app),
        Focus::Composer => {
            let mut items = join_hints(
                keys,
                ctx,
                &[
                    (&[Send], "send"),
                    (&[], "/ commands"),
                    (&[Help], "help"),
                    (
                        if vim {
                            &[LeaveInsert]
                        } else {
                            &[LeaveComposer]
                        },
                        match (ns.target(), ns.attachment(), vim) {
                            (Some(Target::Reply(_)), _, _) => "cancel reply",
                            (Some(Target::Edit { .. }), _, _) => "cancel edit",
                            (None, Some(_), _) => "remove file",
                            (None, None, true) => "normal mode",
                            (None, None, false) => "back to list",
                        },
                    ),
                    (&[SelectMessages], "select"),
                    (&[InsertNewline], "newline"),
                    (&[EmojiPicker], "emoji"),
                    (&[Attach], "attach"),
                    (&[ScrollUp, ScrollDown], "scroll"),
                    (&[NextNetwork], "network"),
                ],
            );
            // The pointer is for an empty box: once you type, the keys matter more.
            if !ns.draft().is_empty() {
                items.retain(|(t, _)| t != "/ commands");
            }
            items
        }
    }
}

/// The footer while a message is selected: (text, applies). What does not apply to the selected
/// message (edit on someone else's, an action the backend cannot do) is drawn dimmed.
fn select_items(app: &App) -> Vec<(String, bool)> {
    use Action::*;
    let ns = app.active();
    let keys = app.keys();
    let m = app.selected_message(ns.network);
    let own = m.is_some_and(|m| m.from_me);
    let has_link = m.is_some_and(|m| !crate::links::urls(&m.text).is_empty());
    // Open and view only exist for a message that carries a file; they are dimmed when the
    // backend cannot fetch it (a Teams file with only a browser address can still be opened).
    let atts = m.map_or(&[][..], |m| m.attachments.as_slice());
    let can_open =
        ns.caps.fetch_media || ns.caps.fetch_files || atts.iter().any(|a| a.open_url.is_some());
    let can_view = ns.caps.fetch_media
        && app.inline_images()
        && atts
            .iter()
            .any(|a| a.kind == chatcli_core::model::MediaKind::Image);
    let head: [(&[Action], &str, bool); 4] = [
        (&[SelectUp, SelectDown], "move", true),
        (&[Reply], "reply", ns.caps.reply),
        (&[React], "react", ns.caps.react),
        (&[OpenLink], "link", has_link),
    ];
    let mut items = head.to_vec();
    if !atts.is_empty() {
        items.push((&[OpenMedia], "open", can_open));
        items.push((&[ViewImage], "view", can_view));
    }
    items.extend([
        (&[Edit][..], "edit", ns.caps.edit && own),
        (&[Delete][..], "delete", ns.caps.delete && own),
        (&[LeaveSelect][..], "back", true),
        (&[CommandBar][..], "commands", true),
        (&[Help][..], "help", true),
    ]);
    items
        .iter()
        .filter_map(|(actions, label, applies)| {
            keys.hint(Context::Select, actions)
                .map(|k| (format!("{k} {label}"), *applies))
        })
        .collect()
}

/// The footer hints as spans, as many whole items as fit in `room`. Normal intensity, except
/// while selecting a message, where the actions that apply are bold and the rest stay dim (in
/// every theme: the high-contrast theme has no dim, so the weight is the signal). A y/n question
/// is bold and never dim: the keyboard is modal while it shows.
fn hint_spans_of(app: &App, room: usize) -> Vec<Span<'static>> {
    let t = app.theme();
    if let Some(prompt) = app.prompt() {
        let line = link_line(prompt, app.keys(), app.context());
        return clip_spans(prompt_spans(&line, t), room).spans;
    }
    // A key being captured, or a swap asked: the question is bold, like a y/n.
    if let Some(line) = app
        .settings_page()
        .and_then(|p| crate::settings_view::capture_line(p.edit.as_ref()))
    {
        return clip_spans(prompt_spans(&line, t), room).spans;
    }
    let selecting = app.active().focus == Focus::Select
        && !app.active().panel()
        && !app.overlay_open()
        && app.popup().is_none();
    let strong = t.text.patch(bold());
    let items = hints(app);
    let mut spans = Vec::new();
    let mut used = 0;
    for (i, (text, applies)) in items.iter().enumerate() {
        let w = sw(text) + if i > 0 { 2 } else { 0 };
        if used + w > room {
            // Even the first item is cut when nothing else fits; later ones are dropped whole.
            if i == 0 {
                spans.push(Span::styled(truncate(text, room), t.hint));
            }
            break;
        }
        if i > 0 {
            spans.push(Span::styled("  ", t.hint));
        }
        let style = match (selecting, applies) {
            (true, true) => strong,
            _ => t.hint,
        };
        spans.push(Span::styled(text.clone(), style));
        used += w;
    }
    spans
}

/// A y/n question in bold. The uppercase danger word (EXECUTABLE) and "Delete" are red.
fn prompt_spans(line: &str, t: &Theme) -> Vec<Span<'static>> {
    let strong = t.text.patch(bold());
    line.split_inclusive(' ')
        .map(|word| {
            let danger = matches!(word.trim_end(), "EXECUTABLE" | "Delete");
            Span::styled(
                word.to_string(),
                if danger {
                    t.error.patch(bold())
                } else {
                    strong
                },
            )
        })
        .collect()
}

/// The footer line while a link waits: the host it really goes to, so a lookalike shows, and
/// the keys. A host with non-ASCII letters is flagged, since those can imitate a known site.
/// Also the delete question for one of your messages.
fn link_line(prompt: &Prompt, keys: &Keymap, ctx: Context) -> String {
    let host_of = |url: &str| {
        let host = chatcli_core::open::url_host(url).unwrap_or_else(|| "this link".to_string());
        if host.is_ascii() {
            host
        } else {
            format!("{host} (NON-ASCII NAME)")
        }
    };
    match prompt {
        Prompt::Confirm(url) => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            format!("Open {}? {yn}", host_of(url))
        }
        Prompt::Delete { .. } => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            format!("Delete for everyone? {yn}")
        }
        Prompt::MarkAllRead { count } => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            let what = if *count == 1 { "chat" } else { "chats" };
            format!("Mark {count} {what} read? {yn}")
        }
        Prompt::SignOut(net) => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            format!("Sign out of {}? {yn}", net.label())
        }
        Prompt::ResetKeys => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            format!("Reset every key to the preset? {yn}")
        }
        Prompt::SendFile {
            name,
            executable,
            onedrive,
            ..
        } => {
            let yn = keys
                .hint(ctx, &[Action::ConfirmYes, Action::ConfirmNo])
                .unwrap_or_else(|| "y/n".to_string());
            let name = truncate(&clean_line(name), 24);
            match (executable, onedrive) {
                (true, true) => format!("Upload EXECUTABLE {name} to your OneDrive? {yn}"),
                (true, false) => format!("Send EXECUTABLE {name}? {yn}"),
                _ => format!("Upload {name} to your OneDrive? The chat may not have access. {yn}"),
            }
        }
        Prompt::Pick(urls) => {
            let list: Vec<String> = urls
                .iter()
                .enumerate()
                .map(|(i, u)| format!("{} {}", i + 1, host_of(u)))
                .collect();
            let no = keys
                .hint(ctx, &[Action::ConfirmNo])
                .unwrap_or_else(|| "Esc".to_string());
            format!("Open which link? {}  {no} cancel", list.join("  "))
        }
    }
}

/// `-- NORMAL --` / `-- INSERT --` and the held sequence key, at the left of the footer. Only
/// with the vim preset; text, never colour alone.
fn mode_spans(app: &App) -> Vec<Span<'static>> {
    let Some(mode) = app.mode() else {
        return Vec::new();
    };
    let label = match mode {
        Mode::Normal => "-- NORMAL --",
        Mode::Insert => "-- INSERT --",
        Mode::Select => "-- SELECT --",
    };
    let mut spans = vec![Span::styled(label, bold())];
    if let Some(held) = app.pending_text() {
        spans.push(Span::styled(format!(" {held}"), bold()));
    }
    spans.push(Span::raw("  "));
    spans
}

fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut left = width;
    let mut out = Vec::new();
    for s in spans {
        if left == 0 {
            break;
        }
        let text = truncate(&s.content, left);
        left = left.saturating_sub(sw(&text));
        out.push(Span::styled(text, s.style));
    }
    Line::from(out)
}

/// Status and notice as footer rows, each wrapped to at most `FOOTER_WRAP_LINES`.
fn problem_lines(app: &App, w: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for part in [status_part(app), notice_part(app)].into_iter().flatten() {
        let mut lines = wrap(&part.content, w);
        let cut = lines.len() > FOOTER_WRAP_LINES;
        lines.truncate(FOOTER_WRAP_LINES);
        if cut {
            if let Some(last) = lines.last_mut() {
                *last = truncate(&format!("{last} ..."), w);
            }
        }
        out.extend(
            lines
                .into_iter()
                .map(|l| Line::from(Span::styled(l, part.style))),
        );
    }
    out
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect, screen_height: u16) {
    let t = app.theme();
    if area.height == 0 {
        return;
    }
    bar_background(f, t, Panel::Footer, area);
    let w = area.width as usize;
    let mode = mode_spans(app);
    let mode_w: usize = mode.iter().map(Span::width).sum();
    let mut hint_spans = mode.clone();
    hint_spans.extend(hint_spans_of(app, w.saturating_sub(mode_w)));
    let hint_line = Line::from(hint_spans);
    let mut rows: Vec<Line<'static>> = match screen_height {
        20.. => {
            let mut rows = problem_lines(app, w);
            rows.push(hint_line);
            rows
        }
        14..=19 => {
            let (status, notice) = (status_part(app), notice_part(app));
            let mut both: Vec<Span<'static>> = status.into_iter().collect();
            if !both.is_empty() && notice.is_some() {
                both.push(Span::styled("  |  ", t.hint));
            }
            both.extend(notice);
            if both.is_empty() {
                vec![hint_line]
            } else {
                vec![clip_spans(both, w), hint_line]
            }
        }
        // One row: a problem outranks the key hints, which return once it is gone.
        // The vim mode label stays: it is the one thing that must always be visible.
        // A link waiting for y/n outranks the problem: it is modal and its keys must be visible.
        _ => match status_part(app)
            .or(notice_part(app))
            .filter(|_| app.prompt().is_none())
        {
            Some(s) => {
                let mut spans = mode;
                spans.push(s);
                vec![clip_spans(spans, w)]
            }
            None => vec![hint_line],
        },
    };
    if let Some(cmd) = app.command_text() {
        // Same drawing as the filter box: a prefix, the text, the cursor at its end.
        let shown = tail(cmd, w.saturating_sub(2));
        let x = area.x + (1 + sw(&shown)).min(w.saturating_sub(1)) as u16;
        rows.insert(
            0,
            Line::from(vec![Span::styled(":", bold()), Span::raw(shown)]),
        );
        f.set_cursor_position((x, area.y));
    }
    f.render_widget(Paragraph::new(rows), area);
}

// ---- help overlay ----

const HELP_TOO_SMALL: &str = "Terminal too small for help, press Esc to close";
/// Smallest screen that fits the overlay: a header, a few rows of keys and a status row.
const HELP_MIN_W: u16 = 34;
const HELP_MIN_BODY_H: u16 = 8;
const HELP_MAX_W: u16 = 100;
/// Width of the key column at most; longer key lists are cut with an ellipsis.
const HELP_KEYS_MAX: usize = 30;
/// Every section's key column is at least this wide (the width the longest key of the first
/// sections needs), so the short sections line up the same as before the long ones existed.
const HELP_KEYS_MIN: usize = 9;
/// On a narrow box the key column stays this wide so the action names keep their room.
const HELP_KEYS_NARROW: usize = 12;
const HELP_NARROW_W: usize = 50;
const HELP_NAME_W: usize = 16;
/// The one-line doc is shown from this inner width on.
const HELP_DOC_MIN_W: usize = 64;

pub(crate) enum HelpRow {
    Title(String),
    Entry {
        keys: String,
        name: String,
        doc: &'static str,
    },
    Blank,
}

/// Every line of the help text, read from the active keymap (never a fixed list).
pub(crate) fn help_rows(app: &App) -> Vec<HelpRow> {
    // Commands first: they are what a new user can type without knowing any key.
    let mut rows = vec![HelpRow::Title("Commands".to_string())];
    for c in crate::commands::COMMANDS {
        let action = match c.id {
            crate::commands::CmdId::Help => Some(Action::Help),
            crate::commands::CmdId::Settings => Some(Action::Settings),
            _ => None,
        };
        rows.push(HelpRow::Entry {
            keys: format!("/{}", c.name),
            name: action
                .and_then(|a| app.keys().keys_for(Context::Global, a).into_iter().next())
                .unwrap_or_default(),
            doc: c.doc,
        });
    }
    for (ctx, entries) in app.keys().help_sections() {
        rows.push(HelpRow::Blank);
        rows.push(HelpRow::Title(ctx.title().to_string()));
        for (action, keys) in entries {
            rows.push(HelpRow::Entry {
                keys: keys.join(", "),
                name: action.label(),
                doc: action.doc(),
            });
        }
    }
    rows
}

fn help_fits(area: Rect, body: Rect) -> bool {
    area.width >= HELP_MIN_W && body.height >= HELP_MIN_BODY_H
}

/// The overlay fills the body (between header and footer), at most `HELP_MAX_W` wide and
/// centred. The body is cleared first, so nothing of the panes behind shows at its edges.
fn help_box(body: Rect) -> Rect {
    let w = body.width.min(HELP_MAX_W);
    Rect::new(body.x + (body.width - w) / 2, body.y, w, body.height)
}

/// Rows of help text visible at once on a screen of `size` (at least 1).
pub(crate) fn help_view_rows(app: &App, size: Rect) -> usize {
    // Box border (2), header row and status row.
    (app.layout_in(size).body.height as usize)
        .saturating_sub(4)
        .max(1)
}

fn draw_help(f: &mut Frame, app: &App, area: Rect, body: Rect) {
    let t = app.theme();
    let Some(top) = app.help_open() else { return };
    f.render_widget(Clear, body);
    if !help_fits(area, body) {
        centered_note(f, body, HELP_TOO_SMALL, t);
        return;
    }
    let b = help_box(body);
    f.render_widget(pane(app.theme(), Panel::Popup, "Help", true), b);
    let inner = b.inner(Margin::new(1, 1));
    let w = inner.width as usize;
    let user = app.keys().user_bindings();
    let header = format!(
        "{} preset, {}",
        app.keys().preset().name(),
        if user == 0 {
            "no user bindings".to_string()
        } else {
            format!("{user} user bindings")
        }
    );
    f.render_widget(
        Paragraph::new(Span::styled(truncate(&header, w), t.hint)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let rows = help_rows(app);
    let view = help_view_rows(app, area);
    // A notice expiring can make the body taller since the last key: show a full page.
    let top = top.min(rows.len().saturating_sub(view));
    // Each section sizes its own key column, so one long key list (Alt+Enter, Shift+Enter) does
    // not push every action name in the other sections to the right.
    let cap = if w < HELP_NARROW_W {
        HELP_KEYS_NARROW
    } else {
        HELP_KEYS_MAX
    };
    let mut key_ws = vec![HELP_KEYS_MIN; rows.len()];
    let mut start = 0;
    for i in 0..=rows.len() {
        if i == rows.len() || matches!(rows[i], HelpRow::Blank) {
            let widest = rows[start..i]
                .iter()
                .filter_map(|r| match r {
                    HelpRow::Entry { keys, .. } => Some(sw(keys)),
                    _ => None,
                })
                .max()
                .unwrap_or(HELP_KEYS_MIN)
                .clamp(HELP_KEYS_MIN, cap);
            key_ws[start..i].fill(widest);
            start = i + 1;
        }
    }
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(top)
        .take(view)
        .map(|(i, r)| match r {
            HelpRow::Title(t) => Line::from(Span::styled(t.clone(), bold())),
            HelpRow::Blank => Line::from(""),
            HelpRow::Entry { keys, name, doc } => {
                let mut spans = vec![
                    Span::styled(fit(keys, key_ws[i]), bold()),
                    Span::raw(" "),
                    Span::raw(fit(name, HELP_NAME_W)),
                ];
                if w >= HELP_DOC_MIN_W {
                    spans.push(Span::styled(format!("  {doc}"), t.dim));
                }
                clip_spans(spans, w)
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y + 1, inner.width, view as u16).intersection(inner),
    );
    let last = (top + view).min(rows.len());
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate(
                &format!("lines {}-{} of {}", (top + 1).min(last), last, rows.len()),
                w,
            ),
            t.hint,
        )),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

// ---- panes ----

/// A framed panel in the palette's look. Every outline keeps the one-cell frame, so the text
/// inside sits in the same place whatever the palette.
pub(crate) fn pane(t: &Theme, panel: Panel, title: &str, focused: bool) -> Block<'static> {
    let look = t.panel(panel);
    let (outline, bg) = if focused {
        (look.focus_outline, look.focus_bg)
    } else {
        (look.outline, look.bg)
    };
    let mut b = Block::bordered()
        .border_set(outline.set())
        .title(Span::styled(
            format!(" {title} "),
            if focused { bold() } else { Style::new() },
        ));
    if let Some(c) = bg {
        b = b.style(Style::new().bg(c));
    }
    match outline {
        Outline::Rule if !focused => b.border_style(t.dim),
        _ if focused && look.focus_tint => b.border_style(t.accent),
        _ => b,
    }
}

/// Fills a one-line bar (top bar, footer) with its palette background, if it has one.
fn bar_background(f: &mut Frame, t: &Theme, panel: Panel, area: Rect) {
    if let Some(c) = t.panel(panel).bg {
        f.render_widget(Block::new().style(Style::new().bg(c)), area);
    }
}

pub(crate) fn centered_note(f: &mut Frame, area: Rect, text: &str, t: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let r = Rect::new(
        area.x,
        area.y + area.height / 3,
        area.width,
        area.height - area.height / 3,
    );
    f.render_widget(
        Paragraph::new(text.to_string())
            .style(t.hint)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        r,
    );
}

fn draw_sidebar(f: &mut Frame, app: &App, lay: &Layout) {
    let t = app.theme();
    let look = app.look();
    let ns = app.active();
    let focused = matches!(ns.focus, Focus::List | Focus::Filter);
    // Reverse video marks the chat only while the list has the keys (see the row drawing).
    let list_has_keys = !matches!(ns.focus, Focus::Composer | Focus::Select);
    let title = if ns.show_hidden {
        "Chats (hidden shown)"
    } else {
        "Chats"
    };
    f.render_widget(
        pane(app.theme(), Panel::Sidebar, title, focused),
        lay.sidebar,
    );
    if let Some(r) = lay.filter_row {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/", bold()),
                Span::raw(ns.filter.clone()),
            ])),
            r,
        );
        if ns.focus == Focus::Filter && app.help_open().is_none() && app.command_text().is_none() {
            let x = r.x + (1 + sw(&ns.filter)).min(r.width.saturating_sub(1) as usize) as u16;
            f.set_cursor_position((x, r.y));
        }
    }
    let list = lay.list;
    let vis = ns.visible();
    if vis.is_empty() {
        let note = if !ns.chats.is_empty() {
            if ns.filter.is_empty() {
                let key = app
                    .keys()
                    .hint(app.context(), &[Action::ShowHidden])
                    .unwrap_or_else(|| "the show_hidden key".to_string());
                format!("Every chat is hidden: press {key} to see them")
            } else {
                format!("No chats match \"{}\"", ns.filter)
            }
        } else {
            match ns.status {
                Status::Starting | Status::Connecting => "Loading chats...".to_string(),
                Status::Error { .. } => "No chats loaded. See the line below.".to_string(),
                _ => "No chats yet".to_string(),
            }
        };
        centered_note(f, list, &note, t);
        return;
    }
    let w = list.width as usize;
    let row_h = app.row_h() as usize;
    let now = look.clock.now_ms;
    for (k, &ci) in vis
        .iter()
        .skip(ns.list_top)
        .take(list.height as usize / row_h)
        .enumerate()
    {
        let c = &ns.chats[ci];
        let selected = ns.selected.as_ref() == Some(&c.id);
        let muted = chats::is_muted(c, now);
        let (num_style, title_style, sub_style) = if selected {
            (Style::new(), Style::new(), Style::new())
        } else if muted {
            // Muted: the count is dimmed and the title never bold.
            (t.dim, t.text, t.dim)
        } else {
            (
                t.unread,
                if c.unread > 0 { t.unread } else { t.text },
                t.dim,
            )
        };
        let num = match c.unread {
            0 => String::new(),
            1..=99 => c.unread.to_string(),
            _ => "99+".to_string(),
        };
        let time = c
            .last_ts
            .map(|ts| look.clock.list_time_f(ts, look.fmt))
            .unwrap_or_default();
        let time_w = if time.is_empty() { 0 } else { sw(&time) + 1 };
        // Someone typing here: the preview line says so; a one-row chat gets the word instead.
        let typing = ns.typing.get(&c.id).and_then(|ty| ty.text());
        let mut marks = chats::marks(c, now);
        if typing.is_some() && row_h == 1 {
            marks = format!("typing {marks}").trim().to_string();
        }
        let marks_w = if marks.is_empty() { 0 } else { sw(&marks) + 1 };
        let title = fit(&c.title, w.saturating_sub(ROW_PREFIX + time_w + marks_w));
        let mut row1 = vec![
            Span::styled(format!("{num:>3}"), num_style),
            // A bold `@` after the count: some of the unread messages mention you. A space
            // follows, so it never reads as the start of the chat's name.
            Span::styled(
                if c.mentions > 0 { "@" } else { " " },
                num_style.patch(bold()),
            ),
            Span::raw(" "),
            Span::styled(title, title_style),
        ];
        if !marks.is_empty() {
            row1.push(Span::styled(format!(" {marks}"), sub_style));
        }
        row1.push(Span::styled(
            if time.is_empty() {
                String::new()
            } else {
                format!(" {time}")
            },
            sub_style,
        ));
        let mut rows = vec![Line::from(row1)];
        if row_h > 1 {
            rows.push(match typing {
                Some(text) => Line::from(Span::styled(
                    format!(
                        "{:ROW_PREFIX$}{}",
                        "",
                        truncate(&text, w.saturating_sub(ROW_PREFIX))
                    ),
                    if selected { Style::new() } else { t.accent },
                )),
                None => Line::from(Span::styled(
                    format!(
                        "{:ROW_PREFIX$}{}",
                        "",
                        truncate(&c.last_preview, w.saturating_sub(ROW_PREFIX))
                    ),
                    sub_style,
                )),
            });
        }
        let r = Rect::new(
            list.x,
            list.y + (row_h * k) as u16,
            list.width,
            row_h as u16,
        )
        .intersection(list);
        // The highlighted chat is reverse video while the list has the keys. Once the keys are in
        // the message pane that block belongs to the message, so the chat is only bold and
        // underlined: one reverse block shows where the keys go.
        let (base, rows) = if selected && !list_has_keys {
            let marked = Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            let rows = rows
                .into_iter()
                .map(|l| {
                    Line::from(
                        l.spans
                            .into_iter()
                            .map(|s| Span::styled(s.content, s.style.patch(marked)))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            (Style::new(), rows)
        } else if selected {
            (t.selected, rows)
        } else {
            (Style::new(), rows)
        };
        f.render_widget(Paragraph::new(rows).style(base), r);
    }
}

/// Where one message sits in the wrapped conversation: lines `start..end` (its quote, body, tags
/// and reactions, not the day separator above it).
pub(crate) struct MsgSpan {
    pub id: String,
    pub start: usize,
    pub end: usize,
    /// Thumbnails drawn under this message, for the terminal graphics overlay.
    pub images: Vec<ImageSlot>,
}

/// Where a thumbnail sits in the conversation lines: `rows` lines from `line`, `cols` wide,
/// starting `indent` columns in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageSlot {
    pub key: MediaKey,
    pub line: usize,
    pub cols: usize,
    pub rows: usize,
    pub indent: usize,
}

/// Longest file name in a chip.
const CHIP_NAME_MAX: usize = 30;

/// `[image sunset.png 240 KB]`: what the attachment is, never its content.
fn chip_label(a: &chatcli_core::model::Attachment) -> String {
    let mut parts = vec![kind_label(a.kind).to_string()];
    if let Some(name) = a.name.as_deref().filter(|n| !n.trim().is_empty()) {
        parts.push(truncate(name, CHIP_NAME_MAX));
    }
    if let Some(size) = a.size {
        parts.push(human_size(size));
    }
    format!("[{}]", parts.join(" "))
}

/// Every line of the open conversation, wrapped to `width`, oldest first.
pub(crate) fn message_lines(ns: &NetState, look: &Look, width: usize) -> Vec<Line<'static>> {
    message_layout(ns, look, width).0
}

/// Adds a short dim tag (` [sending]`, ` (edited)`) after the last row, or on a row of its own
/// when it does not fit.
fn push_tag(
    rows: &mut Vec<Vec<Span<'static>>>,
    tag: &str,
    width: usize,
    indent_w: usize,
    t: &Theme,
) {
    let fits = rows
        .last()
        .is_some_and(|r| r.iter().map(Span::width).sum::<usize>() + sw(tag) <= width);
    if fits {
        if let Some(last) = rows.last_mut() {
            last.push(Span::styled(tag.to_string(), t.dim));
        }
    } else {
        rows.push(vec![
            Span::raw(" ".repeat(indent_w)),
            Span::styled(tag.trim_start().to_string(), t.dim),
        ]);
    }
}

/// The conversation lines and where each message sits in them. A message you have selected
/// is drawn in reverse video over its text, row by row (never a stripe on one edge).
pub(crate) fn message_layout(
    ns: &NetState,
    look: &Look,
    width: usize,
) -> (Vec<Line<'static>>, Vec<MsgSpan>) {
    let (t, clock, fmt) = (look.theme, look.clock, look.fmt);
    let Some(msgs) = ns.open.as_ref().and_then(|id| ns.messages.get(id)) else {
        return (Vec::new(), Vec::new());
    };
    let label = |m: &ChatMessage| {
        if m.from_me {
            "You".to_string()
        } else {
            m.sender.clone()
        }
    };
    // Name rows: the name and time on a row of their own with the text underneath (every chat
    // unless `avatars = "off"`, which keeps the one-line layout). Avatars beside them: group
    // chats, or every chat with `all`, when there is room.
    let name_rows = look.avatars != Avatars::Off;
    let avatars = width >= AVATAR_MIN_WIDTH
        && match look.avatars {
            Avatars::All => true,
            Avatars::Groups => ns.open_is_group(),
            Avatars::Off => false,
        };
    let sender_w = msgs
        .iter()
        .map(|m| sw(&label(m)))
        .max()
        .unwrap_or(0)
        .min(SENDER_MAX)
        .min(width.saturating_sub(fmt.time_width() + 10));
    let indent_w = if avatars {
        AVATAR_W + AVATAR_GAP
    } else if name_rows {
        0
    } else {
        fmt.time_width() + 1 + sender_w + 2
    };
    let text_w = width.saturating_sub(indent_w).max(1);
    let chat_id = ns.open.as_deref();
    let selected = if ns.focus == Focus::Select {
        ns.sel_msg.as_deref()
    } else {
        None
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    // At the very top: the history is being loaded, or there is no more of it.
    let top_note = match chat_id
        .and_then(|c| ns.older.get(c))
        .copied()
        .unwrap_or_default()
    {
        Older::Loading => Some("loading older..."),
        Older::AtStart => Some("start of conversation"),
        Older::Idle => None,
    };
    if let Some(note) = top_note {
        lines.push(Line::from(Span::styled(note, t.hint)).alignment(Alignment::Center));
    }
    let mut spans: Vec<MsgSpan> = Vec::new();
    let mut last_day = None;
    let mut prev: Option<&ChatMessage> = None;
    for m in msgs {
        let day = clock.day(m.ts);
        let new_day = day != last_day || spans.is_empty();
        if new_day {
            last_day = day;
            // Room above the day line, as between people.
            if name_rows && !spans.is_empty() {
                lines.push(Line::default());
            }
            let sep = format!("---- {} ----", clock.day_label_f(m.ts, fmt));
            lines.push(Line::from(Span::styled(sep, t.dim)).alignment(Alignment::Center));
        }
        // Avatar layout: a run of messages from one person shares one name row and avatar, and
        // a blank row sets each run apart. A reply starts a run, so its quote sits under a name.
        let starts_run = name_rows
            && (new_day
                || m.reply_to.is_some()
                || prev.is_none_or(|p| person_key(p) != person_key(m) || m.ts - p.ts > GROUP_MS));
        if starts_run && !new_day {
            lines.push(Line::default());
        }
        prev = Some(m);
        let first_line = lines.len();
        let deleted = matches!(m.state, MessageState::Deleted);
        // Attachments are drawn as chips, so the backends' `[image]` markers leave the text.
        let shown = if m.attachments.is_empty() {
            m.text.clone()
        } else {
            strip_markers(&m.text, m.attachments.len())
        };
        let (body, body_style) = match &m.state {
            MessageState::Deleted => ("[message deleted]".to_string(), t.dim),
            MessageState::Failed(_) => (shown, t.error),
            // A message that mentions you stands out in bold.
            _ if m.mentions_me => (shown, t.text.patch(bold())),
            _ => (shown, t.text),
        };
        let who_style = if m.from_me { t.own_name } else { bold() };
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        // Avatar layout: name (in the person's colour) and time on a row of their own, the
        // avatar's top half beside them and its bottom half beside the first row of text.
        let name_style = if m.from_me {
            t.own_name
        } else {
            t.person(&person_key(m))
        };
        let (av_top, av_bottom) = avatar_cells(&label(m), name_style);
        let mut av_pending = starts_run && avatars;
        if starts_run {
            let time = clock.hm_f(m.ts, fmt);
            let name_w = text_w.saturating_sub(sw(&time) + 1).max(1);
            let mut row = if avatars {
                vec![av_top, Span::raw(" ".repeat(AVATAR_GAP))]
            } else {
                Vec::new()
            };
            row.extend([
                Span::styled(truncate(&label(m), name_w), name_style),
                Span::raw(" "),
                Span::styled(time, t.dim),
            ]);
            rows.push(row);
        }
        // Time and sender go on the first row of the message, whatever that row is.
        let mut need_lead = !name_rows;
        let lead = |need: &mut bool, av: &mut bool| -> Vec<Span<'static>> {
            if std::mem::take(av) {
                vec![av_bottom.clone(), Span::raw(" ".repeat(AVATAR_GAP))]
            } else if std::mem::take(need) {
                vec![
                    Span::styled(clock.hm_f(m.ts, fmt), t.dim),
                    Span::raw(" "),
                    Span::styled(fit(&label(m), sender_w), who_style),
                    Span::raw("  "),
                ]
            } else {
                vec![Span::raw(" ".repeat(indent_w))]
            }
        };
        // A reply shows what it quotes, dimmed, on one line above the body.
        if let Some(q) = &m.reply_to {
            let who = if q.sender.is_empty() {
                "message"
            } else {
                &q.sender
            };
            let what = if q.preview.is_empty() {
                "[original message not available]"
            } else {
                &q.preview
            };
            let mut row = lead(&mut need_lead, &mut av_pending);
            row.push(Span::styled(
                truncate(&format!("> {who}: {what}"), text_w),
                t.dim,
            ));
            rows.push(row);
        }
        // Marks come from the stored text, not the wrapped lines, so a link that wraps is
        // underlined on every line it covers. Mentions are bold.
        let mut marks: Vec<(usize, usize, Modifier)> = Vec::new();
        if !deleted {
            marks.extend(
                find_links(&body)
                    .into_iter()
                    .map(|l| (l.start, l.end, Modifier::UNDERLINED)),
            );
            let mut from = 0;
            for p in &m.mentions {
                let token = format!("@{}", p.name);
                if let Some(at) = body[from..].find(&token) {
                    let (a, b) = (from + at, from + at + token.len());
                    marks.push((a, b, Modifier::BOLD));
                    from = b;
                }
            }
        }
        // A message that is only attachments has no body line: the chips carry the time and name.
        if !(body.trim().is_empty() && !m.attachments.is_empty()) {
            for pieces in wrap_pieces(&body, text_w) {
                let mut row = lead(&mut need_lead, &mut av_pending);
                row.extend(piece_spans(&pieces, &marks, body_style));
                rows.push(row);
            }
        }
        // One chip per attachment, with what happened to it; a thumbnail under it when shown.
        let mut thumb_rows: Vec<std::ops::Range<usize>> = Vec::new();
        let mut slots: Vec<ImageSlot> = Vec::new();
        for (idx, a) in m.attachments.iter().enumerate() {
            let item = chat_id.and_then(|c| ns.media.get(c, &m.id, idx as u32));
            let label = chip_label(a);
            let mut row = lead(&mut need_lead, &mut av_pending);
            match item.and_then(MediaState::status_text) {
                // The state shares the chip's row when it fits, else it gets rows of its own: a
                // reason cut off ("FAILED: MEDIA EXP...") is no reason.
                Some((text, is_error)) if sw(&label) + 2 + sw(&text) <= text_w => {
                    row.push(Span::styled(label, bold()));
                    row.push(Span::raw("  "));
                    row.push(Span::styled(text, if is_error { t.error } else { t.dim }));
                    rows.push(row);
                }
                Some((text, is_error)) => {
                    row.extend(clip_spans(vec![Span::styled(label, bold())], text_w).spans);
                    rows.push(row);
                    for line in wrap(&text, text_w) {
                        rows.push(vec![
                            Span::raw(" ".repeat(indent_w)),
                            Span::styled(line, if is_error { t.error } else { t.dim }),
                        ]);
                    }
                }
                None => {
                    row.extend(clip_spans(vec![Span::styled(label, bold())], text_w).spans);
                    rows.push(row);
                }
            }
            if let Some(View::Shown(t)) = item.map(|i| &i.view) {
                let (cols, n_rows) = t.cells(text_w);
                let start = rows.len();
                for line in t.lines(text_w) {
                    let mut row = vec![Span::raw(" ".repeat(indent_w))];
                    row.extend(line);
                    rows.push(row);
                }
                thumb_rows.push(start..rows.len());
                if let (Some(c), Some(Fetch::Ready(_))) = (chat_id, item.map(|i| &i.fetch)) {
                    slots.push(ImageSlot {
                        key: MediaKey {
                            chat: c.to_string(),
                            msg: m.id.clone(),
                            idx: idx as u32,
                        },
                        line: first_line + start,
                        cols,
                        rows: n_rows,
                        indent: indent_w,
                    });
                }
            }
        }
        match &m.state {
            MessageState::Pending => {
                // Upload progress of a file in flight, when the backend reported any.
                let tag = match ns.progress.get(&m.id).and_then(|&(d, t)| percent(d, t)) {
                    Some(p) => format!(" [sending {p}%]"),
                    None => " [sending]".to_string(),
                };
                push_tag(&mut rows, &tag, width, indent_w, t);
            }
            MessageState::Failed(reason) => {
                for row in wrap(&format!("NOT SENT - {reason}"), text_w) {
                    rows.push(vec![
                        Span::raw(" ".repeat(indent_w)),
                        Span::styled(row, t.error.patch(bold())),
                    ]);
                }
            }
            _ => {}
        }
        if m.edited && !deleted {
            push_tag(&mut rows, " (edited)", width, indent_w, t);
        }
        // Reaction counts under the message, your own in bold.
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = indent_w;
        for r in &m.reactions {
            let text = format!("{} {}", r.emoji, r.count);
            let w = sw(&text);
            if used > indent_w && used + 2 + w > indent_w + text_w {
                rows.push(std::mem::take(&mut row));
                used = indent_w;
            }
            if used == indent_w {
                row = vec![Span::raw(" ".repeat(indent_w))];
            } else {
                row.push(Span::raw("  "));
                used += 2;
            }
            row.push(Span::styled(
                text,
                if r.mine { t.text.patch(bold()) } else { t.text },
            ));
            used += w;
        }
        if used > indent_w {
            rows.push(row);
        }
        let style = if selected == Some(m.id.as_str()) {
            t.selected
        } else {
            Style::new()
        };
        // The highlight is reverse video: a thumbnail drawn in it would turn upside down.
        lines.extend(rows.into_iter().enumerate().map(|(i, r)| {
            let plain = thumb_rows.iter().any(|t| t.contains(&i));
            Line::from(r).style(if plain { Style::new() } else { style })
        }));
        spans.push(MsgSpan {
            id: m.id.clone(),
            start: first_line,
            end: lines.len(),
            images: slots,
        });
    }
    (lines, spans)
}

/// Who sent a message, for grouping and name colours: their network id when known. `:me` can
/// not clash with a real id (Teams ids are GUIDs, WhatsApp ids have an `@`).
fn person_key(m: &ChatMessage) -> String {
    if m.from_me {
        ":me".to_string()
    } else if m.sender_id.is_empty() {
        m.sender.clone()
    } else {
        m.sender_id.clone()
    }
}

/// A person's initials (`Alex Example` -> `AE`, a phone number -> `#`), at most two.
fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let first = |w: &str| w.chars().next();
    let picked: Vec<char> = match words.as_slice() {
        [] => Vec::new(),
        [w] => first(w).into_iter().collect(),
        [a, .., b] => first(a).into_iter().chain(first(b)).collect(),
    };
    if picked.first().is_some_and(|c| !c.is_alphabetic()) {
        return "#".to_string();
    }
    picked.into_iter().flat_map(char::to_uppercase).collect()
}

/// The two rows of an initials avatar: a block in the person's colour (reverse video when there
/// is no colour), the initials centred on the top row.
fn avatar_cells(name: &str, person: Style) -> (Span<'static>, Span<'static>) {
    let style = match person.fg {
        Some(c) => Style::new()
            .bg(c)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        None => Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
    };
    let ini = initials(name);
    let ini = if sw(&ini) > AVATAR_W {
        String::new()
    } else {
        ini
    };
    let left = (AVATAR_W - sw(&ini)) / 2;
    let top = format!(
        "{}{ini}{}",
        " ".repeat(left),
        " ".repeat(AVATAR_W - sw(&ini) - left)
    );
    (
        Span::styled(top, style),
        Span::styled(" ".repeat(AVATAR_W), style),
    )
}

/// The spans of one wrapped line: words joined by one space, the parts that lie inside a mark
/// (`marks` are byte ranges in the wrapped text) drawn with the mark's modifier.
fn piece_spans(
    pieces: &[Piece],
    marks: &[(usize, usize, Modifier)],
    base: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (k, p) in pieces.iter().enumerate() {
        if k > 0 {
            out.push(Span::styled(" ", base));
        }
        let end = p.start + p.text.len();
        // Cut points inside this piece, as offsets into `p.text`.
        let mut cuts = vec![0, p.text.len()];
        for &(a, b, _) in marks {
            for edge in [a, b] {
                if edge > p.start && edge < end {
                    cuts.push(edge - p.start);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let style = marks
                .iter()
                .filter(|&&(x, y, _)| p.start + a >= x && p.start + b <= y)
                .fold(base, |s, &(_, _, m)| s.add_modifier(m));
            if let Some(text) = p.text.get(a..b) {
                out.push(Span::styled(text.to_string(), style));
            }
        }
    }
    out
}

fn draw_convo(f: &mut Frame, app: &App, lay: &Layout) {
    let t = app.theme();
    let ns = app.active();
    let Some(open) = ns.open.as_ref() else {
        f.render_widget(
            pane(app.theme(), Panel::Messages, "Conversation", false),
            lay.convo,
        );
        centered_note(
            f,
            lay.messages,
            &format!(
                "Select a chat and press Enter to open it.\n{}",
                crate::commands::COMMANDS_HINT
            ),
            t,
        );
        return;
    };
    let name = ns
        .chats
        .iter()
        .find(|c| &c.id == open)
        .map_or("Chat", |c| c.title.as_str());
    let mut title = truncate(name, (lay.convo.width as usize).saturating_sub(6));
    if ns.scroll > 0 {
        title = truncate(
            &format!("{title} - scrolled, PgDn for newer"),
            (lay.convo.width as usize).saturating_sub(4),
        );
    }
    f.render_widget(
        pane(
            app.theme(),
            Panel::Messages,
            &title,
            ns.focus == Focus::Select,
        ),
        lay.convo,
    );
    if !ns.messages.contains_key(open) {
        centered_note(f, lay.messages, "Loading messages...", t);
        return;
    }
    let lines = message_lines(ns, &app.look(), lay.messages.width as usize);
    if lines.is_empty() {
        centered_note(f, lay.messages, "No messages in this chat yet.", t);
        return;
    }
    let height = lay.messages.height as usize;
    let end = lines.len() - ns.scroll.min(lines.len().saturating_sub(height));
    let start = end.saturating_sub(height);
    f.render_widget(
        Paragraph::new(lines[start..end].to_vec()),
        bottom_anchored(lay.messages, end - start),
    );
}

/// The bottom `rows` rows of the message area: a short conversation sits next to the message
/// box, as in other chat apps, instead of under the title.
fn bottom_anchored(area: Rect, rows: usize) -> Rect {
    let rows = (rows as u16).min(area.height);
    Rect::new(area.x, area.y + area.height - rows, area.width, rows)
}

/// A thumbnail that is wholly on screen: where, and the cached file it comes from. The terminal
/// graphics overlay (Sixel, Kitty, iTerm2) draws these over the half-block thumbnail. A
/// thumbnail only partly on screen keeps its half-blocks, which clip with the lines.
pub(crate) struct VisibleImage {
    pub path: PathBuf,
    pub rect: Rect,
}

pub(crate) fn visible_images(app: &App, lay: &Layout) -> Vec<VisibleImage> {
    let ns = app.active();
    // Something drawn over the conversation (help, a list, a question) must stay on top.
    if ns.panel() || ns.open.is_none() || app.overlay_open() || app.popup().is_some() {
        return Vec::new();
    }
    // A terminal too small to draw the conversation, or one that hides it (narrow list).
    let drawn_height = lay.header.height + lay.body.height + lay.footer.height;
    if lay.header.width < MIN_W || drawn_height < MIN_H || lay.messages.width == 0 {
        return Vec::new();
    }
    let (lines, spans) = message_layout(ns, &app.look(), lay.messages.width as usize);
    let height = lay.messages.height as usize;
    let end = lines.len() - ns.scroll.min(lines.len().saturating_sub(height));
    let start = end.saturating_sub(height);
    let area = bottom_anchored(lay.messages, end - start);
    let mut out = Vec::new();
    for slot in spans.iter().flat_map(|s| &s.images) {
        if slot.line < start || slot.line + slot.rows > end || slot.cols == 0 {
            continue;
        }
        let Some(Fetch::Ready(path)) = ns.media.items.get(&slot.key).map(|i| i.fetch.clone())
        else {
            continue;
        };
        out.push(VisibleImage {
            path,
            rect: Rect::new(
                area.x + slot.indent as u16,
                area.y + (slot.line - start) as u16,
                slot.cols as u16,
                slot.rows as u16,
            )
            .intersection(lay.messages),
        });
    }
    out
}

/// The reply or edit line, and the file chip, above the text in the message box.
fn composer_extras(app: &App, ns: &NetState, width: usize) -> Vec<Line<'static>> {
    let t = app.theme();
    let mut out = Vec::new();
    if let Some(target) = ns.target() {
        let esc = app
            .keys()
            .hint(
                app.context(),
                &[
                    Action::LeaveComposer,
                    Action::LeaveInsert,
                    Action::CancelReply,
                ],
            )
            .unwrap_or_else(|| "Esc".to_string());
        let tail = format!("  {esc} cancel");
        let room = width.saturating_sub(sw(&tail));
        let head = match target {
            Target::Reply(q) => {
                let who = if q.sender.is_empty() {
                    "message"
                } else {
                    &q.sender
                };
                let what = if q.preview.is_empty() {
                    "[original message not available]"
                } else {
                    &q.preview
                };
                format!("Replying to {who}: {what}")
            }
            Target::Edit { original, .. } => format!(
                "Editing: {}",
                original.split_whitespace().collect::<Vec<_>>().join(" ")
            ),
        };
        out.push(Line::from(vec![
            Span::styled(truncate(&head, room), t.dim),
            Span::styled(tail, t.hint),
        ]));
    }
    if let Some(p) = ns.attachment() {
        let esc = app
            .keys()
            .hint(app.context(), &[Action::LeaveComposer, Action::LeaveInsert])
            .unwrap_or_else(|| "Esc".to_string());
        let tail = format!("  {esc} remove");
        let room = width.saturating_sub(sw(&tail));
        out.push(Line::from(vec![
            Span::styled(truncate(&p.chip(), room), bold()),
            Span::styled(tail, t.hint),
        ]));
    }
    out
}

/// The path prompt: the message box becomes its input line, the matching folder entries list
/// above it like the `:shortcode` suggestions.
fn draw_attach(f: &mut Frame, app: &App, lay: &Layout) {
    let t = app.theme();
    let Some(a) = app.attach_prompt() else { return };
    let r = lay.composer;
    f.render_widget(Clear, r);
    f.render_widget(pane(app.theme(), Panel::Composer, "Attach file", true), r);
    let inner = r.inner(Margin::new(1, 1));
    if inner.height == 0 {
        return;
    }
    let w = inner.width as usize;
    let shown = tail(&clean_line(&a.input), w.saturating_sub(2));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", bold()),
            Span::raw(shown.clone()),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let x = (2 + sw(&shown)).min(w.saturating_sub(1)) as u16;
    f.set_cursor_position((inner.x + x, inner.y));
    if lay.composer.width < 12 {
        return;
    }
    let room = (lay.convo.height as usize).saturating_sub(4);
    let n = a.cands.len().clamp(1, 10).min(room);
    if n == 0 {
        return;
    }
    let h = n as u16 + 2;
    let list = Rect::new(
        lay.composer.x + 1,
        lay.composer.y.saturating_sub(1 + h),
        lay.composer.width - 2,
        h,
    );
    f.render_widget(Clear, list);
    f.render_widget(pane(app.theme(), Panel::Popup, "Files", false), list);
    let rows = list.inner(Margin::new(1, 1));
    if a.cands.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("No match: Enter says FILE NOT FOUND", t.hint)),
            rows,
        );
        return;
    }
    // Keep the highlighted entry in the window.
    let top = (a.sel + 1).saturating_sub(n);
    for (k, c) in a.cands.iter().skip(top).take(n).enumerate() {
        let mark = if c.dir { "/" } else { "" };
        let text = truncate(
            &format!("{}{mark}", clean_line(&c.name)),
            rows.width as usize,
        );
        let style = if top + k == a.sel {
            t.selected
        } else if c.dir {
            bold()
        } else {
            Style::new()
        };
        f.render_widget(
            Paragraph::new(Span::styled(text, style)),
            Rect::new(rows.x, rows.y + k as u16, rows.width, 1),
        );
    }
}

fn draw_composer(f: &mut Frame, app: &App, lay: &Layout) {
    let t = app.theme();
    let ns = app.active();
    let focused = ns.focus == Focus::Composer;
    let mut block = pane(app.theme(), Panel::Composer, "Message", focused);
    // Someone is typing in this chat: on the box's top edge, right-aligned (WhatsApp only).
    if let Some(text) = ns
        .open
        .as_ref()
        .and_then(|id| ns.typing.get(id))
        .and_then(|ty| ty.text())
    {
        let room = (lay.composer.width as usize).saturating_sub(14);
        block = block.title(
            Line::from(Span::styled(
                format!(" {} ", truncate(&text, room)),
                t.accent,
            ))
            .right_aligned(),
        );
    }
    f.render_widget(block, lay.composer);
    let inner = lay.composer.inner(Margin::new(1, 1));
    if inner.height == 0 {
        return;
    }
    // The reply or edit line and the file chip take the first rows, while one row is left
    // for the text.
    let extras = composer_extras(app, ns, inner.width as usize);
    let take = extras.len().min(inner.height.saturating_sub(1) as usize);
    for (i, line) in extras.into_iter().take(take).enumerate() {
        f.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
        );
    }
    let inner = Rect::new(
        inner.x,
        inner.y + take as u16,
        inner.width,
        inner.height - take as u16,
    );
    if ns.open.is_none() {
        f.render_widget(
            Paragraph::new(Span::styled("Open a chat to type", t.hint)),
            inner,
        );
        return;
    }
    let draft = ns.draft();
    if draft.is_empty() && !focused {
        // Focus is on the list: Enter there opens the selected chat and letters are commands,
        // so the placeholder must not invite typing. While selecting, the way back is Esc.
        let note = if ns.focus == Focus::Select {
            let back = app
                .keys()
                .hint(Context::Select, &[Action::LeaveSelect])
                .unwrap_or_else(|| "Esc".to_string());
            format!("{back} to go back and type")
        } else {
            "Enter on a chat to type".to_string()
        };
        f.render_widget(Paragraph::new(Span::styled(note, t.hint)), inner);
        return;
    }
    let empty = Composer::default();
    let comp = ns.composer().unwrap_or(&empty);
    let (rows, crow, ccol) = comp.layout(composer_text_width(lay.composer.width));
    // Show the rows around the cursor when the draft is taller than the box.
    let visible = inner.height as usize;
    let top = composer::top_row(crow, visible);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(i, r)| {
            Line::from(vec![
                Span::styled(if i == 0 { "> " } else { "  " }, bold()),
                Span::raw(r.text.clone()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
    // Rows above the window: the draft starts higher than it shows, so say how many.
    if top > 0 && lay.composer.width > 8 {
        let cue = truncate(
            &format!(" ^ {top} more "),
            (lay.composer.width as usize).saturating_sub(4),
        );
        let r = Rect::new(
            lay.composer.x + 2,
            lay.composer.y + lay.composer.height.saturating_sub(1),
            sw(&cue) as u16,
            1,
        );
        f.render_widget(Paragraph::new(Span::styled(cue, t.hint)), r);
    }
    if focused && !app.overlay_open() {
        let x = (2 + ccol).min(inner.width.saturating_sub(1) as usize) as u16;
        f.set_cursor_position((inner.x + x, inner.y + (crow - top) as u16));
    }
}

/// The `:shortcode` list, drawn just above the message box and over the conversation.
fn draw_popup(f: &mut Frame, app: &App, lay: &Layout) {
    let t = app.theme();
    let Some(p) = app.popup() else { return };
    if app.overlay_open() || lay.composer.width < 18 {
        return;
    }
    // Inside the conversation pane, above its bottom border: its inner rows minus the list's own
    // two border rows.
    let room = (lay.convo.height as usize).saturating_sub(4);
    let n = p.items.len().min(room);
    if n == 0 {
        return;
    }
    // The full width of the conversation pane, so no half of a wide character of the messages
    // underneath is left at the list's edges.
    let h = n as u16 + 2;
    let r = Rect::new(
        lay.composer.x + 1,
        lay.composer.y - 1 - h,
        lay.composer.width - 2,
        h,
    );
    f.render_widget(Clear, r);
    f.render_widget(pane(app.theme(), Panel::Popup, p.title, false), r);
    let inner = r.inner(Margin::new(1, 1));
    // A list longer than the box scrolls to keep the chosen row in view.
    let start = (p.selected + 1).saturating_sub(n);
    for (k, item) in p.items.iter().skip(start).take(n).enumerate() {
        let selected = start + k == p.selected;
        let detail = if selected { Style::new() } else { t.dim };
        let line = Line::from(vec![
            Span::raw(item.label.clone()),
            Span::raw("  "),
            Span::styled(
                truncate(
                    &item.detail,
                    (inner.width as usize).saturating_sub(sw(&item.label) + 2),
                ),
                detail,
            ),
        ]);
        let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
        f.render_widget(
            Paragraph::new(line).style(if selected { t.selected } else { Style::new() }),
            row,
        );
    }
}

// ---- emoji picker ----

/// Columns one emoji cell takes: the emoji (2) and a gap. A fixed stride keeps the grid
/// aligned however the terminal draws one glyph.
pub(crate) const PICKER_CELL: usize = 3;
const PICKER_COLS: usize = 10;
const PICKER_MAX_ROWS: usize = 8;
const PICKER_TOO_SMALL: &str = "Terminal too small for the emoji picker, press Esc to close";

/// Where the parts of the picker are. Shared by drawing, the keys (scroll maths) and the mouse,
/// so what is drawn is what is clicked.
pub(crate) struct PickerGeom {
    pub outer: Rect,
    pub cats: Rect,
    pub search: Rect,
    pub grid: Rect,
    pub status: Rect,
    pub cols: usize,
    pub rows: usize,
}

/// The picker is centred in the body: a border, a category row, a search row, up to
/// `PICKER_MAX_ROWS` rows of up to `PICKER_COLS` cells, and a status row. None when not even
/// 3 columns and 1 row fit.
pub(crate) fn picker_geometry(body: Rect) -> Option<PickerGeom> {
    const FIXED_ROWS: u16 = 5;
    if (body.width as usize) < 2 + 3 * PICKER_CELL || body.height < FIXED_ROWS + 1 {
        return None;
    }
    let w = body.width.min((2 + PICKER_COLS * PICKER_CELL) as u16);
    let cols = (w as usize - 2) / PICKER_CELL;
    let rows = ((body.height - FIXED_ROWS) as usize).min(PICKER_MAX_ROWS);
    let h = rows as u16 + FIXED_ROWS;
    let outer = Rect::new(
        body.x + (body.width - w) / 2,
        body.y + (body.height - h) / 2,
        w,
        h,
    );
    let inner = outer.inner(Margin::new(1, 1));
    Some(PickerGeom {
        outer,
        cats: Rect::new(inner.x, inner.y, inner.width, 1),
        search: Rect::new(inner.x, inner.y + 1, inner.width, 1),
        grid: Rect::new(inner.x, inner.y + 2, inner.width, rows as u16),
        status: Rect::new(inner.x, inner.y + 2 + rows as u16, inner.width, 1),
        cols,
        rows,
    })
}

/// The category row: every name when they fit, else the current one between arrows.
fn category_spans(cat: usize, searching: bool, width: usize, t: &Theme) -> Vec<Span<'static>> {
    if searching {
        return vec![Span::styled("Search results", t.hint)];
    }
    let all: usize = CATEGORIES.iter().map(|c| sw(c) + 1).sum::<usize>() - 1;
    if all <= width {
        let mut spans = Vec::new();
        for (i, name) in CATEGORIES.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            let style = if i == cat { t.selected } else { Style::new() };
            spans.push(Span::styled(*name, style));
        }
        return spans;
    }
    vec![Span::styled(
        truncate(
            &format!("< {} {}/{} >", CATEGORIES[cat], cat + 1, CATEGORIES.len()),
            width,
        ),
        bold(),
    )]
}

fn draw_picker(f: &mut Frame, app: &App, body: Rect) {
    let t = app.theme();
    let Some(p) = app.picker() else { return };
    let Some(g) = picker_geometry(body) else {
        f.render_widget(Clear, body);
        centered_note(f, body, PICKER_TOO_SMALL, t);
        return;
    };
    let items = p.items(app.recents());
    // Only the picker's own box is cleared, one column wider on each side so no half of a wide
    // character underneath shows at its edges: the conversation stays visible around it, and a
    // reaction still shows which message it goes to.
    let left = g.outer.x.saturating_sub(1).max(body.x);
    let right = (g.outer.x + g.outer.width + 1).min(body.x + body.width);
    f.render_widget(
        Clear,
        Rect::new(left, g.outer.y, right - left, g.outer.height),
    );
    let title = match p.purpose {
        PickerFor::Compose => "Emoji",
        PickerFor::React { .. } => "React",
    };
    f.render_widget(pane(app.theme(), Panel::Popup, title, true), g.outer);
    f.render_widget(
        Paragraph::new(Line::from(category_spans(
            p.cat,
            !p.query.trim().is_empty(),
            g.cats.width as usize,
            t,
        ))),
        g.cats,
    );
    let query_line = if p.query.is_empty() {
        Line::from(vec![
            Span::styled("/", bold()),
            Span::styled("type to search", t.hint),
        ])
    } else {
        Line::from(vec![Span::styled("/", bold()), Span::raw(p.query.clone())])
    };
    f.render_widget(Paragraph::new(query_line), g.search);
    if items.is_empty() {
        let note = if p.query.trim().is_empty() {
            "No recent emoji yet"
        } else {
            "No emoji match"
        };
        centered_note(f, g.grid, note, t);
    }
    let buf = f.buffer_mut();
    for (cell, e) in items
        .iter()
        .enumerate()
        .skip(p.top * g.cols)
        .take(g.rows * g.cols)
    {
        let (row, col) = (cell / g.cols - p.top, cell % g.cols);
        let x = g.grid.x + (col * PICKER_CELL) as u16;
        let y = g.grid.y + row as u16;
        let style = if cell == p.sel {
            t.selected
        } else {
            Style::new()
        };
        // Highlight the two columns an emoji takes, then draw it: one write per cell, so a
        // glyph a terminal draws narrower cannot shift the rest of the row.
        buf.set_style(Rect::new(x, y, 2, 1), style);
        buf.set_stringn(x, y, e.as_str(), 2, style);
    }
    let status = items.get(p.sel).map_or_else(
        || "Esc close".to_string(),
        |e| match e.shortcode() {
            Some(sc) => format!("{} :{}:", e.name(), sc),
            None => e.name().to_string(),
        },
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate(&status, g.status.width as usize),
            t.hint,
        )),
        g.status,
    );
    let x = g.search.x + (1 + sw(&p.query)).min(g.search.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, g.search.y));
}

// ---- sign-in panel and QR ----

/// QR modules (true = dark) including the quiet zone, and the side length.
pub(crate) fn qr_modules(code: &str) -> Option<(Vec<bool>, usize)> {
    let qr = QrCode::with_error_correction_level(code.as_bytes(), EcLevel::L).ok()?;
    let n = qr.width();
    let side = n + 2 * QUIET;
    let mut grid = vec![false; side * side];
    for (i, c) in qr.to_colors().into_iter().enumerate() {
        grid[(i / n + QUIET) * side + i % n + QUIET] = c == QrColor::Dark;
    }
    Some((grid, side))
}

/// Two module rows per text row with half blocks. Fixed black on white so any theme scans.
fn qr_lines(grid: &[bool], side: usize) -> Vec<Line<'static>> {
    let style = Style::new().fg(Color::Black).bg(Color::White);
    (0..side)
        .step_by(2)
        .map(|r| {
            let row: String = (0..side)
                .map(|c| {
                    let top = grid[r * side + c];
                    let bottom = r + 1 < side && grid[(r + 1) * side + c];
                    match (top, bottom) {
                        (true, true) => '\u{2588}',
                        (true, false) => '\u{2580}',
                        (false, true) => '\u{2584}',
                        (false, false) => ' ',
                    }
                })
                .collect();
            Line::from(Span::styled(row, style))
        })
        .collect()
}

fn draw_qr(f: &mut Frame, net: Network, code: &str, area: Rect) {
    let heading = Line::from(Span::styled(
        format!("{}: SCAN QR", net.label().to_uppercase()),
        bold(),
    ));
    let help: Vec<Line<'static>> = wrap(QR_HELP, area.width as usize)
        .into_iter()
        .map(Line::from)
        .collect();
    let mut lines = vec![heading];
    match qr_modules(code) {
        None => lines.push(Line::from(
            "The QR code could not be drawn. Restart chatcli to get a new one.",
        )),
        Some((grid, side)) => {
            let qr = qr_lines(&grid, side);
            // Body rows needed: heading + QR rows + help lines; the terminal adds header and footer.
            let need_h = 1 + qr.len() + help.len();
            if (area.width as usize) < side || (area.height as usize) < need_h {
                // The help text wraps to the width the user will have after enlarging.
                let want_w = side.max(MIN_W as usize);
                let want_body = 1 + qr.len() + wrap(QR_HELP, want_w).len();
                lines.push(Line::from(format!(
                    "Make the window larger to show the QR code (needs {want_w} x {})",
                    height_for_body(want_body)
                )));
            } else {
                lines.extend(qr);
                lines.extend(help);
            }
        }
    }
    let top = (area.height as usize).saturating_sub(lines.len()) / 2;
    let r = Rect::new(
        area.x,
        area.y + top as u16,
        area.width,
        area.height - top as u16,
    );
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        r,
    );
}

fn draw_panel(f: &mut Frame, app: &App, area: Rect) {
    let (ns, t) = (app.active(), app.theme());
    let net = ns.network;
    let name = net.label();
    if let Status::Qr { code } = &ns.status {
        return draw_qr(f, net, code, area);
    }
    let (heading, action) = match (&ns.status, net) {
        (Status::LoggingIn, Network::Teams) => (
            "SIGNING IN".to_string(),
            if app.demo() {
                "Signing in (demo: no browser)"
            } else {
                "Finish signing in in the browser window that opened."
            },
        ),
        (Status::LoggingIn, Network::WhatsApp) => (
            "LINKING".to_string(),
            "Waiting for the QR code from WhatsApp...",
        ),
        (_, Network::Teams) => (
            format!("{} SIGN-IN NEEDED", name.to_uppercase()),
            if app.demo() {
                "Enter to sign in (demo: no browser)"
            } else {
                "Enter to sign in (opens browser)"
            },
        ),
        (_, Network::WhatsApp) => (
            format!("{} SIGN-IN NEEDED", name.to_uppercase()),
            "Enter to link",
        ),
    };
    let w = area.width.min(64);
    let inner_w = (w as usize).saturating_sub(4).max(1);
    let mut lines = vec![Line::from(Span::styled(heading, bold()))];
    if let Status::NeedsLogin { reason: Some(r) } = &ns.status {
        lines.extend(
            wrap(r, inner_w)
                .into_iter()
                .take(3)
                .map(|l| Line::from(Span::styled(l, t.error))),
        );
    }
    lines.push(Line::from(""));
    lines.extend(wrap(action, inner_w).into_iter().map(Line::from));
    lines.push(Line::from(Span::styled("Tab switches network", t.hint)));
    lines.push(Line::from(Span::styled(
        crate::commands::COMMANDS_HINT,
        t.hint,
    )));
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(pane(app.theme(), Panel::Popup, name, true)),
        r,
    );
}

// ---- entry point ----

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        f.render_widget(
            Paragraph::new(format!(
                "Terminal too small: needs at least {MIN_W} x {MIN_H}"
            ))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    draw_tour_bar(f, app, area);
    let lay = app.layout_in(area);
    draw_header(f, app, lay.header);
    if app.settings_page().is_some() {
        crate::settings_ui::draw(f, app, lay.body);
    } else if app.active().panel() {
        draw_panel(f, app, lay.body);
    } else {
        draw_sidebar(f, app, &lay);
        draw_convo(f, app, &lay);
        draw_composer(f, app, &lay);
        draw_popup(f, app, &lay);
        draw_attach(f, app, &lay);
        draw_picker(f, app, lay.body);
    }
    draw_search(f, app, lay.body);
    draw_palette(f, app, lay.body);
    draw_footer(f, app, lay.footer, area.height);
    draw_help(f, app, area, lay.body);
}

// ---- tour caption ----

/// Row 0 while the tour plays: `TOUR 3/26 - caption`, and the keys right-aligned when they fit.
/// Bold accent text only: no fill, no edge bar.
fn draw_tour_bar(f: &mut Frame, app: &App, area: Rect) {
    let Some(bar) = app.tour_bar() else { return };
    let t = app.theme();
    let row = Rect::new(area.x, area.y, area.width, 1);
    let width = usize::from(area.width);
    let hint_w = bar.hint.chars().count() + 2;
    let text_w = bar.text.chars().count();
    let text = crate::text::truncate(&bar.text, width);
    f.render_widget(
        Paragraph::new(Span::styled(text, t.accent.add_modifier(Modifier::BOLD))),
        row,
    );
    if width >= text_w + hint_w + 2 {
        f.render_widget(
            Paragraph::new(Span::styled(bar.hint, t.hint)).alignment(Alignment::Right),
            row,
        );
    }
}

// ---- command bar ----

const PALETTE_TOO_SMALL: &str = "Terminal too small for commands, press Esc to close";
const PALETTE_MAX_W: u16 = 80;
const PALETTE_MIN_W: u16 = 30;
/// Rows the box needs besides the list: border (2) and the query row.
const PALETTE_FIXED_ROWS: u16 = 3;
/// Width of the `/name` column.
const PALETTE_NAME_W: usize = 16;

/// Where the parts of the command bar are: shared by drawing and the key handling (scroll maths).
pub(crate) struct PaletteGeom {
    pub outer: Rect,
    pub query: Rect,
    pub list: Rect,
    pub rows: usize,
}

/// The box fills the body, at most `PALETTE_MAX_W` wide and centred. None when it cannot show
/// even one row.
pub(crate) fn palette_geometry(body: Rect) -> Option<PaletteGeom> {
    if body.width < PALETTE_MIN_W || body.height < PALETTE_FIXED_ROWS + 1 {
        return None;
    }
    let w = body.width.min(PALETTE_MAX_W);
    let outer = Rect::new(body.x + (body.width - w) / 2, body.y, w, body.height);
    let inner = outer.inner(Margin::new(1, 1));
    let rows = inner.height.saturating_sub(1);
    Some(PaletteGeom {
        outer,
        query: Rect::new(inner.x, inner.y, inner.width, 1),
        list: Rect::new(inner.x, inner.y + 1, inner.width, rows),
        rows: rows as usize,
    })
}

fn draw_palette(f: &mut Frame, app: &App, body: Rect) {
    let Some(p) = app.palette() else { return };
    let t = app.theme();
    f.render_widget(Clear, body);
    let Some(g) = palette_geometry(body) else {
        centered_note(f, body, PALETTE_TOO_SMALL, t);
        return;
    };
    f.render_widget(pane(app.theme(), Panel::Popup, "Commands", true), g.outer);
    let shown = tail(
        &clean_line(&p.query),
        (g.query.width as usize).saturating_sub(2),
    );
    let query_line = if p.query.is_empty() {
        Line::from(vec![
            Span::styled("/", bold()),
            Span::styled("type a command or chat name", t.dim),
        ])
    } else {
        Line::from(vec![Span::styled("/", bold()), Span::raw(shown.clone())])
    };
    f.render_widget(Paragraph::new(query_line), g.query);
    let x = g.query.x + (1 + sw(&shown)).min(g.query.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, g.query.y));
    let items = app.palette_items();
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("No command or chat matches", t.hint)),
            Rect::new(g.list.x, g.list.y, g.list.width, g.list.height.min(1)),
        );
        return;
    }
    let w = g.list.width as usize;
    let ns = app.active();
    for (k, row) in crate::palette::rows(&items)
        .into_iter()
        .skip(p.top)
        .take(g.rows)
        .enumerate()
    {
        let area = Rect::new(g.list.x, g.list.y + k as u16, g.list.width, 1);
        let i = match row {
            crate::palette::PalRow::Heading(h) => {
                f.render_widget(Paragraph::new(Span::styled(h, bold())), area);
                continue;
            }
            crate::palette::PalRow::Item(i) => i,
        };
        let line = match &items[i] {
            crate::palette::PalItem::Cmd(id) => {
                let cmd = crate::commands::COMMANDS.iter().find(|c| c.id == *id);
                let (name, doc) = cmd.map_or(("", ""), |c| (c.name, c.doc));
                let reason = app.cmd_unavailable(*id);
                let off = reason.is_some();
                let name_style = if off { t.dim } else { bold() };
                let doc_style = if off { t.dim } else { t.hint };
                // A dimmed command says why, instead of its doc.
                let doc = reason.unwrap_or_else(|| doc.to_string());
                vec![
                    Span::styled(fit(&format!("/{name}"), PALETTE_NAME_W), name_style),
                    Span::raw(" "),
                    Span::styled(doc, doc_style),
                ]
            }
            crate::palette::PalItem::Chat(id) => {
                let title = ns
                    .chats
                    .iter()
                    .find(|c| &c.id == id)
                    .map_or("", |c| c.title.as_str());
                vec![Span::raw(clean_line(title))]
            }
        };
        let selected = i == p.sel;
        f.render_widget(
            Paragraph::new(clip_spans(line, w)).style(if selected {
                t.selected
            } else {
                Style::new()
            }),
            area,
        );
    }
}

// ---- search ----

const SEARCH_TOO_SMALL: &str = "Terminal too small for search, press Esc to close";
const SEARCH_MAX_W: u16 = 100;
const SEARCH_MIN_W: u16 = 30;
/// Rows the box needs besides the hits: border (2), the query row and the status row.
const SEARCH_FIXED_ROWS: u16 = 4;

/// Where the parts of the search box are: shared by drawing and the key handling (scroll maths).
pub(crate) struct SearchGeom {
    pub outer: Rect,
    pub query: Rect,
    pub list: Rect,
    pub status: Rect,
    pub rows: usize,
}

/// The box fills the body, at most `SEARCH_MAX_W` wide and centred. None when it cannot show
/// even one hit.
pub(crate) fn search_geometry(body: Rect) -> Option<SearchGeom> {
    if body.width < SEARCH_MIN_W || body.height < SEARCH_FIXED_ROWS + 1 {
        return None;
    }
    let w = body.width.min(SEARCH_MAX_W);
    let outer = Rect::new(body.x + (body.width - w) / 2, body.y, w, body.height);
    let inner = outer.inner(Margin::new(1, 1));
    let rows = inner.height.saturating_sub(2);
    Some(SearchGeom {
        outer,
        query: Rect::new(inner.x, inner.y, inner.width, 1),
        list: Rect::new(inner.x, inner.y + 1, inner.width, rows),
        status: Rect::new(inner.x, inner.y + 1 + rows, inner.width, 1),
        rows: rows as usize,
    })
}

/// A snippet part's text as one safe line: control characters become spaces.
fn snippet_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn draw_search(f: &mut Frame, app: &App, body: Rect) {
    let Some(s) = app.search() else { return };
    let t = app.theme();
    f.render_widget(Clear, body);
    let Some(g) = search_geometry(body) else {
        centered_note(f, body, SEARCH_TOO_SMALL, t);
        return;
    };
    f.render_widget(pane(app.theme(), Panel::Popup, "Search", true), g.outer);
    let query_line = if s.query.is_empty() {
        Line::from(vec![
            Span::styled("/", bold()),
            Span::styled("type to search every chat", t.hint),
        ])
    } else {
        Line::from(vec![
            Span::styled("/", bold()),
            Span::raw(tail(
                &clean_line(&s.query),
                (g.query.width as usize).saturating_sub(2),
            )),
        ])
    };
    f.render_widget(Paragraph::new(query_line), g.query);
    let shown = tail(
        &clean_line(&s.query),
        (g.query.width as usize).saturating_sub(2),
    );
    let x = g.query.x + (1 + sw(&shown)).min(g.query.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, g.query.y));
    let look = app.look();
    let w = g.list.width as usize;
    // Narrow: the network is one letter and the time goes, so the matched text keeps the room.
    let compact = w < 60;
    let title_w = (w / 4).clamp(if compact { 6 } else { 8 }, 22);
    for (k, hit) in s.hits.iter().skip(s.top).take(g.rows).enumerate() {
        let selected = s.top + k == s.sel;
        let net_style = match hit.network {
            Network::Teams => t.network_teams,
            Network::WhatsApp => t.network_whatsapp,
        };
        let net_label = if compact {
            hit.network.label().chars().take(1).collect()
        } else {
            fit(hit.network.label(), 8)
        };
        let mut spans = vec![
            Span::styled(net_label, net_style),
            Span::raw(" "),
            Span::styled(fit(&clean_line(&hit.chat_title), title_w), bold()),
            Span::raw(" "),
        ];
        if !compact {
            spans.push(Span::styled(
                fit(
                    &look.clock.list_time_f(hit.ts, look.fmt),
                    look.fmt.time_width().max(8),
                ),
                t.dim,
            ));
            spans.push(Span::raw(" "));
        }
        // The match in bold: the words that hit, in the snippet's own context. On the selected
        // row the whole line is reverse video and bold, so the match is underlined as well.
        let hit_style = if selected {
            t.text.patch(bold()).add_modifier(Modifier::UNDERLINED)
        } else {
            t.text.patch(bold())
        };
        spans.extend(hit.snippet.iter().map(|p| {
            Span::styled(
                snippet_text(&p.text),
                if p.hit { hit_style } else { t.text },
            )
        }));
        let row = Rect::new(g.list.x, g.list.y + k as u16, g.list.width, 1);
        f.render_widget(
            Paragraph::new(clip_spans(spans, w)).style(if selected {
                t.selected
            } else {
                Style::new()
            }),
            row,
        );
    }
    let (status, style) = match (&s.error, s.query.trim().is_empty(), s.hits.len()) {
        (Some(e), _, _) => (e.clone(), t.error),
        (None, true, _) => ("Type to search".to_string(), t.hint),
        (None, false, 0) => ("No messages match".to_string(), t.hint),
        (None, false, n) if n as u32 >= crate::search::LIMIT => (
            format!("{} of {n}+ hits: add words to narrow it", s.sel + 1),
            t.hint,
        ),
        (None, false, n) => (format!("{} of {n} hits", s.sel + 1), t.hint),
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate(&status, g.status.width as usize),
            style,
        )),
        g.status,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_and_avatars() {
        assert_eq!(initials("Alex Example"), "AE");
        assert_eq!(initials("Sam"), "S");
        assert_eq!(initials("Jordan de la Placeholder"), "JP");
        assert_eq!(initials("007 Agent"), "#");
        assert_eq!(initials("田中太郎"), "田");
        assert_eq!(initials("  "), "");
        let (top, bottom) = avatar_cells("Alex Example", Style::new().fg(Color::Green));
        assert_eq!(
            (top.content.as_ref(), bottom.content.as_ref()),
            (" AE ", "    ")
        );
        assert_eq!(top.style.bg, Some(Color::Green));
        let (top, _) = avatar_cells("田中太郎", Style::new());
        assert_eq!(top.content.as_ref(), " 田 ");
        assert!(
            top.style.add_modifier.contains(Modifier::REVERSED),
            "no colour: reverse video"
        );
    }

    #[test]
    fn a_person_keeps_one_colour() {
        let t = Theme::default();
        assert_eq!(t.person("u1"), t.person("u1"));
        let picked: std::collections::HashSet<_> =
            (0..40).map(|i| t.person(&format!("user-{i}")).fg).collect();
        assert!(
            picked.len() >= 6,
            "names spread over the colours: {picked:?}"
        );
        assert!(!picked.contains(&Some(Color::Red)), "red is for errors");
        assert_eq!(t.clone().without_colour().person("u1"), bold());
    }

    #[test]
    fn the_network_goes_after_the_uppercase_key_word() {
        assert_eq!(
            tag_network("BLOCKED: executable", "Teams"),
            "BLOCKED (Teams): executable"
        );
        assert_eq!(
            tag_network("PREVIEW FAILED: not a picture", "WhatsApp"),
            "PREVIEW FAILED (WhatsApp): not a picture"
        );
        // No key word: the network leads, as before.
        assert_eq!(tag_network("Opened.", "Teams"), "Teams: Opened.");
        assert_eq!(
            tag_network("Could not send: try again", "Teams"),
            "Teams: Could not send: try again"
        );
    }

    #[test]
    fn a_question_is_bold_and_the_danger_word_is_red() {
        let t = Theme::default();
        let spans = prompt_spans("Upload EXECUTABLE setup.exe to your OneDrive? y/n", &t);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Upload EXECUTABLE setup.exe to your OneDrive? y/n");
        for s in &spans {
            assert!(
                s.style.add_modifier.contains(Modifier::BOLD),
                "{:?} is bold",
                s.content
            );
            assert!(!s.style.add_modifier.contains(Modifier::DIM));
        }
        let danger = spans.iter().find(|s| s.content.trim() == "EXECUTABLE");
        assert_eq!(danger.map(|s| s.style.fg), Some(t.error.fg));
        let delete = prompt_spans("Delete for everyone? y/n", &t);
        assert_eq!(delete[0].style.fg, t.error.fg);
    }

    #[test]
    fn short_status_words_read_as_states_and_differ() {
        let t = Theme::default();
        let words: Vec<&str> = [
            Status::Connecting,
            Status::NeedsLogin { reason: None },
            Status::LoggingIn,
        ]
        .iter()
        .map(|s| chip(s, true, &t).0)
        .collect();
        assert_eq!(words, ["CONN…", "NO LOGIN", "SIGNING"]);
        // Never a bare command word.
        assert!(words.iter().all(|w| *w != "SIGN-IN" && *w != "CONNECT"));
    }
}
