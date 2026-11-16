//! Drawing. `draw` reads `App` and never changes it. Colours are the ANSI 16 named ones only,
//! so the terminal's own light or dark theme decides how they look; every colour also has a
//! text or shape signal (status words, reverse video, double border on the focused pane).

use chatcli_core::model::{ChatMessage, MessageState, Network, Status};
use qrcode::{Color as QrColor, EcLevel, QrCode};
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Focus, NetState, NARROW};
use crate::text::{fit, sw, tail, truncate, wrap, Clock};

const MIN_W: u16 = 30;
const MIN_H: u16 = 8;
const BRAND: &str = " chatcli ";
const SENDER_MAX: usize = 14;
const QR_HELP: &str = "Linked devices (Android: menu, iPhone: Settings) > Link a device";
/// Modules of white border around the QR code (the QR standard asks for 4).
const QUIET: usize = 4;
/// Longest a status or notice may get in the footer: two rows, then it is cut.
const FOOTER_WRAP_LINES: usize = 2;

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// For text the user must be able to read (key hints, empty-state notes). DarkGray is colour 8
/// and some themes draw it in the background colour; DIM keeps the default colour, so the
/// worst case on a terminal that ignores DIM is plain text.
fn hint() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn red() -> Style {
    Style::new().fg(Color::Red)
}

fn reversed() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
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
    match area.height {
        20.. => 1 + problem_lines(app, area.width as usize).len() as u16,
        14..=19 => 1 + u16::from(status_part(app).is_some() || notice_part(app).is_some()),
        _ => 1,
    }
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

/// Screen regions for a terminal of `area`. Under `NARROW` columns only one pane shows:
/// the conversation when `show_convo`, else the chat list. A hidden pane is an empty rect,
/// so mouse hit tests against it never match.
pub(crate) fn layout(area: Rect, footer_h: u16, show_convo: bool, filter_row: bool) -> Layout {
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
        // 28 keeps about 12 columns of title once the unread count and the date are drawn.
        let w = (area.width * 30 / 100).clamp(28, 40);
        (
            Rect::new(body.x, body.y, w, body.height),
            Rect::new(body.x + w, body.y, area.width - w, body.height),
        )
    };
    let composer_h = main.height.min(3);
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

fn chip(status: &Status, short: bool) -> (&'static str, Style) {
    match status {
        Status::Starting | Status::Connecting => {
            (if short { "CONNECT" } else { "CONNECTING" }, hint())
        }
        Status::NeedsLogin { .. } => (
            if short { "SIGN-IN" } else { "SIGN-IN NEEDED" },
            Style::new().fg(Color::Yellow),
        ),
        Status::LoggingIn => (
            if short { "SIGNING" } else { "SIGNING IN" },
            Style::new().fg(Color::Cyan),
        ),
        Status::Qr { .. } => (
            if short { "QR" } else { "SCAN QR" },
            Style::new().fg(Color::Cyan),
        ),
        Status::Online { live: true, .. } => ("LIVE", Style::new().fg(Color::Green)),
        Status::Online { live: false, .. } => ("NOT LIVE", Style::new().fg(Color::Yellow)),
        Status::Error { .. } => ("ERROR", red()),
    }
}

/// Tab cells with their screen position. Shared by drawing and mouse hit testing.
pub(crate) fn tabs(app: &App, width: u16) -> Vec<Tab> {
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
        let (text, chip_style) = chip(&ns.status, short);
        let base = if active { reversed() } else { Style::new() };
        let spans = vec![
            Span::styled(" ", base),
            Span::styled(label, base.patch(bold())),
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
    if area.height == 0 {
        return;
    }
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
            Paragraph::new(Span::styled(quit, hint())).alignment(Alignment::Right),
            area,
        );
    }
}

// ---- footer ----

/// Why the active network is not healthy: the ERROR message, or why it is NOT LIVE.
fn status_part(app: &App) -> Option<Span<'static>> {
    match &app.active().status {
        Status::Error { message } => Some(Span::styled(
            format!("ERROR: {message}"),
            red().patch(bold()),
        )),
        Status::Online {
            live: false,
            detail: Some(d),
        } => Some(Span::raw(format!("NOT LIVE: {d}"))),
        _ => None,
    }
}

/// The newest fresh notice, red when it is an error.
fn notice_part(app: &App) -> Option<Span<'static>> {
    app.current_notice().map(|n| {
        let style = if n.is_error { red() } else { Style::new() };
        Span::styled(format!("{}: {}", n.network.label(), n.text), style)
    })
}

fn hints(app: &App) -> &'static str {
    let ns = app.active();
    if !ns.panel() && ns.focus == Focus::List && !ns.filter.is_empty() {
        return "Esc clear filter  Tab network  Up/Down chat  Enter open  / filter  q quit";
    }
    if ns.panel() {
        return if matches!(ns.status, Status::NeedsLogin { .. }) {
            "Enter sign in  Tab network  q quit"
        } else {
            "Tab network  q quit"
        };
    }
    match ns.focus {
        Focus::List => "Tab network  Up/Down chat  Enter open  / filter  r refresh  q quit",
        Focus::Filter => "Type to filter  Up/Down chat  Enter keep filter  Esc clear",
        Focus::Composer => "Enter send  Esc back to list  PgUp/PgDn scroll  Tab network",
    }
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
    if area.height == 0 {
        return;
    }
    let w = area.width as usize;
    let hint_line = Line::from(Span::styled(truncate(hints(app), w), hint()));
    let rows: Vec<Line<'static>> = match screen_height {
        20.. => {
            let mut rows = problem_lines(app, w);
            rows.push(hint_line);
            rows
        }
        14..=19 => {
            let (status, notice) = (status_part(app), notice_part(app));
            let mut both: Vec<Span<'static>> = status.into_iter().collect();
            if !both.is_empty() && notice.is_some() {
                both.push(Span::styled("  |  ", hint()));
            }
            both.extend(notice);
            if both.is_empty() {
                vec![hint_line]
            } else {
                vec![clip_spans(both, w), hint_line]
            }
        }
        // One row: a problem outranks the key hints, which return once it is gone.
        _ => match status_part(app).or(notice_part(app)) {
            Some(s) => vec![clip_spans(vec![s], w)],
            None => vec![hint_line],
        },
    };
    f.render_widget(Paragraph::new(rows), area);
}

// ---- panes ----

fn pane(title: &str, focused: bool) -> Block<'static> {
    let b = Block::bordered().title(Span::styled(
        format!(" {title} "),
        if focused { bold() } else { Style::new() },
    ));
    if focused {
        b.border_type(BorderType::Double)
    } else {
        b
    }
}

fn centered_note(f: &mut Frame, area: Rect, text: &str) {
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
            .style(hint())
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        r,
    );
}

fn draw_sidebar(f: &mut Frame, app: &App, lay: &Layout) {
    let ns = app.active();
    let focused = matches!(ns.focus, Focus::List | Focus::Filter);
    f.render_widget(pane("Chats", focused), lay.sidebar);
    if let Some(r) = lay.filter_row {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/", bold()),
                Span::raw(ns.filter.clone()),
            ])),
            r,
        );
        if ns.focus == Focus::Filter {
            let x = r.x + (1 + sw(&ns.filter)).min(r.width.saturating_sub(1) as usize) as u16;
            f.set_cursor_position((x, r.y));
        }
    }
    let list = lay.list;
    let vis = ns.visible();
    if vis.is_empty() {
        let note = if !ns.chats.is_empty() {
            format!("No chats match \"{}\"", ns.filter)
        } else {
            match ns.status {
                Status::Starting | Status::Connecting => "Loading chats...".to_string(),
                Status::Error { .. } => "No chats loaded. See the line below.".to_string(),
                _ => "No chats yet".to_string(),
            }
        };
        centered_note(f, list, &note);
        return;
    }
    let w = list.width as usize;
    for (k, &ci) in vis
        .iter()
        .skip(ns.list_top)
        .take(list.height as usize / 2)
        .enumerate()
    {
        let c = &ns.chats[ci];
        let selected = ns.selected.as_ref() == Some(&c.id);
        let (num_style, title_style, sub_style) = if selected {
            (Style::new(), Style::new(), Style::new())
        } else {
            (
                bold(),
                if c.unread > 0 { bold() } else { Style::new() },
                dim(),
            )
        };
        let num = match c.unread {
            0 => String::new(),
            1..=99 => c.unread.to_string(),
            _ => "99+".to_string(),
        };
        let time = c
            .last_ts
            .map(|t| app.clock().list_time(t))
            .unwrap_or_default();
        let time_w = if time.is_empty() { 0 } else { sw(&time) + 1 };
        let title = fit(&c.title, w.saturating_sub(4 + time_w));
        let line1 = Line::from(vec![
            Span::styled(format!("{num:>3} "), num_style),
            Span::styled(title, title_style),
            Span::styled(
                if time.is_empty() {
                    String::new()
                } else {
                    format!(" {time}")
                },
                sub_style,
            ),
        ]);
        let line2 = Line::from(Span::styled(
            format!("    {}", truncate(&c.last_preview, w.saturating_sub(4))),
            sub_style,
        ));
        let r = Rect::new(list.x, list.y + 2 * k as u16, list.width, 2).intersection(list);
        let base = if selected { reversed() } else { Style::new() };
        f.render_widget(Paragraph::new(vec![line1, line2]).style(base), r);
    }
}

/// Every line of the open conversation, wrapped to `width`, oldest first.
pub(crate) fn message_lines(ns: &NetState, clock: &Clock, width: usize) -> Vec<Line<'static>> {
    let Some(msgs) = ns.open.as_ref().and_then(|id| ns.messages.get(id)) else {
        return Vec::new();
    };
    let label = |m: &ChatMessage| {
        if m.from_me {
            "You".to_string()
        } else {
            m.sender.clone()
        }
    };
    let sender_w = msgs
        .iter()
        .map(|m| sw(&label(m)))
        .max()
        .unwrap_or(0)
        .min(SENDER_MAX)
        .min(width.saturating_sub(15));
    let indent_w = 5 + 1 + sender_w + 2;
    let text_w = width.saturating_sub(indent_w).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut last_day = None;
    for m in msgs {
        let day = clock.day(m.ts);
        if day != last_day || lines.is_empty() {
            last_day = day;
            let sep = format!("---- {} ----", clock.day_label(m.ts));
            lines.push(Line::from(Span::styled(sep, dim())).alignment(Alignment::Center));
        }
        let (body, body_style) = match &m.state {
            MessageState::Deleted => ("[deleted]".to_string(), dim()),
            MessageState::Failed(_) => (m.text.clone(), red()),
            _ => (m.text.clone(), Style::new()),
        };
        let who_style = if m.from_me {
            Style::new().fg(Color::Cyan).patch(bold())
        } else {
            bold()
        };
        let mut rows: Vec<Vec<Span<'static>>> = wrap(&body, text_w)
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let lead = if i == 0 {
                    vec![
                        Span::styled(clock.hm(m.ts), dim()),
                        Span::raw(" "),
                        Span::styled(fit(&label(m), sender_w), who_style),
                        Span::raw("  "),
                    ]
                } else {
                    vec![Span::raw(" ".repeat(indent_w))]
                };
                lead.into_iter()
                    .chain([Span::styled(text, body_style)])
                    .collect()
            })
            .collect();
        match &m.state {
            MessageState::Pending => {
                let tag = " [sending]";
                let fits = rows
                    .last()
                    .is_some_and(|r| r.iter().map(Span::width).sum::<usize>() + sw(tag) <= width);
                if fits {
                    rows.last_mut().unwrap().push(Span::styled(tag, dim()));
                } else {
                    rows.push(vec![
                        Span::raw(" ".repeat(indent_w)),
                        Span::styled(tag.trim_start(), dim()),
                    ]);
                }
            }
            MessageState::Failed(reason) => {
                for row in wrap(&format!("NOT SENT: {reason}"), text_w) {
                    rows.push(vec![
                        Span::raw(" ".repeat(indent_w)),
                        Span::styled(row, red().patch(bold())),
                    ]);
                }
            }
            _ => {}
        }
        lines.extend(rows.into_iter().map(Line::from));
    }
    lines
}

fn draw_convo(f: &mut Frame, app: &App, lay: &Layout) {
    let ns = app.active();
    let Some(open) = ns.open.as_ref() else {
        f.render_widget(pane("Conversation", false), lay.convo);
        centered_note(f, lay.messages, "Select a chat and press Enter to open it.");
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
    f.render_widget(pane(&title, false), lay.convo);
    if !ns.messages.contains_key(open) {
        centered_note(f, lay.messages, "Loading messages...");
        return;
    }
    let lines = message_lines(ns, app.clock(), lay.messages.width as usize);
    if lines.is_empty() {
        centered_note(f, lay.messages, "No messages in this chat yet.");
        return;
    }
    let height = lay.messages.height as usize;
    let end = lines.len() - ns.scroll.min(lines.len().saturating_sub(height));
    let start = end.saturating_sub(height);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), lay.messages);
}

fn draw_composer(f: &mut Frame, app: &App, lay: &Layout) {
    let ns = app.active();
    let focused = ns.focus == Focus::Composer;
    f.render_widget(pane("Message", focused), lay.composer);
    let inner = lay.composer.inner(Margin::new(1, 1));
    if inner.height == 0 {
        return;
    }
    if ns.open.is_none() {
        f.render_widget(
            Paragraph::new(Span::styled("Open a chat to type", hint())),
            inner,
        );
        return;
    }
    let draft = ns.draft();
    if draft.is_empty() && !focused {
        // Focus is on the list: Enter there opens the selected chat and letters are commands,
        // so the placeholder must not invite typing.
        f.render_widget(
            Paragraph::new(Span::styled("Enter on a chat to type", hint())),
            inner,
        );
        return;
    }
    let shown = tail(draft, (inner.width as usize).saturating_sub(3));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", bold()),
            Span::raw(shown.clone()),
        ])),
        inner,
    );
    if focused {
        f.set_cursor_position((inner.x + 2 + sw(&shown) as u16, inner.y));
    }
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

fn draw_panel(f: &mut Frame, ns: &NetState, area: Rect) {
    let net = ns.network;
    let name = net.label();
    if let Status::Qr { code } = &ns.status {
        return draw_qr(f, net, code, area);
    }
    let (heading, action) = match (&ns.status, net) {
        (Status::LoggingIn, Network::Teams) => (
            "SIGNING IN".to_string(),
            "Finish signing in in the browser window that opened.",
        ),
        (Status::LoggingIn, Network::WhatsApp) => (
            "LINKING".to_string(),
            "Waiting for the QR code from WhatsApp...",
        ),
        (_, Network::Teams) => (
            format!("{} SIGN-IN NEEDED", name.to_uppercase()),
            "Enter to sign in (opens browser)",
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
                .map(|l| Line::from(Span::styled(l, red()))),
        );
    }
    lines.push(Line::from(""));
    lines.extend(wrap(action, inner_w).into_iter().map(Line::from));
    lines.push(Line::from(Span::styled("Tab switches network", hint())));
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
            .block(pane(name, true)),
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
    let lay = app.layout_in(area);
    draw_header(f, app, lay.header);
    if app.active().panel() {
        draw_panel(f, app.active(), lay.body);
    } else {
        draw_sidebar(f, app, &lay);
        draw_convo(f, app, &lay);
        draw_composer(f, app, &lay);
    }
    draw_footer(f, app, lay.footer, area.height);
}
