//! Paste that arrives as key presses.
//!
//! crossterm never reports a paste on Windows (no `Event::Paste` there): Windows Terminal and
//! the classic console hand the pasted text over as one key event per character, and every line
//! break is the Enter key, which is "send". Dropping a file onto the window is the same: its path
//! is typed. A human cannot queue two key presses in the microseconds between two reads, so
//! several presses that are already waiting together are a paste. The input loop collects them
//! and this module decides what they are. Pure, so every case is unit tested.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::keymap::typed_char;

/// Most events one burst takes. A paste larger than this arrives as several bursts, each one
/// still a paste.
pub const BURST_MAX: usize = 200_000;

/// Outside a text box a burst this long is a paste, not typing: it is dropped, because the same
/// text as keys would run list actions (`q` quits, `x` hides a chat).
const SWALLOW_AT: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Burst {
    /// Ordinary keys: handle each one.
    Keys,
    /// Pasted text, line breaks as `\r`, `\n` or `\r\n` (`clean_paste` folds them).
    Paste(String),
    /// A paste while no text box is open: nothing happens.
    Ignore,
}

/// The text a pasted key stands for, or None for a key that is not text.
fn pasted_char(key: &KeyEvent) -> Option<char> {
    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => Some('\r'),
        (KeyCode::Tab, KeyModifiers::NONE) => Some('\t'),
        // A line feed is Ctrl+J in a console key record.
        (KeyCode::Char('j'), KeyModifiers::CONTROL) => Some('\n'),
        _ => typed_char(key),
    }
}

/// Decides what `keys` (everything that was waiting at once) are. `text_box` is true when a
/// message box, filter, picker, path prompt, search or `:` box is open to take text.
pub fn classify(keys: &[KeyEvent], text_box: bool) -> Burst {
    let presses: Vec<&KeyEvent> = keys
        .iter()
        .filter(|k| k.kind != KeyEventKind::Release)
        .collect();
    if presses.len() < 2 {
        return Burst::Keys;
    }
    // One key that is not text (Esc, an arrow, a Ctrl chord) and it is not a paste.
    let Some(text) = presses
        .iter()
        .map(|k| pasted_char(k))
        .collect::<Option<String>>()
    else {
        return Burst::Keys;
    };
    if text_box {
        Burst::Paste(text)
    } else if presses.len() >= SWALLOW_AT {
        Burst::Ignore
    } else {
        Burst::Keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn text_keys(s: &str) -> Vec<KeyEvent> {
        s.chars()
            .map(|c| match c {
                '\n' => key(KeyCode::Enter, KeyModifiers::NONE),
                c => key(KeyCode::Char(c), KeyModifiers::NONE),
            })
            .collect()
    }

    #[test]
    fn three_pasted_lines_become_one_paste_never_three_sends() {
        let keys = text_keys("one\ntwo\nthree");
        let Burst::Paste(text) = classify(&keys, true) else {
            panic!("a block of keys with Enter is a paste");
        };
        // The Enters are line breaks in the text, not keys.
        assert_eq!(crate::text::clean_paste(&text), "one\ntwo\nthree");
    }

    #[test]
    fn the_key_up_events_of_a_paste_do_not_count_or_break_it() {
        let mut keys = Vec::new();
        for k in text_keys("a\nb") {
            keys.push(k);
            keys.push(KeyEvent {
                kind: KeyEventKind::Release,
                ..k
            });
        }
        assert_eq!(
            classify(&keys, true),
            Burst::Paste("a\rb".to_string()),
            "releases are skipped"
        );
    }

    #[test]
    fn a_crlf_paste_is_one_line_break() {
        let keys = [
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
        ];
        let Burst::Paste(text) = classify(&keys, true) else {
            panic!()
        };
        assert_eq!(crate::text::clean_paste(&text), "a\nb");
    }

    #[test]
    fn one_key_and_a_non_text_key_are_ordinary_typing() {
        let a = key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(classify(&[a], true), Burst::Keys);
        assert_eq!(classify(&[], true), Burst::Keys);
        // An Esc or an arrow in the burst: handled key by key, in order.
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(classify(&[a, esc, a], true), Burst::Keys);
        let left = key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(classify(&[a, left], true), Burst::Keys);
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(classify(&[a, ctrl_c], true), Burst::Keys);
        // Alt+Enter and Shift+Enter are the newline keys, not pasted Enters.
        let alt_enter = key(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(classify(&[a, alt_enter], true), Burst::Keys);
    }

    #[test]
    fn altgr_characters_and_tabs_are_text() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let keys = [
            key(KeyCode::Char('@'), altgr),
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            key(KeyCode::Tab, KeyModifiers::NONE),
        ];
        assert_eq!(classify(&keys, true), Burst::Paste("@x\t".to_string()));
    }

    #[test]
    fn outside_a_text_box_a_long_burst_is_dropped_and_a_short_one_is_typing() {
        let long = text_keys("q\nqx");
        assert_eq!(classify(&long, false), Burst::Ignore);
        let short = text_keys("jj");
        assert_eq!(classify(&short, false), Burst::Keys);
    }
}
