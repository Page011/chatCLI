//! Screen snapshots: each scenario is rendered into a TestBackend and written as plain text
//! (the screen, then a list of styled runs so colour and reverse video are visible too).
//! A changed screen fails the test. `UPDATE_SNAPSHOTS=1 cargo test -p chatcli-tui` rewrites
//! the files under tests/snapshots; read the diff before keeping it.
//!
//! Time is fixed: "now" is Monday 2026-11-16 09:30 in a +10:00 zone.

use std::path::PathBuf;

use chatcli_core::model::{
    ChatKind, ChatMessage, ChatSummary, Event, MessageState, Network, Status,
};
use chatcli_tui::{draw, App, Clock};
use chrono::{FixedOffset, TimeZone};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::text::Span;
use ratatui::Terminal;

const SAMPLE_QR: &str = "2@sample-payload-not-a-real-link-code,AAAA,BBBB,CCCC";

fn zone() -> FixedOffset {
    FixedOffset::east_opt(10 * 3600).unwrap()
}

fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    zone()
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

fn new_app() -> App {
    App::new(Clock {
        now_ms: ts(2026, 11, 16, 9, 30),
        tz: Some(zone()),
    })
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn chat(
    id: &str,
    title: &str,
    kind: ChatKind,
    last: i64,
    preview: &str,
    unread: u32,
) -> ChatSummary {
    ChatSummary {
        network: Network::Teams,
        id: id.into(),
        title: title.into(),
        kind,
        last_ts: Some(last),
        last_preview: preview.into(),
        unread,
    }
}

fn teams_chats() -> Vec<ChatSummary> {
    vec![
        chat(
            "alex",
            "Alex Example",
            ChatKind::Direct,
            ts(2026, 11, 16, 9, 15),
            "Alex Example: is the build done?",
            2,
        ),
        chat(
            "proj",
            "Project Team",
            ChatKind::Group,
            ts(2026, 11, 16, 8, 2),
            "Sam Sample: thanks",
            100,
        ),
        chat(
            "sam",
            "Sam Sample",
            ChatKind::Direct,
            ts(2026, 11, 15, 17, 40),
            "You: see you tomorrow",
            1,
        ),
        chat(
            "rel",
            "Release Planning for the Autumn Quarter Kickoff",
            ChatKind::Group,
            ts(2026, 11, 13, 11, 0),
            "Jordan Placeholder: agenda attached to the invite",
            0,
        ),
        chat(
            "cjk",
            "\u{7530}\u{4e2d}\u{592a}\u{90ce} (Taro) \u{1F389}",
            ChatKind::Direct,
            ts(2026, 11, 5, 14, 20),
            "\u{4e86}\u{89e3}\u{3057}\u{307e}\u{3057}\u{305f}",
            0,
        ),
        chat(
            "jor",
            "Jordan Placeholder",
            ChatKind::Direct,
            ts(2026, 10, 9, 9, 0),
            "You: thanks",
            0,
        ),
    ]
}

fn msg(id: &str, at: i64, from_me: bool, sender: &str, text: &str) -> ChatMessage {
    ChatMessage {
        network: Network::Teams,
        chat_id: "alex".into(),
        id: id.into(),
        ts: at,
        sender: sender.into(),
        from_me,
        text: text.into(),
        state: MessageState::Sent,
    }
}

fn alex_history() -> Vec<ChatMessage> {
    vec![
        msg("m1", ts(2026, 11, 5, 14, 20), false, "Alex Example", "Can you review the draft before Friday?"),
        msg("m2", ts(2026, 11, 5, 14, 31), true, "You", "Will do."),
        msg(
            "m3",
            ts(2026, 11, 15, 16, 5),
            false,
            "Alex Example",
            "Notes from the review: the summary page needs a shorter introduction, the figures need captions, and the appendix can move to the end of the document.",
        ),
        msg("m4", ts(2026, 11, 16, 9, 10), false, "Alex Example", "is the build done? \u{1F680} \u{4eca}\u{65e5}\u{306f}\u{3069}\u{3046}\u{3067}\u{3059}\u{304b}"),
        msg("m5", ts(2026, 11, 16, 9, 15), true, "You", "yes, shipped"),
    ]
}

/// Teams signed in with chats and a chat open, one message pending and one failed.
fn open_chat_app() -> App {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::NeedsLogin { reason: None },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    press(&mut app, KeyCode::Enter);
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: alex_history(),
    });
    type_str(&mut app, "on it, will confirm shortly");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "this one will fail");
    press(&mut app, KeyCode::Enter);
    app.handle_event(Event::MessageFailed {
        network: Network::Teams,
        chat_id: "alex".into(),
        client_id: format!("local-{}-2", ts(2026, 11, 16, 9, 30)),
        error: "demo: this chat refuses messages".into(),
    });
    type_str(&mut app, "typing a draft");
    app
}

fn dump(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let area = buf.area;
    let mut screen = String::new();
    let mut styles = String::new();
    for y in 0..area.height {
        let mut row = String::new();
        let mut skip = 0usize;
        let mut last_key: Option<String> = None;
        let mut run: Option<(u16, String)> = None;
        let mut runs: Vec<(u16, u16, String)> = Vec::new();
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            for c in [cell.fg, cell.bg] {
                assert!(
                    !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                    "only the 16 named ANSI colours are allowed, found {c:?} at {x},{y}"
                );
            }
            let key = if skip > 0 {
                // Second half of a wide character: ratatui leaves it unstyled, the terminal
                // draws it with the first half, so it continues the previous run.
                skip -= 1;
                last_key.clone()
            } else {
                row.push_str(cell.symbol());
                skip = Span::raw(cell.symbol()).width().saturating_sub(1);
                (cell.fg != Color::Reset || cell.bg != Color::Reset || !cell.modifier.is_empty())
                    .then(|| format!("fg={:?} bg={:?} {:?}", cell.fg, cell.bg, cell.modifier))
            };
            last_key = key.clone();
            match (run.take(), key) {
                (Some((start, k)), Some(nk)) if k == nk => run = Some((start, k)),
                (Some((start, k)), next) => {
                    runs.push((start, x - 1, k));
                    run = next.map(|nk| (x, nk));
                }
                (None, Some(nk)) => run = Some((x, nk)),
                (None, None) => {}
            }
        }
        if let Some((start, k)) = run {
            runs.push((start, area.width - 1, k));
        }
        screen.push_str(row.trim_end());
        screen.push('\n');
        for (a, b, k) in runs {
            styles.push_str(&format!("row {y} cols {a}-{b}: {k}\n"));
        }
    }
    format!("{screen}--- styled runs (everything else is default style) ---\n{styles}")
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    app.resize(w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    dump(&term)
}

fn check(name: &str, w: u16, h: u16, got: &str) {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "snapshots",
        &format!("{name}_{w}x{h}.txt"),
    ]
    .iter()
    .collect();
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            panic!(
                "missing snapshot {}: run with UPDATE_SNAPSHOTS=1 and read it",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    assert!(
        want == got,
        "snapshot {} differs.\n--- want ---\n{want}\n--- got ---\n{got}",
        path.display()
    );
}

fn both_sizes(name: &str, mut build: impl FnMut() -> App) {
    for (w, h) in [(100, 30), (80, 24)] {
        let mut app = build();
        let got = render(&mut app, w, h);
        check(name, w, h, &got);
    }
}

fn qr_app() -> App {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::Qr {
            code: SAMPLE_QR.into(),
        },
    ));
    press(&mut app, KeyCode::Tab);
    app
}

#[test]
fn teams_sign_in() {
    both_sizes("teams_sign_in", || {
        let mut app = new_app();
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        app.handle_event(Event::Status(
            Network::WhatsApp,
            Status::NeedsLogin { reason: None },
        ));
        app
    });
}

#[test]
fn whatsapp_qr() {
    both_sizes("whatsapp_qr", qr_app);
}

#[test]
fn chat_list_and_open_chat() {
    both_sizes("chat_list_and_open_chat", open_chat_app);
}

#[test]
fn filter_no_matches() {
    both_sizes("filter_no_matches", || {
        let mut app = new_app();
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, teams_chats()));
        press(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "zzz");
        app
    });
}

fn error_app() -> App {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Error {
            message: "Teams refused the saved sign-in: run chatcli teams login.".into(),
        },
    ));
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::Online {
            live: false,
            detail: Some("phone is offline".into()),
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    app.handle_event(Event::Notice {
        network: Network::Teams,
        text: "Loaded 6 chats from the local copy.".into(),
        is_error: false,
    });
    app.handle_event(Event::Notice {
        network: Network::Teams,
        text: "Could not reach the relay: connection refused. Check relay_url in config.json."
            .into(),
        is_error: true,
    });
    app
}

#[test]
fn error_notice() {
    both_sizes("error_notice", error_app);
}

fn status_app(teams: Status, whatsapp: Status) -> App {
    let mut app = new_app();
    app.handle_event(Event::Status(Network::Teams, teams));
    app.handle_event(Event::Status(Network::WhatsApp, whatsapp));
    app
}

#[test]
fn signing_in_and_linking_panels() {
    both_sizes("signing_in_panel", || {
        status_app(Status::LoggingIn, Status::LoggingIn)
    });
}

#[test]
fn sign_in_needed_with_reason() {
    both_sizes("sign_in_needed_reason", || {
        let mut app = status_app(
            Status::NeedsLogin {
                reason: Some("Teams sign-in was rejected. Sign in again.".into()),
            },
            Status::NeedsLogin {
                reason: Some("WhatsApp unlinked this device. Press Enter to link again.".into()),
            },
        );
        press(&mut app, KeyCode::Tab);
        app
    });
}

#[test]
fn empty_and_loading_lists() {
    both_sizes("empty_list", || {
        status_app(
            Status::Online {
                live: true,
                detail: None,
            },
            Status::Connecting,
        )
    });
    both_sizes("loading_list", || {
        status_app(Status::Connecting, Status::Connecting)
    });
}

#[test]
fn whatsapp_not_live_phone_offline() {
    both_sizes("whatsapp_not_live", || {
        let mut app = status_app(
            Status::Online {
                live: true,
                detail: None,
            },
            Status::Online {
                live: false,
                detail: Some("phone is offline".into()),
            },
        );
        press(&mut app, KeyCode::Tab);
        app
    });
}

#[test]
fn narrow_layout_list_and_open_conversation() {
    let mut app = open_chat_app();
    let list = render(&mut app, 50, 20);
    press(&mut app, KeyCode::Esc);
    let list_again = render(&mut app, 50, 20);
    assert_eq!(list_again.lines().next(), list.lines().next());
    check("narrow_convo", 50, 20, &list);
    check("narrow_list", 50, 20, &list_again);
}

#[test]
fn tiny_terminal() {
    let mut app = error_app();
    let got = render(&mut app, 40, 12);
    check("tiny_terminal", 40, 12, &got);
}

#[test]
fn qr_too_small_says_so() {
    let mut app = qr_app();
    let got = render(&mut app, 40, 12);
    // The sentence wraps at 40 columns, so check its two ends.
    assert!(
        got.contains("Make the window larger to show the QR")
            && got.contains("code (needs 37 x 25)"),
        "{got}"
    );
}

#[test]
fn below_minimum_size_does_not_panic() {
    let mut app = open_chat_app();
    let got = render(&mut app, 20, 5);
    assert!(got.contains("Terminal too small"), "{got}");
}

/// Every scenario at many sizes, wide chars and all: nothing may panic or index out of range.
#[test]
fn no_panic_at_any_size() {
    for build in [open_chat_app as fn() -> App, qr_app, error_app] {
        for w in (1..=120).step_by(7) {
            for h in (1..=45).step_by(4) {
                let mut app = build();
                render(&mut app, w, h);
            }
        }
    }
    // Narrow layout with the conversation showing.
    let mut app = open_chat_app();
    render(&mut app, 45, 20);
}
