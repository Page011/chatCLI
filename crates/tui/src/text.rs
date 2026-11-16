//! Width-aware text helpers and the clock used for every time shown on screen.
//!
//! Everything that measures, cuts or wraps text works on grapheme clusters (what a user sees as
//! one character: a flag, a family of emoji joined by ZWJ, a letter plus its accent) and takes
//! each cluster's width from the whole cluster string. Measuring per `char` would count a flag
//! as 4 columns and could cut a family in half. The width comes from `unicode-width`, the same
//! table ratatui uses to place cells, so the columns we count are the columns it draws.
//!
//! Terminals can still disagree with the Unicode tables (some draw a text-style symbol with
//! VS16 one column wide); chatcli cannot detect that, and a result that is right by the
//! standard may look off there. All helpers are pure so tests need no terminal.

use chatcli_core::settings::{DateOrder, TimeFormat};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The grapheme clusters of `s`, in order.
pub fn graphemes(s: &str) -> unicode_segmentation::Graphemes<'_> {
    s.graphemes(true)
}

/// Columns one cluster takes, measured on the whole cluster (emoji presentation counts as 2).
pub fn cluster_width(g: &str) -> usize {
    UnicodeWidthStr::width(g)
}

/// Display width of a string in terminal columns: the sum of its clusters' widths.
pub fn display_width(s: &str) -> usize {
    // Printable ASCII is one column a byte: skips the cluster walk for most message text.
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.len();
    }
    graphemes(s).map(cluster_width).sum()
}

/// Short name for `display_width`, used all over the drawing code.
pub fn sw(s: &str) -> usize {
    display_width(s)
}

/// Cuts `s` to at most `max` columns, ending in an ellipsis when something was removed. Never
/// cuts inside a cluster.
pub fn truncate(s: &str, max: usize) -> String {
    if sw(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for g in graphemes(s) {
        let w = cluster_width(g);
        if used + w + 1 > max {
            break;
        }
        out.push_str(g);
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

/// Pasted text for the composer: line breaks (CRLF, lone CR) become `\n`, tabs become four
/// spaces, every other control character is dropped (so an escape sequence in the clipboard
/// cannot reach the screen or the sent message) and bidi overrides are dropped.
pub fn clean_paste(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() || is_bidi_control(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// One word, or a piece of a word that was split because it is wider than the line. `start` is
/// the byte offset of the piece in the text that was wrapped, so a caller can map a part of the
/// original text (a link) onto the wrapped lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub start: usize,
    pub text: String,
}

/// Word-wraps `text` to `width` columns and keeps where each piece came from. Words on a line
/// are separated by one space (runs of spaces collapse). Newlines start a new line. A word
/// longer than the width is split between clusters, never inside one. Empty for width 0.
pub fn wrap_pieces(text: &str, width: usize) -> Vec<Vec<Piece>> {
    if width == 0 {
        return Vec::new();
    }
    let base = text.as_ptr() as usize;
    let mut lines: Vec<Vec<Piece>> = Vec::new();
    for para in text.split('\n') {
        let mut line: Vec<Piece> = Vec::new();
        let mut used = 0;
        for word in para.split_whitespace() {
            let start = word.as_ptr() as usize - base;
            let ww = sw(word);
            if used > 0 && used + 1 + ww <= width {
                line.push(Piece {
                    start,
                    text: word.to_string(),
                });
                used += 1 + ww;
                continue;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if ww <= width {
                line.push(Piece {
                    start,
                    text: word.to_string(),
                });
                used = ww;
                continue;
            }
            // Too wide for one line: fill lines cluster by cluster.
            let mut chunk = Piece {
                start,
                text: String::new(),
            };
            for (off, g) in word.grapheme_indices(true) {
                let w = cluster_width(g);
                if used > 0 && used + w > width {
                    lines.push(vec![std::mem::replace(
                        &mut chunk,
                        Piece {
                            start: start + off,
                            text: String::new(),
                        },
                    )]);
                    used = 0;
                }
                chunk.text.push_str(g);
                used += w;
            }
            line.push(chunk);
        }
        lines.push(line);
    }
    lines
}

/// Word-wraps `text` to `width` columns (see `wrap_pieces`), one string per line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_pieces(text, width)
        .into_iter()
        .map(|line| {
            line.iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// How times and dates are written: `[layout] time_format` and `date_order`. The rules for which
/// form shows when (time today, weekday this week, date older) do not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fmt {
    pub time: TimeFormat,
    pub date: DateOrder,
}

impl Fmt {
    fn time_pattern(self) -> &'static str {
        match self.time {
            TimeFormat::H24 => "%H:%M",
            TimeFormat::H12 => "%I:%M %p",
        }
    }

    fn date_pattern(self) -> &'static str {
        match self.date {
            DateOrder::Dmy => "%d/%m/%y",
            DateOrder::Mdy => "%m/%d/%y",
            DateOrder::Ymd => "%Y-%m-%d",
        }
    }

    /// Columns a time takes: `09:30` or `09:30 AM`.
    pub fn time_width(self) -> usize {
        match self.time {
            TimeFormat::H24 => 5,
            TimeFormat::H12 => 8,
        }
    }
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
        self.hm_f(ts_ms, Fmt::default())
    }

    /// The time in the user's `[layout] time_format`; `fmt.time_width()` columns.
    pub fn hm_f(&self, ts_ms: i64, fmt: Fmt) -> String {
        self.local(ts_ms).map_or_else(
            || match fmt.time {
                TimeFormat::H24 => "--:--".to_string(),
                TimeFormat::H12 => "--:-- --".to_string(),
            },
            |t| t.format(fmt.time_pattern()).to_string(),
        )
    }

    fn days_ago(&self, ts_ms: i64) -> Option<(NaiveDateTime, i64)> {
        let t = self.local(ts_ms)?;
        let now = self.local(self.now_ms)?;
        Some((t, (now.date() - t.date()).num_days()))
    }

    /// Sidebar time: HH:MM today, weekday within the last week, dd/mm/yy older.
    pub fn list_time(&self, ts_ms: i64) -> String {
        self.list_time_f(ts_ms, Fmt::default())
    }

    /// Same rules as `list_time`, in the user's time format and date order.
    pub fn list_time_f(&self, ts_ms: i64, fmt: Fmt) -> String {
        match self.days_ago(ts_ms) {
            None => String::new(),
            Some((t, 0)) => t.format(fmt.time_pattern()).to_string(),
            Some((t, d)) if (1..=6).contains(&d) => t.format("%a").to_string(),
            Some((t, _)) => t.format(fmt.date_pattern()).to_string(),
        }
    }

    /// Day separator label in the message pane.
    pub fn day_label(&self, ts_ms: i64) -> String {
        self.day_label_f(ts_ms, Fmt::default())
    }

    /// Same rules as `day_label`, in the user's date order.
    pub fn day_label_f(&self, ts_ms: i64, fmt: Fmt) -> String {
        match self.days_ago(ts_ms) {
            None => "unknown date".to_string(),
            Some((_, 0)) => "Today".to_string(),
            Some((_, 1)) => "Yesterday".to_string(),
            Some((t, d)) if (2..=6).contains(&d) => t.format("%A").to_string(),
            Some((t, _)) => t.format(fmt.date_pattern()).to_string(),
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
    fn wrap_breaks_on_words_then_hard() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert!(wrap("x", 0).is_empty());
        // A char wider than the width must not loop forever.
        assert_eq!(wrap("\u{4f60}\u{597d}", 1).len(), 2);
    }

    /// Strings people actually send, with the columns the Unicode tables give them.
    const WIDTHS: &[(&str, &str, usize)] = &[
        (
            "family ZWJ",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            2,
        ),
        ("flag", "\u{1F1FA}\u{1F1F3}", 2),
        ("thumbs up, skin tone", "\u{1F44D}\u{1F3FD}", 2),
        ("heart + VS16", "\u{2764}\u{FE0F}", 2),
        ("heart, text style", "\u{2764}", 1),
        ("keycap 1", "1\u{FE0F}\u{20E3}", 2),
        ("profession ZWJ", "\u{1F469}\u{200D}\u{1F4BB}", 2),
        ("rainbow flag", "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}", 2),
        ("CJK", "\u{4f60}\u{597d}", 4),
        ("e + combining acute", "e\u{301}", 1),
        ("ASCII", "hello", 5),
        ("mixed", "a\u{1F600}b\u{4f60}", 6),
    ];

    #[test]
    fn display_width_table() {
        for (name, s, want) in WIDTHS {
            assert_eq!(display_width(s), *want, "{name}");
            assert_eq!(sw(s), *want, "{name}");
        }
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn ascii_fast_path_agrees_with_the_cluster_walk() {
        for s in [
            "",
            "hello world",
            "~ ! \" #",
            " ",
            "a\tb",
            "tab\u{7f}del",
            "\u{1F600}x",
        ] {
            assert_eq!(
                display_width(s),
                graphemes(s).map(cluster_width).sum::<usize>(),
                "{s:?}"
            );
        }
    }

    #[test]
    fn graphemes_keep_clusters_whole() {
        let g: Vec<&str> =
            graphemes("a\u{1F468}\u{200D}\u{1F469}\u{1F1FA}\u{1F1F3}e\u{301}").collect();
        assert_eq!(
            g,
            vec![
                "a",
                "\u{1F468}\u{200D}\u{1F469}",
                "\u{1F1FA}\u{1F1F3}",
                "e\u{301}"
            ]
        );
    }

    /// No cluster is split by truncation or wrapping, for any limit.
    #[test]
    fn cutting_never_splits_a_cluster() {
        let all: String = WIDTHS.iter().map(|w| w.1).collect::<Vec<_>>().join(" ");
        let mut samples: Vec<String> = WIDTHS.iter().map(|w| w.1.repeat(3)).collect();
        samples.push(all);
        for s in &samples {
            let want: Vec<&str> = graphemes(s).collect();
            for max in 0..=14 {
                let t = truncate(s, max);
                assert!(sw(&t) <= max, "truncate width {t:?}");
                let kept: Vec<&str> = graphemes(t.trim_end_matches('\u{2026}')).collect();
                assert_eq!(kept, want[..kept.len()], "truncate {s:?} to {max}");
            }
            for width in 1..=14 {
                // Without whitespace in the sample, wrapping only hard-splits: the clusters
                // come back in order, none lost, none broken, and no line is too wide unless a
                // single cluster is.
                let flat: Vec<&str> = graphemes(s).filter(|g| g.trim() != "").collect();
                let lines = wrap(s, width);
                let got: Vec<&str> = lines
                    .iter()
                    .flat_map(|l| graphemes(l))
                    .filter(|g| g.trim() != "")
                    .collect();
                assert_eq!(got, flat, "wrap {s:?} at {width}");
                for l in &lines {
                    assert!(
                        sw(l) <= width || graphemes(l).count() == 1,
                        "{l:?} at {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn truncate_of_emoji_stays_aligned() {
        // A family emoji is 2 columns: 3 of them need 6, so 5 columns keep one plus the ellipsis.
        let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(truncate(&fam.repeat(3), 5), format!("{fam}{fam}\u{2026}"));
        assert_eq!(sw(&fit(&format!("{fam}{fam}{fam}"), 5)), 5);
    }

    #[test]
    fn wrap_pieces_remember_where_they_came_from() {
        let text = "see https://example.com/long/path now";
        let lines = wrap_pieces(text, 12);
        for line in &lines {
            for p in line {
                // Every piece sits where it says in the source.
                assert!(text[p.start..].starts_with(&p.text), "{p:?}");
            }
        }
        assert!(lines.len() > 2, "{lines:?}");
    }

    #[test]
    fn clean_paste_keeps_newlines_and_drops_escapes() {
        assert_eq!(clean_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(clean_paste("a\tb"), "a    b");
        // The escape byte goes; what follows it is plain text and harmless.
        assert_eq!(clean_paste("x\u{1b}[31mred\u{1b}[0m"), "x[31mred[0m");
        assert_eq!(clean_paste("a\u{7}b\u{0}c\u{202E}d"), "abcd");
        assert_eq!(
            clean_paste("caf\u{e9} \u{1F44D}\u{1F3FD}"),
            "caf\u{e9} \u{1F44D}\u{1F3FD}"
        );
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

    #[test]
    fn time_format_and_date_order_change_only_how_things_are_written() {
        let c = clock();
        let h12 = Fmt {
            time: TimeFormat::H12,
            date: DateOrder::Mdy,
        };
        // Same rules as ever: time today, weekday this week, date older.
        assert_eq!(c.hm_f(ms(2026, 11, 16, 9, 5), h12), "09:05 AM");
        assert_eq!(c.hm_f(ms(2026, 11, 16, 21, 5), h12), "09:05 PM");
        assert_eq!(c.hm_f(ms(2026, 11, 16, 0, 5), h12), "12:05 AM");
        assert_eq!(c.list_time_f(ms(2026, 11, 16, 15, 30), h12), "03:30 PM");
        assert_eq!(c.list_time_f(ms(2026, 11, 15, 23, 0), h12), "Sun");
        assert_eq!(c.list_time_f(ms(2026, 11, 9, 9, 0), h12), "11/09/26");
        assert_eq!(c.day_label_f(ms(2026, 11, 5, 12, 0), h12), "11/05/26");
        let ymd = Fmt {
            time: TimeFormat::H24,
            date: DateOrder::Ymd,
        };
        assert_eq!(c.list_time_f(ms(2026, 11, 9, 9, 0), ymd), "2026-11-09");
        assert_eq!(c.day_label_f(ms(2026, 11, 5, 12, 0), ymd), "2026-11-05");
        assert_eq!(c.day_label_f(ms(2026, 11, 16, 0, 1), ymd), "Today");
        // The default is today's output, through the old entry points too.
        assert_eq!(
            Fmt::default(),
            Fmt {
                time: TimeFormat::H24,
                date: DateOrder::Dmy
            }
        );
        assert_eq!(
            c.hm(ms(2026, 11, 16, 9, 5)),
            c.hm_f(ms(2026, 11, 16, 9, 5), Fmt::default())
        );
    }

    #[test]
    fn a_time_is_always_the_width_the_layout_reserves() {
        let c = clock();
        for time in [TimeFormat::H24, TimeFormat::H12] {
            let fmt = Fmt {
                time,
                date: DateOrder::Dmy,
            };
            for h in [0, 1, 9, 11, 12, 13, 23] {
                let t = c.hm_f(ms(2026, 11, 16, h, 7), fmt);
                assert_eq!(sw(&t), fmt.time_width(), "{t}");
            }
            assert_eq!(sw(&c.hm_f(i64::MAX, fmt)), fmt.time_width());
        }
    }
}
