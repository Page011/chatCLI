//! Typo tolerance for search: how far apart two words are, and which stored word is nearest to
//! one the index does not have. Pure: `Store::correct_query` reads the stored words (FTS5's
//! vocabulary) and hands them here.
//!
//! Costs are in half edits. A letter missing, one too many, or two neighbours swapped costs
//! `STEP` (2); a wrong letter costs `WRONG` (3). Real typos are mostly the first kind, so
//! `tmorow` is nearer `tomorrow` (two letters missing, 4) than `throw` (a wrong letter and one
//! too many, 5), though both are two plain edits away.

/// A letter missing, one too many, or two neighbours swapped.
pub const STEP: usize = 2;
/// A wrong letter.
pub const WRONG: usize = 3;

/// How far (in half edits) a word of this length may be from its correction: one wrong letter
/// for 3 to 5 letters, two for longer words. Short words are not corrected: one edit away from a
/// two-letter word is nearly every other two-letter word.
pub fn allowed(word: &str) -> usize {
    match word.chars().count() {
        0..=2 => 0,
        3..=5 => WRONG,
        _ => 2 * WRONG,
    }
}

/// Weighted edit distance by character, in half edits (`STEP`, `WRONG`): insertions,
/// deletions, substitutions, and two neighbours swapped as one step (the "optimal string
/// alignment" distance).
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    to_prefixes(a, &b)[b.len()]
}

/// `distance` from `a` to every start of `b`: entry `j` is the distance to `b[..j]`. The last row
/// of the one table `distance` fills, so the starts of a word cost one table, not one each.
fn to_prefixes(a: &str, b: &[char]) -> Vec<usize> {
    let a: Vec<char> = a.chars().collect();
    // Three rows: two back (for swaps), one back, and this one.
    let mut two_back = vec![0; b.len() + 1];
    let mut back: Vec<usize> = (0..=b.len()).map(|j| j * STEP).collect();
    let mut row = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        row[0] = i * STEP;
        for j in 1..=b.len() {
            let change = if a[i - 1] == b[j - 1] { 0 } else { WRONG };
            row[j] = (back[j] + STEP)
                .min(row[j - 1] + STEP)
                .min(back[j - 1] + change);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                row[j] = row[j].min(two_back[j - 2] + STEP);
            }
        }
        std::mem::swap(&mut two_back, &mut back);
        std::mem::swap(&mut back, &mut row);
    }
    back
}

/// The stored words nearest to `word`, at most `max` (half edits) away, best first, at most
/// `keep` of them: the nearest, then the most messages, then the first in alphabetical order
/// (so the order never depends on the order the words came in). `candidates` are (word,
/// messages it is in). The caller tries them in turn: a stored word can be in no message it may
/// show.
///
/// For the word being typed (`typing`), each candidate's start is compared instead, from as many
/// letters shorter to as many longer as `max` allows letters missing or extra, and the start is
/// the answer: it is then searched as a prefix, like any word being typed. A start shorter than
/// the typed word drops letters the user typed, each costing as much as a wrong letter (else a
/// rare word starting `budje` would beat `budget` for `budjet`). Each candidate offers its
/// nearest start (the length nearest the typed word's on a tie); words that offer the same start
/// count together.
pub fn ranked<'a>(
    word: &str,
    typing: bool,
    max: usize,
    keep: usize,
    candidates: impl IntoIterator<Item = (&'a str, i64)>,
) -> Vec<String> {
    let len = word.chars().count();
    let reach = max / STEP;
    // Each answer: (distance, messages).
    let mut found: std::collections::HashMap<String, (usize, i64)> = Default::default();
    for (cand, docs) in candidates {
        let clen = cand.chars().count();
        let best = if typing {
            let chars: Vec<char> = cand.chars().take(len + reach).collect();
            let row = to_prefixes(word, &chars);
            (len.saturating_sub(reach).max(1)..=chars.len())
                .map(|take| {
                    // A start shorter than the typed word drops letters the user did type:
                    // each costs as much as a wrong one.
                    let dropped = len.saturating_sub(take) * (WRONG - STEP);
                    (row[take] + dropped, take.abs_diff(len), take)
                })
                .min()
                .map(|(d, off, take)| (d, off, chars[..take].iter().collect::<String>()))
        } else if clen.abs_diff(len) <= reach {
            Some((distance(word, cand), 0, cand.to_string()))
        } else {
            None
        };
        let Some((d, _, text)) = best.filter(|(d, _, text)| *d <= max && text != word) else {
            continue;
        };
        found.entry(text).or_insert((d, 0)).1 += docs;
    }
    let mut list: Vec<(String, (usize, i64))> = found.into_iter().collect();
    list.sort_by(|(a, (ad, adocs)), (b, (bd, bdocs))| (ad, -adocs, a).cmp(&(bd, -bdocs, b)));
    list.into_iter().take(keep).map(|(text, _)| text).collect()
}

/// The best of `ranked`: the nearest stored word, or start of one while typing.
pub fn closest<'a>(
    word: &str,
    typing: bool,
    max: usize,
    candidates: impl IntoIterator<Item = (&'a str, i64)>,
) -> Option<String> {
    ranked(word, typing, max, 1, candidates).pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_letter_costs_more_than_a_missing_extra_or_swapped_one() {
        for (a, b, d) in [
            ("budget", "budget", 0),
            ("budjet", "budget", WRONG),
            ("budgt", "budget", STEP),
            ("budgget", "budget", STEP),
            ("teh", "the", STEP),
            ("recieve", "receive", STEP),
            ("abc", "", 3 * STEP),
            ("", "", 0),
            ("kitten", "sitting", 2 * WRONG + STEP),
            ("café", "cafe", WRONG),
            ("東京", "東京都", STEP),
            // The case that set the weights.
            ("tmorow", "tomorrow", 2 * STEP),
            ("tmorow", "throw", WRONG + STEP),
        ] {
            assert_eq!(distance(a, b), d, "{a} -> {b}");
            assert_eq!(distance(b, a), d, "{b} -> {a}");
        }
    }

    #[test]
    fn short_words_are_never_corrected_and_long_ones_may_be_further_off() {
        assert_eq!(allowed("ok"), 0);
        assert_eq!(allowed("teh"), WRONG);
        assert_eq!(allowed("budjet"), 2 * WRONG);
        assert_eq!(allowed("東京"), 0);
    }

    #[test]
    fn the_nearest_word_wins_then_the_commonest_then_the_alphabetical() {
        let words = [
            ("budget", 40),
            ("budgie", 2),
            ("bridget", 5),
            ("budgets", 9),
        ];
        assert_eq!(closest("budjet", false, 6, words), Some("budget".into()));
        // Two words a wrong letter away: the one in more messages.
        let tie = [("cart", 3), ("card", 12)];
        assert_eq!(closest("carx", false, 3, tie), Some("card".into()));
        let even = [("cart", 3), ("card", 3)];
        assert_eq!(closest("carx", false, 3, even), Some("card".into()));
        // Nothing near enough.
        assert_eq!(closest("zzzzzz", false, 6, words), None);
        // Too different in length to be within reach is not even compared.
        assert_eq!(closest("bud", false, 3, words), None);
        // Several, best first, for the caller to try in turn.
        let m = [("mating", 2), ("meeting", 40)];
        assert_eq!(ranked("meting", false, 3, 5, m), ["meeting", "mating"]);
    }

    #[test]
    fn a_word_being_typed_is_matched_against_the_start_of_each_word() {
        let words = [("budget", 40), ("buddy", 7), ("bridge", 3)];
        // "budj" starts no stored word. Dropping the "j" costs as much as changing it, and the
        // start of the typed length wins: "budg", not "bud".
        assert_eq!(closest("budj", true, 3, words), Some("budg".into()));
        // So a rare start one letter short never beats a common word one wrong letter off.
        let b = [("budget", 40), ("budjes", 1)];
        assert_eq!(closest("budjet", true, 6, b), Some("budget".into()));
        // A letter missing: "brdg" is the start "bridg".
        assert_eq!(
            closest("brdg", true, 3, [("bridge", 3)]),
            Some("bridg".into())
        );
        // Letters missing reach starts longer than the typed word: "tmorow" is "tomorrow",
        // not the commoner "throw".
        let t = [("throw", 50), ("tomorrow", 10)];
        assert_eq!(closest("tmorow", true, 6, t), Some("tomorrow".into()));
        // Between words equally near, the commoner wins: "meting" is "meeting", not "mating".
        let m = [("mating", 2), ("meeting", 40)];
        assert_eq!(closest("meting", true, 3, m), Some("meeting".into()));
        assert_eq!(closest("buddyy", true, 6, words), Some("buddy".into()));
    }
}
