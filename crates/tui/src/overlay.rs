//! The one popup component. Every box that opens over the screen frames, clears, prompts and
//! notes the same way here: help, the command bar, search, the emoji picker, the `:shortcode`
//! and `@mention` list, the file list and the sign-in panel. Each popup keeps its own place and
//! contents; only how it is drawn lives here.
//!
//! Two kinds:
//! - a **page** (help, commands, search) fills the body and hides the panes behind it, so
//!   nothing of them shows at its edges;
//! - an **anchored** popup (the picker, the lists, the sign-in panel) clears only its own box,
//!   one column wider on each side so no half of a wide character underneath shows at its
//!   edges, and the screen stays in view around it (a reaction still shows its message).

use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::input::TextInput;
use crate::text::{sw, truncate};
use crate::theme::{Panel, Theme};
use crate::ui::{bold, centered_note, pane};

/// An anchored popup: clears `outer` plus one column each side (inside `clip`), frames it and
/// returns the inside.
pub(crate) fn anchored(
    f: &mut Frame,
    t: &Theme,
    outer: Rect,
    clip: Rect,
    title: &str,
    focused: bool,
) -> Rect {
    // Never less than the box itself, whatever `clip` says.
    let left = outer.x.saturating_sub(1).max(clip.x).min(outer.x);
    let right = (outer.right() + 1).min(clip.right()).max(outer.right());
    f.render_widget(Clear, Rect::new(left, outer.y, right - left, outer.height));
    f.render_widget(pane(t, Panel::Popup, title, focused), outer);
    outer.inner(Margin::new(1, 1))
}

/// A page: clears all of `body`, frames `outer` (inside it) and returns the inside.
pub(crate) fn page(f: &mut Frame, t: &Theme, body: Rect, outer: Rect, title: &str) -> Rect {
    f.render_widget(Clear, body);
    f.render_widget(pane(t, Panel::Popup, title, true), outer);
    outer.inner(Margin::new(1, 1))
}

/// Shown instead of a popup that does not fit: `area` cleared, one line a third of the way
/// down saying what did not fit and how to close it.
pub(crate) fn too_small(f: &mut Frame, t: &Theme, area: Rect, what: &str) {
    f.render_widget(Clear, area);
    centered_note(
        f,
        area,
        &format!("Terminal too small for {what}, press Esc to close"),
        t,
    );
}

/// The search row: `prompt` in bold, then the typed text, or `placeholder` in the hint style
/// while nothing is typed. The terminal cursor goes after the text.
pub(crate) fn query(
    f: &mut Frame,
    t: &Theme,
    area: Rect,
    prompt: &str,
    input: &TextInput,
    placeholder: &str,
) {
    let prompt_w = sw(prompt) as u16;
    let (shown, col) = input.view(area.width.saturating_sub(prompt_w) as usize);
    let rest = if input.is_empty() {
        Span::styled(placeholder.to_string(), t.hint)
    } else {
        Span::raw(shown)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.to_string(), bold()),
            rest,
        ])),
        area,
    );
    f.set_cursor_position((area.x + prompt_w + col as u16, area.y));
}

/// A note where the list's first row would be (no match, nothing yet), in the hint style.
pub(crate) fn note(f: &mut Frame, t: &Theme, list: Rect, text: &str) {
    if list.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(Span::styled(truncate(text, list.width as usize), t.hint)),
        Rect::new(list.x, list.y, list.width, 1),
    );
}

/// One list row: the selected one in the selected style over its own spans.
pub(crate) fn row(f: &mut Frame, t: &Theme, area: Rect, line: Line<'static>, selected: bool) {
    let p = Paragraph::new(line);
    f.render_widget(if selected { p.style(t.selected) } else { p }, area);
}
