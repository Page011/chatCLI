//! Desktop notifications for new messages (issue 24): the decision, the 10 s per-chat fold and
//! the toast text. Pure: the clock is a parameter, nothing here talks to the OS. The app crate
//! owns the Windows toast (`Notifier`); the TUI only hands it a `Toast`.
//!
//! A toast comes only from `Event::Message` (a message that just arrived), never from history,
//! never from a timer that calls a network.

use chatcli_core::settings::NotifyPreview;

use crate::text::{clean_line, truncate};

/// At most one toast per chat in this time; the rest fold into `N new messages`.
pub const FOLD_MS: i64 = 10_000;
/// A message older than the start of the TUI by more than this is catch-up, not news.
pub const STALE_MS: i64 = 60_000;
/// Longest toast body: a lock screen is no place for a long message.
const BODY_MAX: usize = 120;

/// Everything the decision needs about one arrived message.
#[derive(Debug, Clone, Copy)]
pub struct Arrived {
    pub from_me: bool,
    /// The message's own time (network clock).
    pub ts: i64,
    /// When the TUI started (clock ms).
    pub ui_start_ms: i64,
    pub enabled: bool,
    pub muted: bool,
    /// The terminal window has focus. False until a focus event says otherwise.
    pub focused: bool,
    /// The message's chat is the one on screen.
    pub chat_on_screen: bool,
}

/// Why no toast, for tests and for anyone reading a log of decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Toast,
    Disabled,
    Own,
    Stale,
    Muted,
    Watching,
}

/// Should this message raise a toast? Checked in this order, so the reason is the first one that
/// applies.
pub fn decide(a: &Arrived) -> Verdict {
    if !a.enabled {
        Verdict::Disabled
    } else if a.from_me {
        Verdict::Own
    } else if a.ts < a.ui_start_ms - STALE_MS {
        Verdict::Stale
    } else if a.muted {
        Verdict::Muted
    } else if a.focused && a.chat_on_screen {
        // You are looking at it.
        Verdict::Watching
    } else {
        Verdict::Toast
    }
}

/// Toast title and body for one message. `preview` decides how much of the message leaves the
/// process: `Off` never sends sender or text (only the chat title, which the issue allows).
pub fn toast_text(
    preview: NotifyPreview,
    chat_title: &str,
    sender: &str,
    text: &str,
) -> (String, String) {
    let title = truncate(&clean_line(chat_title), 60);
    let body = match preview {
        NotifyPreview::Off => "New message".to_string(),
        NotifyPreview::Sender => format!("{}: new message", truncate(&clean_line(sender), 40)),
        NotifyPreview::Full => {
            let one_line = clean_line(text);
            if one_line.trim().is_empty() {
                "New message".to_string()
            } else {
                truncate(
                    &format!("{}: {}", truncate(&clean_line(sender), 40), one_line),
                    BODY_MAX,
                )
            }
        }
    };
    (title, body)
}

/// `1 new message`, `3 new messages`.
pub fn folded_body(n: u32) -> String {
    if n == 1 {
        "1 new message".to_string()
    } else {
        format!("{n} new messages")
    }
}

/// Per-chat bookkeeping for the fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// When this chat last got a toast (clock ms).
    pub last_ms: i64,
    /// Messages held back since.
    pub pending: u32,
    /// Chat title for the folded toast, as of the last held-back message.
    pub title: String,
}

/// What `admit` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Show this message's own toast now.
    Now,
    /// Held back: `due` (clock ms) is when the folded toast is shown.
    Held { due: i64 },
}

/// Applies the 10 s rule to one eligible message of a chat.
pub fn admit(slot: &mut Option<Fold>, title: &str, now_ms: i64) -> Admit {
    match slot {
        Some(f) if f.pending > 0 || now_ms - f.last_ms < FOLD_MS => {
            f.pending += 1;
            f.title = title.to_string();
            Admit::Held {
                due: f.last_ms + FOLD_MS,
            }
        }
        _ => {
            *slot = Some(Fold {
                last_ms: now_ms,
                pending: 0,
                title: title.to_string(),
            });
            Admit::Now
        }
    }
}

/// The folded toast for a chat whose window ended, if one is due: `(title, body)`. Resets the
/// fold so the next message gets its own toast after another 10 s.
pub fn take_due(slot: &mut Fold, now_ms: i64) -> Option<(String, String)> {
    if slot.pending == 0 || now_ms < slot.last_ms + FOLD_MS {
        return None;
    }
    let out = (slot.title.clone(), folded_body(slot.pending));
    slot.pending = 0;
    slot.last_ms = now_ms;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Arrived {
        Arrived {
            from_me: false,
            ts: 1_000_000,
            ui_start_ms: 1_000_000,
            enabled: true,
            muted: false,
            focused: false,
            chat_on_screen: false,
        }
    }

    #[test]
    fn a_background_chat_while_unfocused_gives_a_toast() {
        assert_eq!(decide(&base()), Verdict::Toast);
    }

    #[test]
    fn open_and_focused_gives_none_but_open_and_unfocused_does() {
        let open = Arrived {
            chat_on_screen: true,
            ..base()
        };
        assert_eq!(
            decide(&Arrived {
                focused: true,
                ..open
            }),
            Verdict::Watching
        );
        assert_eq!(decide(&open), Verdict::Toast, "terminal in the background");
        let other_chat = Arrived {
            focused: true,
            ..base()
        };
        assert_eq!(decide(&other_chat), Verdict::Toast, "a chat you are not in");
    }

    #[test]
    fn own_stale_muted_and_disabled_give_none() {
        assert_eq!(
            decide(&Arrived {
                from_me: true,
                ..base()
            }),
            Verdict::Own
        );
        assert_eq!(
            decide(&Arrived {
                muted: true,
                ..base()
            }),
            Verdict::Muted
        );
        assert_eq!(
            decide(&Arrived {
                enabled: false,
                ..base()
            }),
            Verdict::Disabled
        );
        let a = base();
        let just_fresh = Arrived {
            ts: a.ui_start_ms - STALE_MS,
            ..a
        };
        assert_eq!(
            decide(&just_fresh),
            Verdict::Toast,
            "a minute old still counts"
        );
        let stale = Arrived {
            ts: a.ui_start_ms - STALE_MS - 1,
            ..a
        };
        assert_eq!(
            decide(&stale),
            Verdict::Stale,
            "catch-up of old messages is not news"
        );
    }

    #[test]
    fn preview_off_puts_no_message_text_or_sender_in_the_toast() {
        let (title, body) = toast_text(
            NotifyPreview::Off,
            "Alex Example",
            "Sam Sample",
            "secret plan",
        );
        assert_eq!(
            (title.as_str(), body.as_str()),
            ("Alex Example", "New message")
        );
        assert!(!body.contains("secret") && !body.contains("Sam"));
    }

    #[test]
    fn preview_sender_names_the_sender_only() {
        let (_, body) = toast_text(
            NotifyPreview::Sender,
            "Project Team",
            "Sam Sample",
            "secret plan",
        );
        assert_eq!(body, "Sam Sample: new message");
    }

    #[test]
    fn preview_full_is_one_clean_line_and_cut() {
        let (_, body) = toast_text(
            NotifyPreview::Full,
            "Project Team",
            "Sam Sample",
            "line one\nline two\u{1b}[2J",
        );
        assert!(!body.contains('\n') && !body.contains('\u{1b}'), "{body:?}");
        assert!(body.starts_with("Sam Sample: line one"));
        let long = "x".repeat(500);
        let (_, body) = toast_text(NotifyPreview::Full, "t", "s", &long);
        assert!(body.chars().count() <= BODY_MAX);
        // An attachment-only or empty message has no text to show.
        let (_, body) = toast_text(NotifyPreview::Full, "t", "s", "  ");
        assert_eq!(body, "New message");
    }

    #[test]
    fn a_hostile_title_is_one_clean_line() {
        let (title, _) = toast_text(NotifyPreview::Off, "Evil\u{1b}[2JName\nnext", "s", "t");
        assert!(!title.contains('\u{1b}') && !title.contains('\n'));
    }

    #[test]
    fn one_toast_per_chat_per_ten_seconds_the_rest_fold() {
        let mut slot = None;
        assert_eq!(admit(&mut slot, "A", 1_000), Admit::Now);
        assert_eq!(
            admit(&mut slot, "A", 2_000),
            Admit::Held {
                due: 1_000 + FOLD_MS
            }
        );
        assert_eq!(
            admit(&mut slot, "A", 5_000),
            Admit::Held {
                due: 1_000 + FOLD_MS
            }
        );
        let f = slot.as_mut().unwrap();
        assert_eq!(
            take_due(f, 1_000 + FOLD_MS - 1),
            None,
            "not before the window ends"
        );
        assert_eq!(
            take_due(f, 1_000 + FOLD_MS),
            Some(("A".to_string(), "2 new messages".to_string()))
        );
        assert_eq!(take_due(f, 99_999), None, "nothing more is held");
        // The folded toast counted as this chat's toast: the next message waits again.
        assert_eq!(
            admit(&mut slot, "A", 1_000 + FOLD_MS + 1),
            Admit::Held {
                due: 1_000 + 2 * FOLD_MS
            }
        );
    }

    #[test]
    fn a_message_after_a_quiet_window_gets_its_own_toast() {
        let mut slot = None;
        assert_eq!(admit(&mut slot, "A", 0), Admit::Now);
        assert_eq!(admit(&mut slot, "A", FOLD_MS), Admit::Now);
        assert_eq!(folded_body(1), "1 new message");
    }
}
