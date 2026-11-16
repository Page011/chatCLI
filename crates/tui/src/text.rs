//! Width-aware text helpers and the clock used for every time shown on screen.
//!
//! Widths come from ratatui's `Span::width`, which uses unicode-width, so emoji and CJK take
//! the columns the terminal gives them. All helpers are pure so tests need no terminal.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime};
use ratatui::text::Span;

/// Display width of a string in terminal columns.
pub fn sw(s: &str) -> usize {
    Span::raw(s).width()
}

/// Display width of one char.
pub fn cw(c: char) -> usize {
    let mut buf = [0u8; 4];
    sw(c.encode_utf8(&mut buf))
}

/// Cuts `s` to at most `max` columns, ending in an ellipsis when something was removed.
// Limit: width is summed per char, so a ZWJ emoji sequence may be cut mid-sequence.
pub fn truncate(s: &str, max: usize) -> String {
    if sw(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = cw(c);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('\u{2026}');
    out
}

pub fn pad(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    for _ in sw(s)..width {
        out.push(' ');
    }
    out
}

/// Truncate then pad: exactly `width` columns (or one column less when a wide char did not fit).
pub fn fit(s: &str, width: usize) -> String {
    pad(&truncate(s, width), width)
}

/// Last `max` columns of `s`, with a leading ellipsis when cut. Used by the composer so the
/// end of what is being typed stays visible.
pub fn tail(s: &str, max: usize) -> String {
    if sw(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = cw(c);
        if used + w + 1 > max {
            break;
        }
        kept.push(c);
        used += w;
    }
    kept.reverse();
    let mut out = String::from('\u{2026}');
    out.extend(kept);
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// One-line text from the network: control characters become spaces (a message must never
/// be able to send terminal escape codes) and bidi overrides are dropped (they reorder
/// the screen).
pub fn clean_line(s: &str) -> String {
    s.chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Message text: like `clean_line` but keeps newlines, normalising CRLF.
pub fn clean_msg(s: &str) -> String {
    s.replace("\r\n", "\n")
        .chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// Word-wraps `text` to `width` columns. Newlines start a new line; a word longer than the
/// width is split. Never returns an empty vec for non-empty width.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in para.split_whitespace() {
            let ww = sw(word);
            if used > 0 && used + 1 + ww <= width {
                line.push(' ');
                line.push_str(word);
                used += 1 + ww;
                continue;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if ww <= width {
                line.push_str(word);
                used = ww;
                continue;
            }
            for c in word.chars() {
                let w = cw(c);
                if used > 0 && used + w > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line);
    }
    lines
}

/// "Now" and the zone every time is shown in. `tz: None` = the machine's local zone; tests
/// pass a fixed offset so snapshots do not depend on where they run.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub now_ms: i64,
    pub tz: Option<FixedOffset>,
}

impl Clock {
    pub fn system() -> Clock {
        Clock {
            now_ms: chatcli_core::model::now_ms(),
            tz: None,
        }
    }

    fn local(&self, ts_ms: i64) -> Option<NaiveDateTime> {
        let utc = DateTime::from_timestamp_millis(ts_ms)?;
        Some(match self.tz {
            Some(off) => utc.with_timezone(&off).naive_local(),
            None => utc.with_timezone(&Local).naive_local(),
        })
    }

    /// Whole minutes since the epoch: changes when a clock on screen may need a redraw.
    pub fn minute(&self) -> i64 {
        self.now_ms.div_euclid(60_000)
    }

    /// Local calendar day of a timestamp (day separators compare these).
    pub fn day(&self, ts_ms: i64) -> Option<NaiveDate> {
        self.local(ts_ms).map(|t| t.date())
    }

    /// HH:MM, or "--:--" for a timestamp that cannot be shown.
    pub fn hm(&self, ts_ms: i64) -> String {
        self.local(ts_ms)
            .map_or_else(|| "--:--".to_string(), |t| t.format("%H:%M").to_string())
    }

    fn days_ago(&self, ts_ms: i64) -> Option<(NaiveDateTime, i64)> {
        let t = self.local(ts_ms)?;
        let now = self.local(self.now_ms)?;
        Some((t, (now.date() - t.date()).num_days()))
    }

    /// Sidebar time: HH:MM today, weekday within the last week, dd/mm/yy older.
    pub fn list_time(&self, ts_ms: i64) -> String {
        match self.days_ago(ts_ms) {
            None => String::new(),
            Some((t, 0)) => t.format("%H:%M").to_string(),
            Some((t, d)) if (1..=6).contains(&d) => t.format("%a").to_string(),
            Some((t, _)) => t.format("%d/%m/%y").to_string(),
        }
    }

    /// Day separator label in the message pane.
    pub fn day_label(&self, ts_ms: i64) -> String {
        match self.days_ago(ts_ms) {
            None => "unknown date".to_string(),
            Some((_, 0)) => "Today".to_string(),
            Some((_, 1)) => "Yesterday".to_string(),
            Some((t, d)) if (2..=6).contains(&d) => t.format("%A").to_string(),
            Some((t, _)) => t.format("%d/%m/%y").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        FixedOffset::east_opt(10 * 3600)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn clock() -> Clock {
        Clock {
            now_ms: ms(2026, 11, 16, 9, 30),
            tz: FixedOffset::east_opt(10 * 3600),
        }
    }

    #[test]
    fn truncate_counts_columns_not_chars() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello\u{2026}");
        // CJK chars are 2 columns each: 3 chars = 6 columns, so max 4 keeps one char + ellipsis.
        assert_eq!(truncate("\u{4f60}\u{597d}\u{4e16}", 4), "\u{4f60}\u{2026}");
        assert!(sw(&truncate("\u{1F600}\u{1F600}\u{1F600}\u{1F600}", 5)) <= 5);
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn fit_is_exact_width_for_ascii() {
        assert_eq!(sw(&fit("abc", 8)), 8);
        assert_eq!(sw(&fit("abcdefghijkl", 8)), 8);
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abcdef", 4), "\u{2026}def");
        assert_eq!(tail("abc", 4), "abc");
    }

    #[test]
    fn wrap_breaks_on_words_then_hard() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert!(wrap("x", 0).is_empty());
        // A char wider than the width must not loop forever.
        assert_eq!(wrap("\u{4f60}\u{597d}", 1).len(), 2);
    }

    #[test]
    fn clean_strips_escapes_and_bidi() {
        assert_eq!(clean_line("a\u{1b}[31mb\nc"), "a [31mb c");
        assert_eq!(clean_line("a\u{202E}b"), "ab");
        assert_eq!(clean_msg("a\r\nb\u{7}c"), "a\nb c");
    }

    #[test]
    fn list_time_rules() {
        let c = clock();
        assert_eq!(c.list_time(ms(2026, 11, 16, 8, 5)), "08:05");
        assert_eq!(c.list_time(ms(2026, 11, 15, 23, 0)), "Sun");
        assert_eq!(c.list_time(ms(2026, 11, 10, 9, 0)), "Tue");
        assert_eq!(c.list_time(ms(2026, 11, 9, 9, 0)), "09/11/26");
        assert_eq!(c.list_time(ms(2026, 11, 17, 9, 0)), "17/11/26");
    }

    #[test]
    fn day_labels() {
        let c = clock();
        assert_eq!(c.day_label(ms(2026, 11, 16, 0, 1)), "Today");
        assert_eq!(c.day_label(ms(2026, 11, 15, 12, 0)), "Yesterday");
        assert_eq!(c.day_label(ms(2026, 11, 13, 12, 0)), "Friday");
        assert_eq!(c.day_label(ms(2026, 11, 5, 12, 0)), "05/11/26");
    }
}
