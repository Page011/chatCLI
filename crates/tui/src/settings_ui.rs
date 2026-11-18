//! Drawing the settings screen. The model is `settings_view`, the keys are `app/settings.rs`.

use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, NARROW};
use crate::keymap::{Action, Context};
use crate::list::keep_visible;
use crate::settings_view::{Kind, PageEdit, Row, RowId, Section, SettingsPage};
use crate::text::{fit, sw, truncate, wrap};
use crate::theme::{Panel, Theme};
use crate::ui::{bold, pane};

const SECTIONS_W: u16 = 18;
const LABEL_W: usize = 19;
const VALUE_W: usize = 14;
/// The Keys page lists several keys per action, so its value column is wider.
const KEYS_VALUE_W: usize = 30;
/// The hint column shows only when the longest hint of the page fits whole; otherwise the status
/// line under the list shows the chosen row's hint.
/// Rows the live preview needs: four sample lines, the first one named.
const PREVIEW_ROWS: usize = 4;

/// Where the parts of the screen are: shared by drawing and the key handling (scroll maths).
pub(crate) struct Geom {
    /// The section list pane. None under `NARROW` columns: one pane shows at a time.
    pub left: Option<Rect>,
    /// The pane that holds the rows (the whole body when narrow).
    pub right: Rect,
    /// Rows (or, when narrow and not in a section, the section names).
    pub list: Rect,
    /// One line under the list: the chosen row's hint, or the red error.
    pub status: Rect,
}

pub(crate) fn geometry(body: Rect) -> Option<Geom> {
    let (left, right) = if body.width < NARROW {
        (None, body)
    } else {
        (
            Some(Rect::new(body.x, body.y, SECTIONS_W, body.height)),
            Rect::new(
                body.x + SECTIONS_W,
                body.y,
                body.width - SECTIONS_W,
                body.height,
            ),
        )
    };
    let inner = right.inner(Margin::new(1, 1));
    if inner.height < 2 || inner.width < 10 {
        return None;
    }
    Some(Geom {
        left,
        right,
        list: Rect::new(inner.x, inner.y, inner.width, inner.height - 1),
        status: Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    })
}

/// The section list as drawn: the left pane, or the whole list while narrow and not in a section.
fn sections_area(g: &Geom, page: &SettingsPage) -> Option<Rect> {
    match g.left {
        Some(left) => Some(left.inner(Margin::new(1, 1))),
        None if !page.in_rows => Some(g.list),
        None => None,
    }
}

/// The section drawn at `pos`, if any.
pub(crate) fn section_at(page: &SettingsPage, g: &Geom, pos: Position) -> Option<Section> {
    let area = sections_area(g, page).filter(|a| a.contains(pos))?;
    let idx = Section::ALL.iter().position(|s| *s == page.section)?;
    let top = keep_visible(0, idx, area.height as usize, Section::ALL.len());
    Section::ALL.get(top + (pos.y - area.y) as usize).copied()
}

/// The index of the row drawn at `pos`, if any (a heading or a wrapped problem line counts: the
/// caller checks whether it can be chosen).
pub(crate) fn row_at(page: &SettingsPage, rows: &[Row], g: &Geom, pos: Position) -> Option<usize> {
    if (g.left.is_none() && !page.in_rows) || !g.list.contains(pos) {
        return None;
    }
    let (w, h) = (g.list.width as usize, g.list.height as usize);
    let top = keep_visible(page.top, page.row, h, rows.len());
    let line = (pos.y - g.list.y) as usize;
    row_layout(rows, top, w, h)
        .into_iter()
        .find(|(_, at, n)| (*at..at + n).contains(&line))
        .map(|(k, ..)| k)
}

/// Rows of the list that fit on screen.
pub(crate) fn rows_shown(body: Rect) -> usize {
    geometry(body).map_or(1, |g| g.list.height as usize)
}

pub(crate) fn draw(f: &mut Frame, app: &App, body: Rect) {
    let Some(page) = app.settings_page() else {
        return;
    };
    let t = app.theme();
    let Some(g) = geometry(body) else {
        return crate::overlay::too_small(f, t, body, "settings");
    };
    f.render_widget(Clear, body);
    let rows = app.page_rows();
    match g.left {
        Some(left) => {
            f.render_widget(pane(t, Panel::Sidebar, "Settings", !page.in_rows), left);
            draw_sections(f, page, left.inner(Margin::new(1, 1)), t, !page.in_rows);
            f.render_widget(
                pane(
                    t,
                    Panel::Messages,
                    &pane_title(page, &rows, &g),
                    page.in_rows,
                ),
                g.right,
            );
            draw_rows(f, app, page, &rows, &g);
        }
        None if page.in_rows => {
            f.render_widget(
                pane(t, Panel::Messages, &pane_title(page, &rows, &g), true),
                g.right,
            );
            draw_rows(f, app, page, &rows, &g);
        }
        None => {
            f.render_widget(pane(t, Panel::Sidebar, "Settings", true), g.right);
            draw_sections(f, page, g.list, t, true);
            draw_status(f, page, &rows, &g, t);
        }
    }
}

/// The pane title. On the Keys page it names the context of the first row on screen, so the
/// rows always say which context they edit (the headings scroll away with the rows).
fn pane_title(page: &SettingsPage, rows: &[Row], g: &Geom) -> String {
    if page.section != Section::Keys {
        return page.section.title().to_string();
    }
    let top = keep_visible(page.top, page.row, g.list.height as usize, rows.len());
    match rows[..(top + 1).min(rows.len())]
        .iter()
        .rev()
        .find(|r| r.kind == Kind::Heading)
    {
        Some(h) => format!("Keys - {}", h.label),
        None => page.section.title().to_string(),
    }
}

fn draw_sections(f: &mut Frame, page: &SettingsPage, area: Rect, t: &Theme, focused: bool) {
    let h = area.height as usize;
    let idx = Section::ALL
        .iter()
        .position(|s| *s == page.section)
        .unwrap_or(0);
    let top = keep_visible(0, idx, h, Section::ALL.len());
    for (k, s) in Section::ALL.iter().enumerate().skip(top).take(h) {
        let style = match (*s == page.section, focused) {
            (true, true) => t.selected,
            (true, false) => bold(),
            _ => Style::new(),
        };
        f.render_widget(
            Paragraph::new(fit(s.title(), area.width as usize)).style(style),
            Rect::new(area.x, area.y + (k - top) as u16, area.width, 1),
        );
    }
}

/// Where each row from `top` sits in a list `w` wide and `h` lines tall: (row index, first
/// line, lines). A problem row wraps over several lines; the rest take one. Drawing and the
/// mouse both read this.
pub(crate) fn row_layout(
    rows: &[Row],
    top: usize,
    w: usize,
    h: usize,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for (k, row) in rows.iter().enumerate().skip(top) {
        if used >= h {
            break;
        }
        let lines = if matches!(row.id, RowId::Problem(_)) {
            wrap(&row.value, w).len().min(h - used).max(1)
        } else {
            1
        };
        out.push((k, used, lines));
        used += lines;
    }
    out
}

fn draw_rows(f: &mut Frame, app: &App, page: &SettingsPage, rows: &[Row], g: &Geom) {
    let t = app.theme();
    let area = g.list;
    let w = area.width as usize;
    let h = area.height as usize;
    let label_w = LABEL_W.min(w / 2);
    let after_label = w.saturating_sub(label_w + 1);
    let want_w = if page.section == Section::Keys {
        KEYS_VALUE_W
    } else {
        VALUE_W
    };
    let longest_hint = rows
        .iter()
        .filter(|r| r.selectable())
        .map(|r| sw(r.hint))
        .max()
        .unwrap_or(0);
    let show_hint = g.left.is_some() && after_label >= want_w + 1 + longest_hint;
    let value_w = if show_hint { want_w } else { after_label };
    let top = keep_visible(page.top, page.row, h, rows.len());
    // A problem line wraps over several screen rows: `used` counts the rows drawn.
    for (k, used, lines) in row_layout(rows, top, w, h) {
        let row = &rows[k];
        let y = area.y + used as u16;
        let selected = page.in_rows && k == page.row;
        let line_area = Rect::new(area.x, y, area.width, 1);
        let base = if selected { t.selected } else { Style::new() };
        if row.kind == Kind::Heading {
            f.render_widget(
                Paragraph::new(Span::styled(truncate(&row.label, w), bold())),
                line_area,
            );
            continue;
        }
        if matches!(row.id, RowId::Problem(_)) {
            for (i, text) in wrap(&row.value, w).iter().take(lines).enumerate() {
                f.render_widget(
                    Paragraph::new(Span::styled(text.clone(), t.error)),
                    Rect::new(area.x, y + i as u16, area.width, 1),
                );
            }
            continue;
        }
        if row.kind == Kind::Info {
            let style = if row.is_error() { t.error } else { t.hint };
            let spans = if row.label.is_empty() {
                vec![Span::styled(truncate(&row.value, w), style)]
            } else {
                vec![
                    Span::styled(fit(&row.label, label_w), bold()),
                    Span::raw(" "),
                    Span::raw(truncate(&row.value, after_label)),
                ]
            };
            f.render_widget(Paragraph::new(Line::from(spans)).style(base), line_area);
            continue;
        }
        // Choosing a key to delete: only the chosen key carries the selection colour, so the row
        // drops its own.
        if let (Some(PageEdit::Pick { ctx, action, index }), true) = (&page.edit, selected) {
            let mut spans = vec![
                Span::styled(fit(&row.label, label_w), bold()),
                Span::raw(" "),
            ];
            spans.extend(pick_spans(
                app, *ctx, *action, *index, row.custom, value_w, t,
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), line_area);
            continue;
        }
        let editing = match (&page.edit, selected) {
            (Some(PageEdit::Text(buf)), true) => Some(buf.view(value_w)),
            _ => None,
        };
        // A key being captured: the row itself says it is listening.
        let listening = match (&page.edit, selected) {
            (Some(PageEdit::Capture { .. }), true) => Some("press a key".to_string()),
            (Some(PageEdit::Conflict { key, .. }), true) => {
                Some(format!("{}? Enter swaps", key.display()))
            }
            (Some(PageEdit::Add { .. }), true) => Some("press a key to add".to_string()),
            (Some(PageEdit::AddOverlap { key, .. }), true) => {
                Some(format!("{}? Enter adds", key.display()))
            }
            _ => None,
        };
        let value = match (&editing, &listening) {
            (Some((shown, _)), _) => fit(shown, value_w),
            (None, Some(text)) => fit(text, value_w),
            _ => fit(&row.value, value_w),
        };
        let mut value_style = if row.custom || listening.is_some() {
            bold()
        } else {
            Style::new()
        };
        // The `selected` role is not shown in its own style: reversed text would read as a
        // second cursor.
        if let RowId::Role(name) = row.id {
            if name.ends_with("_background") {
                // A swatch: the value drawn on the colour it names.
                if let (false, Ok(Some(c))) =
                    (app.no_colour(), crate::theme::background(&row.value))
                {
                    value_style = value_style.bg(c);
                }
            } else if name != "selected" {
                value_style = value_style.patch(t.role(name).unwrap_or_default());
            }
        }
        let mut spans = vec![
            Span::raw(fit(&row.label, label_w)),
            Span::raw(" "),
            Span::styled(value, value_style),
        ];
        if show_hint {
            let hint_w = after_label - value_w - 1;
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fit(row.hint, hint_w), t.hint));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(base), line_area);
        if let Some((_, col)) = &editing {
            let x = area.x + (label_w + 1 + col) as u16;
            f.set_cursor_position((x.min(area.x + area.width - 1), y));
        }
    }
    if page.section == Section::Appearance {
        let shown_rows = rows.len().saturating_sub(top);
        let free = h.saturating_sub(shown_rows);
        if top == 0 && free >= PREVIEW_ROWS {
            let gap = usize::from(free > PREVIEW_ROWS);
            let y = area.y + (shown_rows + gap) as u16;
            draw_preview(f, Rect::new(area.x, y, area.width, PREVIEW_ROWS as u16), t);
        }
    }
    draw_status(f, page, rows, g, t);
}

/// The value of a key row while one of its keys is chosen for deleting: the keys as on the row,
/// the chosen one in the selection colour, cut to `w` columns. Sequences show but cannot be
/// chosen.
fn pick_spans(
    app: &App,
    ctx: Context,
    action: Action,
    index: usize,
    custom: bool,
    w: usize,
    t: &Theme,
) -> Vec<Span<'static>> {
    let keys = app.keys();
    let plain = if custom { bold() } else { Style::new() };
    let mut parts: Vec<(String, Style)> = Vec::new();
    let mut chosen = None;
    for (i, k) in keys.keys_in(ctx, action).iter().enumerate() {
        if i > 0 {
            parts.push((", ".to_string(), plain));
        }
        let style = if i == index { t.selected } else { plain };
        if i == index {
            chosen = Some(parts.len());
        }
        parts.push((k.display(), style));
    }
    for s in keys.sequences_in(ctx, action) {
        parts.push((", ".to_string(), plain));
        parts.push((s, plain));
    }
    if custom {
        parts.push((" *".to_string(), plain));
    }
    // Keys before the chosen one give way (a leading `…`) so the chosen key is always on screen.
    if let Some(c) = chosen {
        let width = |ps: &[(String, Style)]| ps.iter().map(|p| sw(&p.0)).sum::<usize>();
        let mut start = 0;
        while start < c && width(&parts[start..=c]) + usize::from(start > 0) > w {
            start += 1;
        }
        if start > 0 {
            parts.drain(..start);
            if parts.first().is_some_and(|p| p.0 == ", ") {
                parts.remove(0);
            }
            parts.insert(0, ("…".to_string(), plain));
        }
    }
    let mut out = Vec::new();
    let mut left = w;
    for (text, style) in parts {
        if left == 0 {
            break;
        }
        let shown = truncate(&text, left);
        left -= sw(&shown);
        out.push(Span::styled(shown, style));
    }
    if left > 0 {
        out.push(Span::raw(" ".repeat(left)));
    }
    out
}

/// The line under the list: the red error, else the chosen row's hint.
fn draw_status(f: &mut Frame, page: &SettingsPage, rows: &[Row], g: &Geom, t: &Theme) {
    let w = g.status.width as usize;
    let line = match (&page.error, rows.get(page.row)) {
        (Some(e), _) => Span::styled(truncate(e, w), t.error),
        (None, Some(row)) if page.in_rows => Span::styled(truncate(row.hint, w), t.hint),
        _ => return,
    };
    f.render_widget(Paragraph::new(line), g.status);
}

/// Four sample lines in the live theme, so a colour change is visible without leaving the page.
fn draw_preview(f: &mut Frame, area: Rect, t: &Theme) {
    let w = area.width as usize;
    let lines = [
        Line::from(vec![
            Span::styled("Preview ", bold()),
            Span::styled("Teams", t.network_teams),
            Span::raw(" "),
            Span::styled("LIVE", t.chip_live),
            Span::raw("  "),
            Span::styled("WhatsApp", t.network_whatsapp),
            Span::raw(" "),
            Span::styled("SIGN-IN NEEDED", t.chip_attention),
        ]),
        Line::from(vec![
            Span::raw("["),
            Span::styled(fit("Alex Morgan", w.saturating_sub(6).min(24)), t.selected),
            Span::raw("]"),
            Span::raw(" "),
            Span::styled("3", t.unread),
        ]),
        Line::from(vec![
            Span::styled("You", t.own_name),
            Span::styled(" 09:30", t.dim),
            Span::styled("  See you at ten", t.text),
        ]),
        Line::from(vec![
            Span::styled("NOT SENT", t.error),
            Span::raw(" "),
            Span::styled("slow", t.warn),
            Span::raw(" "),
            Span::styled("45%", t.ok),
            Span::raw(" "),
            Span::styled("Enter", t.hint),
        ]),
    ];
    for (i, line) in lines.into_iter().enumerate() {
        f.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + i as u16, area.width, 1),
        );
    }
}
