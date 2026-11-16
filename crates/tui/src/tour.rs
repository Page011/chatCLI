//! The guided tour (`chatcli --demo --tour`, or `/tour` in the demo): a script of steps, each a
//! caption plus the keys the tour presses for you. Pure data and a small state machine; `App`
//! (app/tour.rs) plays it through the same key handler as typed keys.
//!
//! Keys only, with three tour-only acts (`Row`, `SelectMsg`, `ToList`) that go straight to a
//! place instead of pressing a fixed number of keys, so reordering the settings rows or the demo
//! messages cannot break the tour.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::keymap::{Action, Context};
use crate::settings_view::RowId;
use chatcli_core::model::Network;

/// What an `Until` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cond {
    Online(Network),
    /// The open chat of the active network has messages.
    ChatLoaded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    Key(KeyCode, KeyModifiers),
    /// Typed one character at a time.
    Type(&'static str),
    /// `/`, the name, a pause, Enter: a command through the command bar.
    Cmd(&'static str),
    Wait(u32),
    /// Holds until the condition is true, at most `UNTIL_MAX_MS`.
    Until(Cond),
    /// Settings page: the cursor goes to this row.
    Row(RowId),
    /// Message selection: the highlighted message is the newest whose text starts with this.
    SelectMsg(&'static str),
    /// Back to the chat list: overlays closed, both tabs on their list.
    ToList,
}

pub(crate) struct Step {
    pub caption: &'static str,
    pub acts: &'static [Act],
}

/// Time to read a caption before its keys start, and after its keys end.
pub(crate) const LEAD_MS: i64 = 700;
pub(crate) const STEP_MS: i64 = 2000;
/// Between two key presses.
pub(crate) const KEY_GAP_MS: i64 = 120;
pub(crate) const UNTIL_MAX_MS: i64 = 8000;
pub(crate) const UNTIL_POLL_MS: i64 = 250;
/// After `/`, and after the command name, so each can be seen.
const CMD_OPEN_MS: u32 = 400;
const CMD_NAME_MS: u32 = 700;

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

const fn key(code: KeyCode) -> Act {
    Act::Key(code, NONE)
}

const fn ctrl(code: KeyCode) -> Act {
    Act::Key(code, CTRL)
}

const fn ch(c: char) -> Act {
    Act::Key(KeyCode::Char(c), NONE)
}

const ENTER: Act = key(KeyCode::Enter);
const ESC: Act = key(KeyCode::Esc);
const TAB: Act = key(KeyCode::Tab);
const UP: Act = key(KeyCode::Up);
const DOWN: Act = key(KeyCode::Down);
const LEFT: Act = key(KeyCode::Left);
const RIGHT: Act = key(KeyCode::Right);

const REFRESH_ROW: RowId = RowId::KeyAction(Context::List, Action::Refresh);

pub(crate) const STEPS: &[Step] = &[
    Step {
        caption: "Welcome to the chatcli demo: fake chats, nothing is sent.",
        acts: &[Act::Wait(1000)],
    },
    Step {
        caption: "Tab switches network: Teams and WhatsApp.",
        acts: &[TAB, Act::Wait(1500), TAB],
    },
    Step {
        caption: ": opens the command bar. :signin signs in (demo: no browser).",
        acts: &[Act::Cmd("signin"), Act::Until(Cond::Online(Network::Teams))],
    },
    Step {
        caption: "Down moves through chats; the right pane previews each one.",
        acts: &[
            key(KeyCode::Home),
            DOWN,
            Act::Wait(900),
            DOWN,
            Act::Wait(900),
            key(KeyCode::Home),
        ],
    },
    Step {
        caption: "i opens the chat's message box (Enter goes into the chat to select messages).",
        acts: &[ch('i'), Act::Until(Cond::ChatLoaded)],
    },
    Step {
        caption: "Type a message.",
        acts: &[Act::Type("Hello from the tour ")],
    },
    Step {
        caption: "Emoji shortcode: type :thu, then Enter inserts the emoji.",
        acts: &[Act::Type(":thu"), Act::Wait(900), ENTER],
    },
    Step {
        caption: "Enter sends.",
        acts: &[ENTER],
    },
    Step {
        caption: "Ctrl+E opens the emoji picker.",
        acts: &[
            ctrl(KeyCode::Char('e')),
            Act::Wait(900),
            RIGHT,
            RIGHT,
            Act::Wait(900),
            ESC,
        ],
    },
    Step {
        caption: "Esc from the box selects a message; k and j move, i goes back to the box.",
        acts: &[ESC, Act::Wait(600), ch('k'), ch('k')],
    },
    Step {
        caption: "r replies with a quote.",
        acts: &[ch('r'), Act::Type("Quoted reply"), ENTER],
    },
    Step {
        caption: "e reacts to the selected message.",
        acts: &[ESC, UP, ch('e'), Act::Wait(900), ENTER],
    },
    Step {
        caption: "c changes (edits) your own message.",
        // Still selecting after the reaction.
        acts: &[
            ch('c'),
            ctrl(KeyCode::Char('u')),
            Act::Type("Edited by the tour"),
            ENTER,
        ],
    },
    Step {
        caption: "d (or x) deletes your message for everyone, after y/n.",
        acts: &[ESC, ch('d'), Act::Wait(1200), ch('y')],
    },
    Step {
        caption: "Attachments show as chips; o opens one in its app. Esc goes back to the chats.",
        acts: &[Act::SelectMsg("[image] sunset"), Act::Wait(2500), ESC],
    },
    Step {
        caption: "Ctrl+F searches every message stored on this PC.",
        acts: &[
            ctrl(KeyCode::Char('f')),
            Act::Type("draft"),
            Act::Wait(1500),
            ESC,
        ],
    },
    Step {
        caption: "p pins the selected chat, m mutes it. Again undoes.",
        acts: &[
            Act::ToList,
            ch('p'),
            Act::Wait(1000),
            ch('m'),
            Act::Wait(1000),
            ch('p'),
            ch('m'),
        ],
    },
    Step {
        caption: "/ filters the chat list; Esc clears the filter.",
        acts: &[
            Act::ToList,
            ch('/'),
            Act::Wait(900),
            Act::Type("sam"),
            Act::Wait(1500),
            ESC,
        ],
    },
    Step {
        caption: ": lists every command; typing filters commands and chats.",
        acts: &[
            ch(':'),
            Act::Wait(1200),
            Act::Type("se"),
            Act::Wait(1500),
            ESC,
        ],
    },
    Step {
        caption: ":settings: changes apply at once and save (demo: temp copy).",
        acts: &[Act::Cmd("settings"), Act::Wait(1200)],
    },
    Step {
        caption: "Right cycles the theme; the whole screen follows.",
        acts: &[
            Act::Row(RowId::ThemeName),
            RIGHT,
            Act::Wait(1200),
            RIGHT,
            Act::Wait(1200),
            LEFT,
            LEFT,
            Act::Wait(600),
            ESC,
        ],
    },
    Step {
        caption: ":layout: density compact fits more chats.",
        acts: &[
            Act::Cmd("layout"),
            Act::Row(RowId::Density),
            RIGHT,
            // Leave the page so the compact chat list shows, then put it back.
            ESC,
            Act::Wait(1800),
            Act::Cmd("layout"),
            Act::Row(RowId::Density),
            LEFT,
            Act::Wait(600),
            ESC,
        ],
    },
    Step {
        caption: ":keys: Enter on an action, then press the new key.",
        acts: &[
            Act::Cmd("keys"),
            Act::Row(REFRESH_ROW),
            ENTER,
            Act::Wait(1200),
            key(KeyCode::F(5)),
            Act::Wait(1500),
            ESC,
        ],
    },
    Step {
        caption: "The list footer shows F5 refresh now. r resets the key.",
        acts: &[
            Act::Wait(1500),
            Act::Cmd("keys"),
            Act::Row(REFRESH_ROW),
            ch('r'),
            Act::Wait(1200),
            ESC,
        ],
    },
    Step {
        caption: "? opens help: commands first, then every key.",
        acts: &[
            Act::ToList,
            ch('?'),
            Act::Wait(1000),
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            Act::Wait(1000),
            ESC,
        ],
    },
    Step {
        caption: "Tour done. Press : for commands. Ctrl+C quits.",
        acts: &[],
    },
];

/// One thing to do, after `Act`s are unrolled (a `Type` is one `Key` per character).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prim {
    Key(KeyCode, KeyModifiers),
    Wait(i64),
    Until(Cond),
    Row(RowId),
    SelectMsg(&'static str),
    ToList,
    /// The step is over: the next one starts.
    End,
    /// The last step: nothing more to play.
    Done,
}

/// The prims of step `i`: lead-in, its acts, the pause after, then End (or Done on the last).
pub(crate) fn unroll(i: usize) -> VecDeque<Prim> {
    let mut q = VecDeque::new();
    let Some(step) = STEPS.get(i) else {
        q.push_back(Prim::Done);
        return q;
    };
    if i + 1 == STEPS.len() {
        q.push_back(Prim::Done);
        return q;
    }
    q.push_back(Prim::Wait(LEAD_MS));
    for act in step.acts {
        match *act {
            Act::Key(c, m) => {
                q.push_back(Prim::Key(c, m));
                q.push_back(Prim::Wait(KEY_GAP_MS));
            }
            Act::Type(s) => {
                for c in s.chars() {
                    q.push_back(Prim::Key(KeyCode::Char(c), NONE));
                    q.push_back(Prim::Wait(KEY_GAP_MS));
                }
            }
            Act::Cmd(name) => {
                q.push_back(Prim::Key(KeyCode::Char(':'), NONE));
                q.push_back(Prim::Wait(i64::from(CMD_OPEN_MS)));
                for c in name.chars() {
                    q.push_back(Prim::Key(KeyCode::Char(c), NONE));
                    q.push_back(Prim::Wait(KEY_GAP_MS));
                }
                q.push_back(Prim::Wait(i64::from(CMD_NAME_MS)));
                q.push_back(Prim::Key(KeyCode::Enter, NONE));
                q.push_back(Prim::Wait(KEY_GAP_MS));
            }
            Act::Wait(ms) => q.push_back(Prim::Wait(i64::from(ms))),
            Act::Until(c) => q.push_back(Prim::Until(c)),
            Act::Row(id) => {
                q.push_back(Prim::Row(id));
                q.push_back(Prim::Wait(KEY_GAP_MS));
            }
            Act::SelectMsg(text) => {
                q.push_back(Prim::SelectMsg(text));
                q.push_back(Prim::Wait(KEY_GAP_MS));
            }
            Act::ToList => q.push_back(Prim::ToList),
        }
    }
    q.push_back(Prim::Wait(STEP_MS));
    q.push_back(Prim::End);
    q
}

/// Where the tour is.
#[derive(Debug, Clone)]
pub(crate) struct Tour {
    pub step: usize,
    pub queue: VecDeque<Prim>,
    /// Clock time (ms) of the next prim.
    pub due: i64,
    pub paused: bool,
    /// `n`: the rest of this step runs without pauses.
    pub fast: bool,
    /// When the current `Until` started waiting.
    pub until_since: Option<i64>,
    pub done: bool,
    /// The user's keymap and `[keys]` settings, put back when the tour ends: the tour itself
    /// plays on plain default keys.
    pub saved: Option<Box<(crate::keymap::Keymap, chatcli_core::settings::KeySettings)>>,
}

impl Tour {
    pub fn new(now: i64) -> Tour {
        Tour {
            step: 0,
            queue: unroll(0),
            due: now,
            paused: false,
            fast: false,
            until_since: None,
            done: false,
            saved: None,
        }
    }

    pub fn caption(&self) -> &'static str {
        STEPS.get(self.step).map_or("", |s| s.caption)
    }

    /// The caption bar's left text.
    pub fn bar_text(&self) -> String {
        let (n, total) = (self.step + 1, STEPS.len());
        if self.paused {
            format!("TOUR {n}/{total} PAUSED")
        } else {
            format!("TOUR {n}/{total} - {}", self.caption())
        }
    }

    /// The caption bar's key hint, shown right-aligned when it fits.
    pub fn bar_hint(&self) -> &'static str {
        if self.done {
            "Any key to leave the tour"
        } else if self.paused {
            "Space resume  n next  Esc end"
        } else {
            "Space pause  n next  Esc end"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_has_a_caption_and_the_counter_fits_40_columns() {
        for (i, s) in STEPS.iter().enumerate() {
            assert!(!s.caption.is_empty(), "step {} has no caption", i + 1);
        }
        let t = Tour::new(0);
        assert!("TOUR 1/26 - ".len() < 40);
        assert!(t.bar_text().starts_with("TOUR 1/26 - Welcome"));
        assert_eq!(STEPS.len(), 26);
    }

    #[test]
    fn unrolled_steps_end_with_end_and_the_last_with_done() {
        for i in 0..STEPS.len() - 1 {
            assert_eq!(unroll(i).back(), Some(&Prim::End), "step {}", i + 1);
        }
        assert_eq!(unroll(STEPS.len() - 1).back(), Some(&Prim::Done));
    }

    #[test]
    fn a_command_is_colon_name_enter() {
        let keys: Vec<KeyCode> = unroll(2)
            .iter()
            .filter_map(|p| match p {
                Prim::Key(c, _) => Some(*c),
                _ => None,
            })
            .collect();
        let text: String = keys
            .iter()
            .map(|c| match c {
                KeyCode::Char(c) => *c,
                KeyCode::Enter => '\n',
                _ => '?',
            })
            .collect();
        assert_eq!(text, ":signin\n");
    }

    #[test]
    fn captions_name_only_real_commands() {
        for s in STEPS {
            for a in s.acts {
                if let Act::Cmd(name) = a {
                    assert!(
                        crate::commands::COMMANDS.iter().any(|c| c.name == *name),
                        ":{name}"
                    );
                }
            }
        }
    }
}
