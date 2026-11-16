//! What every list on screen shares: moving the selection, keeping it in view, and the text
//! filter. The command bar, search, the suggestion popups, the emoji grid, the settings rows and
//! the chat filter use these, so a list behaves the same wherever it is. Pure functions over
//! plain indexes: each list keeps its own `sel` and `top`.

/// One step from `sel` in a list of `len`, wrapping at both ends (arrow keys).
pub(crate) fn step(sel: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (sel as isize + delta).rem_euclid(len as isize) as usize
}

/// A jump from `sel` in a list of `len`, stopping at the ends (page keys).
pub(crate) fn page(sel: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (sel as isize)
        .saturating_add(delta)
        .clamp(0, len as isize - 1) as usize
}

/// The first row to show so `row` is inside a window of `shown` rows over `total` rows: the
/// window moves only as far as it must, and never past the end (no blank rows below the last).
pub(crate) fn keep_visible(top: usize, row: usize, shown: usize, total: usize) -> usize {
    let shown = shown.max(1);
    let top = top.min(total.saturating_sub(shown));
    if row < top {
        row
    } else if row >= top + shown {
        row + 1 - shown
    } else {
        top
    }
}

/// True when `text` contains `query`, ignoring case. An empty query matches everything. The
/// one rule for "type to filter" lists (the chat filter, the chats in the command bar).
pub(crate) fn matches(text: &str, query: &str) -> bool {
    query.is_empty() || text.to_lowercase().contains(&query.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_wraps_and_page_stops_at_the_ends() {
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(1, 3, 1), 2);
        assert_eq!(step(5, 0, 1), 0, "an empty list has nothing to choose");
        assert_eq!(page(1, 3, 10), 2);
        assert_eq!(page(1, 3, -10), 0);
        assert_eq!(page(0, 0, 5), 0);
        assert_eq!(page(1, 5, isize::MAX), 4, "never overflows");
    }

    #[test]
    fn the_window_follows_the_row_and_never_runs_past_the_end() {
        // Below the window: it ends on the row.
        assert_eq!(keep_visible(0, 9, 4, 10), 6);
        // Above it: it starts on the row.
        assert_eq!(keep_visible(6, 2, 4, 10), 2);
        // Inside it: it stays.
        assert_eq!(keep_visible(3, 5, 4, 10), 3);
        // A list that shrank: the window comes back so no blank rows show.
        assert_eq!(keep_visible(8, 9, 4, 10), 6);
        assert_eq!(keep_visible(5, 0, 4, 2), 0);
        assert_eq!(keep_visible(0, 0, 0, 0), 0);
        assert_eq!(keep_visible(0, 3, 5, 20), 0);
        assert_eq!(keep_visible(0, 7, 5, 20), 3);
        assert_eq!(keep_visible(18, 19, 5, 20), 15);
    }

    #[test]
    fn matching_ignores_case_and_an_empty_query_keeps_everything() {
        assert!(matches("Alex Example", "ex ex"));
        assert!(matches("Alex Example", "ALEX"));
        assert!(matches("田中太郎", "太郎"));
        assert!(matches("anything", ""));
        assert!(!matches("Sam Sample", "alex"));
    }
}
