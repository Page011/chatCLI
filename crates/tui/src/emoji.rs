//! Emoji for the composer: `:shortcode:` suggestions and the picker's model. The table comes
//! from the `emojis` crate (static, no network). Pure and tested without a terminal; drawing
//! lives in `ui`.

use crate::input::TextInput;
use emojis::{Emoji, Group, SkinTone};

/// Suggestions shown in the `:shortcode` popup at most.
pub const SUGGESTIONS: usize = 8;
/// Recent emoji kept (also the limit `Store::recent_emoji` clamps to).
pub const RECENT_MAX: usize = 30;

/// A shortcode that matched: the emoji and the shortcode that matched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub emoji: &'static str,
    pub shortcode: String,
}

fn is_code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// The text after a `:` that the cursor is at the end of, when it should open the popup: a colon
/// at the start of a word, then at least two shortcode characters. `10:30`, `https://x` and
/// `a:b` have a letter or digit in front of the colon, so they never match.
pub fn shortcode_query(before_cursor: &str) -> Option<&str> {
    let tail_start = before_cursor
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_code_char(c))
        .last()
        .map_or(before_cursor.len(), |(i, _)| i);
    let query = &before_cursor[tail_start..];
    let head = &before_cursor[..tail_start];
    let head = head.strip_suffix(':')?;
    if query.chars().count() < 2 {
        return None;
    }
    // What comes before the colon must be the start, a space, an opening bracket or quote, or a
    // non-ASCII character (an emoji typed just before). A letter, digit or `/` means this is a
    // time, a URL or a word with a colon in it.
    let ok = match head.chars().next_back() {
        None => true,
        Some(p) => p.is_whitespace() || "([{\"'<>*~,;".contains(p) || !p.is_ascii(),
    };
    ok.then_some(query)
}

/// Emoji worth listing: skin-tone variants are left out (the base emoji stands for them).
fn base_emojis() -> impl Iterator<Item = &'static Emoji> {
    emojis::iter().filter(|e| e.skin_tone().is_none_or(|t| t == SkinTone::Default))
}

/// Emoji whose shortcode starts with `query`, or has a word (between underscores) that does,
/// best first: exact, then prefix, then word start; shorter shortcodes before longer ones; ties
/// in the table's own order. A match in the middle of a word (`lithuania` for `thu`) is noise.
pub fn suggest(query: &str, limit: usize) -> Vec<Suggestion> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, usize, &'static Emoji, &'static str)> = Vec::new();
    for (order, e) in base_emojis().enumerate() {
        let best = e
            .shortcodes()
            .filter_map(|sc| {
                let rank = if sc == q {
                    0
                } else if sc.starts_with(&q) {
                    1
                } else if sc.split('_').any(|w| w.starts_with(&q)) {
                    2
                } else {
                    return None;
                };
                Some((rank, sc.len(), sc))
            })
            .min();
        if let Some((rank, len, sc)) = best {
            hits.push((rank, len, order, e, sc));
        }
    }
    hits.sort_by_key(|h| (h.0, h.1, h.2));
    hits.into_iter()
        .take(limit)
        .map(|h| Suggestion {
            emoji: h.3.as_str(),
            shortcode: h.4.to_string(),
        })
        .collect()
}

// ---- picker ----

/// What choosing an emoji in the picker does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerFor {
    /// Insert into the message box.
    Compose,
    /// React to a message with it (`Command::React`).
    React { chat_id: String, message_id: String },
}

/// Categories across the top: recents first, then the Unicode groups.
pub const CATEGORIES: [&str; 10] = [
    "Recent", "Faces", "People", "Animals", "Food", "Travel", "Activity", "Objects", "Symbols",
    "Flags",
];

fn group_of(cat: usize) -> Option<Group> {
    Some(match cat {
        1 => Group::SmileysAndEmotion,
        2 => Group::PeopleAndBody,
        3 => Group::AnimalsAndNature,
        4 => Group::FoodAndDrink,
        5 => Group::TravelAndPlaces,
        6 => Group::Activities,
        7 => Group::Objects,
        8 => Group::Symbols,
        9 => Group::Flags,
        _ => return None,
    })
}

/// The picker's state: what it is for, the search text, the category, the selected cell (an index
/// into `items`) and the first visible grid row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub purpose: PickerFor,
    pub query: TextInput,
    pub cat: usize,
    pub sel: usize,
    pub top: usize,
}

impl Picker {
    pub fn new(purpose: PickerFor, has_recents: bool) -> Picker {
        Picker {
            purpose,
            query: TextInput::default(),
            // Recent emoji first when there are any; otherwise the first real category.
            cat: usize::from(!has_recents),
            sel: 0,
            top: 0,
        }
    }

    /// The emoji on the grid: a search over every category by name or shortcode when there is a
    /// query, otherwise the category (recents come from `recents`, newest first; entries the
    /// table does not know are skipped).
    pub fn items(&self, recents: &[String]) -> Vec<&'static Emoji> {
        let q = self.query.text().trim().to_lowercase();
        if !q.is_empty() {
            let mut hits: Vec<(u8, &'static Emoji)> = base_emojis()
                .filter_map(|e| {
                    let name = e.name().to_lowercase();
                    let rank = if name.starts_with(&q) || e.shortcodes().any(|s| s.starts_with(&q))
                    {
                        0
                    } else if name.contains(&q) || e.shortcodes().any(|s| s.contains(&q)) {
                        1
                    } else {
                        return None;
                    };
                    Some((rank, e))
                })
                .collect();
            hits.sort_by_key(|h| h.0);
            return hits.into_iter().map(|h| h.1).collect();
        }
        match group_of(self.cat) {
            Some(g) => g
                .emojis()
                .filter(|e| e.skin_tone().is_none_or(|t| t == SkinTone::Default))
                .collect(),
            None => recents.iter().filter_map(|s| emojis::get(s)).collect(),
        }
    }

    /// Moves to category `cat` (wrapping), selection back to the start.
    pub fn go_category(&mut self, delta: isize) {
        self.cat = (self.cat as isize + delta).rem_euclid(CATEGORIES.len() as isize) as usize;
        self.sel = 0;
        self.top = 0;
    }

    /// Moves the selection one step: `(dx, dy)` in cells on a grid `cols` wide with `len` cells.
    /// Left and right wrap across rows; up and down stop at the edges.
    pub fn step(&mut self, len: usize, cols: usize, dx: isize, dy: isize) {
        if len == 0 || cols == 0 {
            self.sel = 0;
            return;
        }
        let last = len - 1;
        let mut sel = self.sel.min(last) as isize;
        sel += dx;
        sel += dy * cols as isize;
        // A vertical step off the top or bottom stays put; a horizontal one clamps at the ends.
        if dy != 0 && (sel < 0 || sel > last as isize) {
            sel = self.sel.min(last) as isize;
        }
        self.sel = sel.clamp(0, last as isize) as usize;
    }

    /// Scrolls the first visible row just enough to keep the selection on screen.
    pub fn keep_visible(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 {
            return;
        }
        let row = self.sel / cols;
        if row < self.top {
            self.top = row;
        } else if row >= self.top + rows {
            self.top = row + 1 - rows;
        }
    }

    /// Scrolls the view by `delta` rows (the wheel), keeping it inside the grid, and the
    /// selection inside the view.
    pub fn scroll(&mut self, len: usize, cols: usize, rows: usize, delta: isize) {
        if cols == 0 || rows == 0 {
            return;
        }
        let total = len.div_ceil(cols);
        let max_top = total.saturating_sub(rows);
        self.top = (self.top as isize + delta).clamp(0, max_top as isize) as usize;
        let (lo, hi) = (self.top * cols, ((self.top + rows) * cols).min(len));
        if len > 0 {
            self.sel = self.sel.clamp(lo, hi.saturating_sub(1).max(lo));
        }
    }
}

/// Puts `emoji` first in a recents list, drops its older copy, keeps `RECENT_MAX`.
pub fn touch_recent(recents: &mut Vec<String>, emoji: &str) {
    recents.retain(|e| e != emoji);
    recents.insert(0, emoji.to_string());
    recents.truncate(RECENT_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_opens_at_a_word_start_with_two_characters() {
        assert_eq!(shortcode_query(":thu"), Some("thu"));
        assert_eq!(shortcode_query("hello :thu"), Some("thu"));
        assert_eq!(shortcode_query("(:smile"), Some("smile"));
        assert_eq!(shortcode_query("\"hi :+1"), Some("+1"));
        assert_eq!(shortcode_query("\u{1F600}:sm"), Some("sm"));
        assert_eq!(shortcode_query("line one\n:ok"), Some("ok"));
    }

    #[test]
    fn times_urls_and_words_with_colons_do_not_open_it() {
        for text in [
            "10:30",
            "at 10:30",
            "meet at 9:45am",
            "https://x",
            "http://example.com:8080",
            "https://example.com/a:bc",
            "note:thu",
            "a:b",
            "ratio 3:2",
            "std::vec",
            ":",
            ":t",
            "hello :t",
            "hello ",
            "hello :thu ",
            ":thu:",
            "",
        ] {
            assert_eq!(shortcode_query(text), None, "{text:?}");
        }
    }

    #[test]
    fn thu_suggests_thumbs_up_first() {
        let s = suggest("thu", SUGGESTIONS);
        assert!(!s.is_empty());
        assert_eq!(s[0].emoji, "\u{1F44D}", "{s:?}");
        assert_eq!(s[0].shortcode, "thumbsup");
        assert!(s.iter().any(|x| x.shortcode == "thumbsdown"));
    }

    #[test]
    fn matcher_ranks_exact_then_prefix_then_word_start() {
        let s = suggest("smile", 30);
        assert_eq!(s[0].shortcode, "smile", "exact first");
        let ranks: Vec<u8> = s
            .iter()
            .map(|x| {
                if x.shortcode == "smile" {
                    0
                } else if x.shortcode.starts_with("smile") {
                    1
                } else {
                    2
                }
            })
            .collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{s:?}");
        // The middle of a word does not count; the start of a later word does.
        let thu = suggest("thu", 30);
        assert!(!thu.iter().any(|x| x.shortcode == "lithuania"), "{thu:?}");
        assert!(thu
            .iter()
            .any(|x| x.shortcode == "hand_with_index_finger_and_thumb_crossed"));
        assert!(suggest("zzzzqqq", 8).is_empty());
        assert!(suggest("", 8).is_empty());
        assert_eq!(suggest("o", 3).len(), 3, "limit");
        assert_eq!(
            suggest("THU", 3)[0].shortcode,
            "thumbsup",
            "case does not matter"
        );
        assert_eq!(suggest("+1", 3)[0].emoji, "\u{1F44D}");
    }

    #[test]
    fn suggestions_have_no_skin_tone_variants() {
        for s in suggest("hand", 100) {
            let e = emojis::get(s.emoji).unwrap();
            assert!(
                e.skin_tone().is_none_or(|t| t == SkinTone::Default),
                "{s:?}"
            );
        }
    }

    #[test]
    fn picker_categories_and_search() {
        let none: Vec<String> = Vec::new();
        let mut p = Picker::new(PickerFor::Compose, false);
        assert_eq!(p.cat, 1, "no recents: starts on the first category");
        assert!(p.items(&none).len() > 50);
        p.go_category(-1);
        assert_eq!(p.cat, 0);
        assert!(p.items(&none).is_empty(), "no recents yet");
        p.go_category(-1);
        assert_eq!(p.cat, 9, "wraps");
        assert!(p.items(&none).iter().all(|e| e.group() == Group::Flags));

        p.query = "thumbs up".into();
        let hits = p.items(&none);
        assert_eq!(hits[0].as_str(), "\u{1F44D}", "search crosses categories");
        p.query = "xxnotanemojixx".into();
        assert!(p.items(&none).is_empty());
    }

    #[test]
    fn recents_show_first_and_skip_unknown_entries() {
        let recents = vec![
            "\u{1F389}".to_string(),
            "not an emoji".to_string(),
            "\u{1F44D}".to_string(),
        ];
        let p = Picker::new(PickerFor::Compose, true);
        assert_eq!(p.cat, 0);
        let items = p.items(&recents);
        assert_eq!(
            items.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
            ["\u{1F389}", "\u{1F44D}"]
        );
    }

    #[test]
    fn recents_order_and_cap() {
        let mut r: Vec<String> = Vec::new();
        touch_recent(&mut r, "a");
        touch_recent(&mut r, "b");
        touch_recent(&mut r, "a");
        assert_eq!(r, ["a", "b"], "moved to the front, no duplicate");
        for i in 0..40 {
            touch_recent(&mut r, &format!("e{i}"));
        }
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], "e39");
    }

    #[test]
    fn grid_movement_stays_inside() {
        let mut p = Picker::new(PickerFor::Compose, false);
        // 10 cells, 4 columns: rows are 0-3, 4-7, 8-9.
        p.step(10, 4, 1, 0);
        assert_eq!(p.sel, 1);
        p.step(10, 4, 0, 1);
        assert_eq!(p.sel, 5);
        p.step(10, 4, 0, 1);
        assert_eq!(p.sel, 9);
        p.step(10, 4, 0, 1);
        assert_eq!(p.sel, 9, "down off the last row stays");
        p.step(10, 4, 1, 0);
        assert_eq!(p.sel, 9, "right at the end stays");
        p.step(10, 4, 0, -1);
        p.step(10, 4, 0, -1);
        assert_eq!(p.sel, 1);
        p.step(10, 4, 0, -1);
        assert_eq!(p.sel, 1, "up off the first row stays");
        p.step(10, 4, -1, 0);
        p.step(10, 4, -1, 0);
        assert_eq!(p.sel, 0);
        p.step(0, 4, 1, 0);
        assert_eq!(p.sel, 0, "empty grid");
    }

    #[test]
    fn view_follows_the_selection_and_the_wheel() {
        let mut p = Picker::new(PickerFor::Compose, false);
        p.sel = 17; // row 4 of a 4-column grid
        p.keep_visible(4, 3);
        assert_eq!(p.top, 2);
        p.sel = 0;
        p.keep_visible(4, 3);
        assert_eq!(p.top, 0);
        // 20 cells = 5 rows, 3 visible: the wheel stops at top 2.
        p.scroll(20, 4, 3, 10);
        assert_eq!(p.top, 2);
        assert!(p.sel >= 8, "selection pulled into view: {}", p.sel);
        p.scroll(20, 4, 3, -10);
        assert_eq!(p.top, 0);
        assert!(p.sel < 12);
    }
}
