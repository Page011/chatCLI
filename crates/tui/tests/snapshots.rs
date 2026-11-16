//! Screen snapshots: each scenario is rendered into a TestBackend and written as plain text
//! (the screen, then a list of styled runs so colour and reverse video are visible too).
//! A changed screen fails the test. `UPDATE_SNAPSHOTS=1 cargo test -p chatcli-tui` rewrites
//! the files under tests/snapshots; read the diff before keeping it.
//!
//! Time is fixed: "now" is Monday 2026-11-16 09:30 in a +10:00 zone.

use std::path::PathBuf;

use chatcli_core::model::{
    Capabilities, ChatKind, ChatMessage, ChatSummary, Event, MessageState, Network, PersonRef,
    QuotedRef, Reaction, Status,
};
use chatcli_core::settings::Settings;
use chatcli_tui::{draw, App, Clock};
use chrono::{FixedOffset, TimeZone};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
};
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

/// An app with the vim preset (as `preset = "vim"` or `--keys vim` would give).
fn new_vim_app() -> App {
    let mut s = Settings::default();
    s.keys.preset = "vim".into();
    new_app_with(s)
}

fn new_app_with(settings: Settings) -> App {
    App::with_settings(
        Clock {
            now_ms: ts(2026, 11, 16, 9, 30),
            tz: Some(zone()),
        },
        &settings,
        None,
    )
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
        ..ChatSummary::blank(Network::Teams)
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
        ..ChatMessage::blank(Network::Teams)
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
    open_chat_in(new_app())
}

fn open_chat_in(mut app: App) -> App {
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
    dump_with(term, false)
}

/// `allow_rgb`: thumbnails are the one place RGB (or a 256-palette index) is allowed.
fn dump_with(term: &Terminal<TestBackend>, allow_rgb: bool) -> String {
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
                    allow_rgb || !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
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
        // `/` opens the command bar: the plain filter needs a binding of its own.
        let mut settings = Settings::default();
        settings
            .keys
            .bindings
            .entry("list".into())
            .or_default()
            .insert("f".into(), "filter_start".into());
        settings.keys.lines.insert(("list".into(), "f".into()), 1);
        let mut app = new_app_with(settings);
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, teams_chats()));
        press(&mut app, KeyCode::Char('f'));
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

/// Cursor in the list on a chat whose messages are already held: the pane shows it, focus stays
/// in the list (double border on the sidebar), the hint says "Enter reply".
#[test]
fn list_focus_with_preview() {
    both_sizes("list_preview", || {
        let mut app = new_app();
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, teams_chats()));
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "alex".into(),
            messages: alex_history(),
        });
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Up);
        app
    });
}

/// Preview of a chat whose messages have not arrived yet.
#[test]
fn list_preview_loading() {
    both_sizes("list_preview_loading", || {
        let mut app = new_app();
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Chats(Network::Teams, teams_chats()));
        press(&mut app, KeyCode::Down);
        app
    });
}

fn online_in(mut app: App) -> App {
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: alex_history(),
    });
    app
}

fn press_mod(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    app.handle_key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

/// Vim Normal mode: `-- NORMAL --` at the left of the footer, the chat list in focus.
#[test]
fn vim_normal_mode() {
    both_sizes("vim_normal", || {
        let mut app = online_in(new_vim_app());
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        app
    });
}

/// Vim Insert mode: the message box has focus and the footer says so.
#[test]
fn vim_insert_mode() {
    both_sizes("vim_insert", || open_chat_in(new_vim_app()));
}

/// A held `g` shows in the footer next to the mode.
#[test]
fn vim_pending_g_is_echoed() {
    both_sizes("vim_pending_g", || {
        let mut app = online_in(new_vim_app());
        press(&mut app, KeyCode::Char('g'));
        app
    });
}

/// The `:` box above the footer, with some text typed.
#[test]
fn vim_command_box() {
    both_sizes("vim_command_box", || {
        let mut app = online_in(new_vim_app());
        press(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "keys default");
        app
    });
}

fn error_app_in(mut app: App) -> App {
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Error {
            message: "Teams refused the saved sign-in: run chatcli teams login.".into(),
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    app
}

/// The mode label must stay visible when a problem takes the only footer row.
#[test]
fn vim_mode_label_survives_a_one_row_footer() {
    let mut app = error_app_in(new_vim_app());
    let got = render(&mut app, 40, 12);
    let footer = got.lines().nth(11).unwrap();
    assert!(footer.starts_with("-- NORMAL --"), "{got}");
    check("vim_tiny", 40, 12, &got);
}

#[test]
fn help_overlay_default_preset() {
    for (w, h) in [(80, 24), (40, 12)] {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Char('?'));
        let got = render(&mut app, w, h);
        assert!(got.contains("default preset, no user bindings"), "{got}");
        check("help_default", w, h, &got);
    }
}

#[test]
fn help_overlay_vim_preset() {
    for (w, h) in [(80, 24), (40, 12)] {
        let mut app = online_in(new_vim_app());
        press(&mut app, KeyCode::Char('?'));
        let got = render(&mut app, w, h);
        assert!(got.contains("vim preset, no user bindings"), "{got}");
        check("help_vim", w, h, &got);
    }
}

/// At 40x12 the list does not fit: scrolling shows the rest, and the status row says where.
#[test]
fn help_overlay_scrolls() {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Char('?'));
    let top = render(&mut app, 40, 12);
    assert!(top.contains("lines 1-"), "{top}");
    for _ in 0..200 {
        press(&mut app, KeyCode::Down);
    }
    let end = render(&mut app, 40, 12);
    assert_ne!(top, end);
    assert!(end.contains("Close help"), "{end}");
    check("help_default_end", 40, 12, &end);
}

#[test]
fn help_too_small_says_so() {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Char('?'));
    // 32x9 is above the app's own minimum (30x8) but below what the overlay needs.
    let got = render(&mut app, 32, 9);
    assert!(
        got.contains("Terminal too small for help") && got.contains("press Esc to close"),
        "{got}"
    );
    // The close key still works there.
    press(&mut app, KeyCode::Char('?'));
    let got = render(&mut app, 32, 9);
    assert!(!got.contains("too small for help"), "{got}");
}

/// Rebinding changes the overlay and the footer.
#[test]
fn rebinding_changes_help_and_footer() {
    let mut s = Settings::default();
    s.keys.bindings.entry("list".into()).or_default().extend([
        ("ctrl+n".to_string(), "list_down".to_string()),
        ("ctrl+p".to_string(), "list_up".to_string()),
        ("q".to_string(), "unbound".to_string()),
        ("ctrl+q".to_string(), "quit".to_string()),
    ]);
    let mut app = online_in(new_app_with(s));
    let list = render(&mut app, 100, 30);
    let footer = list.lines().nth(29).unwrap();
    assert!(
        footer.contains("Ctrl+P/Ctrl+N chat") && footer.contains("Ctrl+Q quit"),
        "{footer}"
    );
    assert!(
        !footer.contains("Up/Down") && !footer.contains(" q quit"),
        "{footer}"
    );
    press(&mut app, KeyCode::Char('?'));
    let help = render(&mut app, 100, 40);
    assert!(help.contains("Ctrl+N, Down"), "{help}");
    assert!(help.contains("default preset, 4 user bindings"), "{help}");
    // The unbound `q` quits no more, Ctrl+Q does.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('q'));
    assert!(!app.should_quit());
    press_mod(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(app.should_quit());
}

/// A start-up settings problem shows as one notice without a network name.
#[test]
fn startup_notice_has_no_network_prefix() {
    let mut app = new_app();
    app.push_global_notice("settings.toml line 3: [keys.list] x: bad. Skipping.", true);
    let got = render(&mut app, 100, 30);
    // The notice row sits above the key hints.
    assert!(
        got.lines()
            .nth(28)
            .unwrap()
            .contains("settings.toml line 3"),
        "{got}"
    );
    assert!(!got.contains("Teams: settings.toml"), "{got}");
}

// ---- composer, emoji, links (issues 8, 9, 10, 14, 15, 25) ----

const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
const FLAG: &str = "\u{1F1FA}\u{1F1F3}";
const THUMB: &str = "\u{1F44D}\u{1F3FD}";

fn ctrl_key(app: &mut App, c: char) {
    press_mod(app, KeyCode::Char(c), KeyModifiers::CONTROL);
}

/// The screen as `render` gives it, plus where the terminal cursor ended up (`none` when the
/// frame never placed it), so the cursor column of wide text is part of the snapshot.
fn render_cursor(app: &mut App, w: u16, h: u16) -> String {
    app.resize(w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.set_cursor_position((u16::MAX, u16::MAX)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    let pos = term.get_cursor_position().unwrap();
    let cursor = if pos.x == u16::MAX {
        "none".to_string()
    } else {
        format!("{},{}", pos.x, pos.y)
    };
    format!("{}cursor: {cursor}\n", dump(&term))
}

/// Teams, Alex Example open with the message box focused and an empty draft.
fn typing_app() -> App {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Enter);
    app
}

fn alt_enter(app: &mut App) {
    press_mod(app, KeyCode::Enter, KeyModifiers::ALT);
}

/// Alex Example open in the pane, focus back on the list (what `u` reads).
fn chat_open_list_focus() -> App {
    let mut app = typing_app();
    press(&mut app, KeyCode::Esc);
    app
}

/// Sidebar rows with emoji in the title and the preview keep the border in one column.
#[test]
fn sidebar_with_emoji_stays_aligned() {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    let mut chats = teams_chats();
    chats[0].title = format!("Family {FAMILY} {FLAG} Chat");
    chats[0].last_preview =
        format!("Alex Example: {THUMB} \u{2764}\u{FE0F} 1\u{FE0F}\u{20E3} done {FLAG}");
    chats[1].title =
        format!("{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}{FLAG}");
    chats[1].last_preview =
        format!("{FAMILY}{FAMILY}{FAMILY}{FAMILY}{FAMILY}{FAMILY}{FAMILY}{FAMILY}{FAMILY}");
    app.handle_event(Event::Chats(Network::Teams, chats));
    app.resize(80, 24);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| draw(f, &app)).unwrap();
    let buf = term.backend().buffer();
    // Sidebar is 28 wide: its right border is column 27 on every body row.
    for y in 2..22 {
        let sym = buf[(27, y)].symbol();
        assert!(sym == "\u{2551}" || sym == "\u{2502}", "row {y}: {sym:?}");
    }
    check("emoji_sidebar", 80, 24, &dump(&term));
}

/// The cursor sits on the right column with wide characters and emoji before it.
#[test]
fn composer_cursor_column_with_wide_text() {
    let mut app = typing_app();
    type_str(&mut app, "\u{4f60}\u{597d} ");
    for c in FAMILY.chars().chain(FLAG.chars()) {
        press(&mut app, KeyCode::Char(c));
    }
    let got = render_cursor(&mut app, 80, 24);
    // Layout: sidebar 28, box border 1, prompt 2; text "\u{4f60}\u{597d} " (5 cols) + family (2) + flag (2).
    assert!(got.contains("cursor: 40,"), "{got}");
    check("composer_wide_cursor", 80, 24, &got);
    // One Left moves over the whole flag: 2 columns back.
    press(&mut app, KeyCode::Left);
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("cursor: 38,"), "{got}");
}

/// Three lines: the box grows by two rows and the conversation gives them up.
#[test]
fn composer_multiline_grows() {
    let mut app = typing_app();
    type_str(&mut app, "first line");
    alt_enter(&mut app);
    type_str(&mut app, "second line");
    alt_enter(&mut app);
    type_str(&mut app, "third");
    both_sizes_cursor("composer_multiline", &mut app);
}

fn both_sizes_cursor(name: &str, app: &mut App) {
    for (w, h) in [(100, 30), (80, 24)] {
        let got = render_cursor(app, w, h);
        check(name, w, h, &got);
    }
}

/// More than six lines: six rows are shown, scrolled to the cursor.
#[test]
fn composer_scrolls_after_six_rows() {
    let mut app = typing_app();
    for i in 1..=9 {
        type_str(&mut app, &format!("line {i}"));
        if i < 9 {
            alt_enter(&mut app);
        }
    }
    both_sizes_cursor("composer_six_rows", &mut app);
    // Cursor moved to the first line: the view scrolls back up with it.
    for _ in 0..8 {
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Left);
    }
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("> line 1"), "{got}");
    check("composer_six_rows_top", 80, 24, &got);
}

/// A long single line wraps inside the box instead of scrolling sideways.
#[test]
fn composer_wraps_a_long_line() {
    let mut app = typing_app();
    type_str(
        &mut app,
        "this is a long message that needs more than one row in the message box so it wraps at a space",
    );
    both_sizes_cursor("composer_wrap", &mut app);
}

#[test]
fn composer_multiline_on_a_narrow_terminal() {
    let mut app = typing_app();
    type_str(&mut app, "one");
    alt_enter(&mut app);
    type_str(&mut app, "two");
    let got = render_cursor(&mut app, 50, 20);
    check("composer_multiline_narrow", 50, 20, &got);
}

/// Pasting three lines inserts them and sends nothing.
#[test]
fn paste_three_lines() {
    let mut app = typing_app();
    app.handle_paste("alpha\r\nbeta\ngamma");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("> alpha") && got.contains("  gamma"), "{got}");
    check("paste_three_lines", 80, 24, &got);
}

/// `:thu` opens the list above the box.
#[test]
fn shortcode_popup() {
    let mut app = typing_app();
    type_str(&mut app, "nice :thu");
    both_sizes_cursor("shortcode_popup", &mut app);
    press(&mut app, KeyCode::Down);
    let got = render_cursor(&mut app, 80, 24);
    check("shortcode_popup_second", 80, 24, &got);
}

/// The popup squeezes into a short conversation pane without overflowing.
#[test]
fn shortcode_popup_in_a_small_terminal() {
    let mut app = typing_app();
    type_str(&mut app, ":sm");
    let got = render_cursor(&mut app, 60, 14);
    check("shortcode_popup_small", 60, 14, &got);
}

#[test]
fn emoji_picker_from_the_composer() {
    let mut app = typing_app();
    type_str(&mut app, "hi ");
    ctrl_key(&mut app, 'e');
    for (w, h) in [(80, 24), (60, 20)] {
        let got = render_cursor(&mut app, w, h);
        check("emoji_picker", w, h, &got);
    }
}

#[test]
fn emoji_picker_selection_and_scroll() {
    let mut app = typing_app();
    ctrl_key(&mut app, 'e');
    for _ in 0..12 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Right);
    let got = render_cursor(&mut app, 80, 24);
    check("emoji_picker_scrolled", 80, 24, &got);
    press(&mut app, KeyCode::PageDown);
    press(&mut app, KeyCode::PageDown);
    let got = render_cursor(&mut app, 80, 24);
    check("emoji_picker_category", 80, 24, &got);
}

#[test]
fn emoji_picker_search_and_empty_results() {
    let mut app = typing_app();
    ctrl_key(&mut app, 'e');
    type_str(&mut app, "heart");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("Search results"), "{got}");
    check("emoji_picker_search", 80, 24, &got);
    type_str(&mut app, "zzzz");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("No emoji match"), "{got}");
    check("emoji_picker_no_match", 80, 24, &got);
}

/// Opened for a reaction (the selection mode will call this): same grid, other title.
#[test]
fn emoji_picker_for_reactions_has_its_own_title() {
    let mut app = typing_app();
    press(&mut app, KeyCode::Esc);
    app.open_reaction_picker("alex", "m4");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains(" React "), "{got}");
    check("emoji_picker_react", 80, 24, &got);
}

/// The smallest screen the app draws (30x8 body of 6 rows): one grid row, still usable.
#[test]
fn emoji_picker_on_the_smallest_screen() {
    let mut app = typing_app();
    ctrl_key(&mut app, 'e');
    let got = render_cursor(&mut app, 32, 8);
    check("emoji_picker_smallest", 32, 8, &got);
}

/// 60x20: the grid, search row and status fit inside the screen.
#[test]
fn emoji_picker_fits_at_60x20() {
    let mut app = typing_app();
    ctrl_key(&mut app, 'e');
    let got = render_cursor(&mut app, 60, 20);
    assert!(got.lines().count() >= 20);
    for line in got.lines().take(20) {
        assert!(cols(line) <= 60, "{line:?}");
    }
}

fn cols(s: &str) -> usize {
    Span::raw(s).width()
}

/// A long link wraps over several lines and every line of it is underlined.
#[test]
fn link_is_underlined_across_wrapped_lines() {
    let mut app = chat_open_list_focus();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: vec![
            msg(
                "l1",
                ts(2026, 11, 16, 9, 20),
                false,
                "Alex Example",
                "Docs are at https://docs.example.invalid/handbook/section/page?id=12345&view=full#top (the newest), and plain words after it.",
            ),
            msg("l2", ts(2026, 11, 16, 9, 21), true, "You", "thanks, no link here"),
        ],
    });
    let got = render(&mut app, 80, 24);
    assert!(got.contains("UNDERLINED"), "{got}");
    check("link_underline", 80, 24, &got);
}

#[test]
fn link_confirm_line_shows_the_host() {
    let mut app = chat_open_list_focus();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: vec![msg(
            "l1",
            ts(2026, 11, 16, 9, 20),
            false,
            "Alex Example",
            "see https://docs.example.invalid/handbook for details",
        )],
    });
    press(&mut app, KeyCode::Char('u'));
    let got = render(&mut app, 80, 24);
    assert!(got.contains("Open docs.example.invalid? y/n"), "{got}");
    check("link_confirm", 80, 24, &got);
}

#[test]
fn link_pick_line_lists_hosts() {
    let mut app = chat_open_list_focus();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: vec![msg(
            "l1",
            ts(2026, 11, 16, 9, 20),
            false,
            "Alex Example",
            "a https://one.example.invalid/x and https://two.example.invalid/y",
        )],
    });
    press(&mut app, KeyCode::Char('u'));
    let got = render(&mut app, 100, 30);
    assert!(
        got.contains("1 one.example.invalid  2 two.example.invalid"),
        "{got}"
    );
    check("link_pick", 100, 30, &got);
}

/// A lookalike host is flagged on the confirm line.
#[test]
fn link_confirm_flags_non_ascii_hosts() {
    let mut app = chat_open_list_focus();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: vec![msg(
            "l1",
            ts(2026, 11, 16, 9, 20),
            false,
            "Alex Example",
            "https://\u{430}pple.example.invalid/login",
        )],
    });
    press(&mut app, KeyCode::Char('u'));
    let got = render(&mut app, 100, 30);
    assert!(got.contains("NON-ASCII NAME"), "{got}");
}

#[test]
fn composer_and_overlays_never_panic_at_any_size() {
    for (w, h) in [(30, 8), (31, 9), (40, 12), (50, 16), (60, 20), (120, 50)] {
        let mut app = typing_app();
        type_str(&mut app, "\u{4f60}\u{597d} :thu");
        for c in FAMILY.chars() {
            press(&mut app, KeyCode::Char(c));
        }
        alt_enter(&mut app);
        type_str(&mut app, "x");
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Esc);
        ctrl_key(&mut app, 'e');
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        let _ = render_cursor(&mut app, w, h);
    }
}

/// On a one-row footer a problem normally hides the key hints; a link waiting for y/n does not
/// lose its line, because every other key is switched off while it waits.
#[test]
fn link_prompt_beats_a_problem_on_a_one_row_footer() {
    let mut app = chat_open_list_focus();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: vec![msg(
            "l1",
            ts(2026, 11, 16, 9, 20),
            false,
            "Alex Example",
            "https://docs.example.invalid/x",
        )],
    });
    app.handle_event(Event::Notice {
        network: Network::Teams,
        text: "Could not refresh: try again.".into(),
        is_error: true,
    });
    let before = render(&mut app, 40, 12);
    assert!(
        before
            .lines()
            .nth(11)
            .unwrap()
            .contains("Could not refresh"),
        "{before}"
    );
    press(&mut app, KeyCode::Char('u'));
    let got = render(&mut app, 40, 12);
    assert!(
        got.lines()
            .nth(11)
            .unwrap()
            .contains("Open docs.example.invalid? y/n"),
        "{got}"
    );
    check("link_prompt_one_row", 40, 12, &got);
}

/// Ctrl+J is the newline key where the terminal keeps Alt+Enter and does not report Shift+Enter.
#[test]
fn ctrl_j_adds_a_line() {
    let mut app = typing_app();
    type_str(&mut app, "a");
    ctrl_key(&mut app, 'j');
    type_str(&mut app, "b");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("> a") && got.contains("  b"), "{got}");
}

/// The whole help text on a tall screen: the editing, popup and link keys are listed by name.
#[test]
fn help_lists_the_editing_popup_and_link_keys() {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Char('?'));
    let got = render(&mut app, 100, 140);
    for name in [
        "Cursor left",
        "Word right",
        "Delete word back",
        "Clear line",
        "Insert newline",
        "Emoji picker",
        "Popup accept",
        "Open link",
        "Confirm yes",
    ] {
        assert!(got.contains(name), "{name} missing:\n{got}");
    }
    assert!(got.contains("Ctrl+J, Alt+Enter, Shift+Enter"), "{got}");
    check("help_default_tall", 100, 140, &got);
}

// ---- message selection, replies, reactions, edit and delete, mentions (issues 11, 12, 13, 27, 28) ----

fn rich_history() -> Vec<ChatMessage> {
    let mut m2 = msg(
        "m2",
        ts(2026, 11, 16, 9, 5),
        true,
        "You",
        "Will do, thanks.",
    );
    m2.reactions = vec![Reaction {
        emoji: "\u{1F44D}".into(),
        count: 1,
        mine: true,
    }];
    let mut m3 = msg(
        "m3",
        ts(2026, 11, 16, 9, 10),
        false,
        "Alex Example",
        "Great. The summary page still needs a shorter introduction and captions for the figures.",
    );
    m3.reply_to = Some(QuotedRef {
        id: "m2".into(),
        sender: "You".into(),
        sender_id: String::new(),
        preview: "Will do, thanks.".into(),
    });
    m3.reactions = vec![
        Reaction {
            emoji: "\u{1F44D}".into(),
            count: 3,
            mine: false,
        },
        Reaction {
            emoji: "\u{2764}\u{FE0F}".into(),
            count: 1,
            mine: true,
        },
    ];
    let mut m4 = msg(
        "m4",
        ts(2026, 11, 16, 9, 12),
        true,
        "You",
        "I shortened the introduction.",
    );
    m4.edited = true;
    let mut m5 = msg("m5", ts(2026, 11, 16, 9, 14), true, "You", "");
    m5.state = MessageState::Deleted;
    let mut m6 = msg(
        "m6",
        ts(2026, 11, 16, 9, 16),
        false,
        "Sam Sample",
        "@Demo User can you also check the appendix?",
    );
    m6.mentions = vec![PersonRef {
        id: "u1".into(),
        name: "Demo User".into(),
    }];
    m6.mentions_me = true;
    let mut m7 = msg(
        "m7",
        ts(2026, 11, 16, 9, 18),
        false,
        "Alex Example",
        "Which one did you mean?",
    );
    m7.reply_to = Some(QuotedRef {
        id: "gone".into(),
        sender: "Sam Sample".into(),
        sender_id: String::new(),
        preview: String::new(),
    });
    vec![
        msg(
            "m1",
            ts(2026, 11, 16, 9, 0),
            false,
            "Alex Example",
            "Can you review the draft before Friday?",
        ),
        m2,
        m3,
        m4,
        m5,
        m6,
        m7,
    ]
}

/// Teams, Alex Example open with the rich history, message box focused. `caps` = the backend
/// has sent its Capabilities (all true, as the demo does).
fn rich_app(caps: bool) -> App {
    rich_in(new_app(), caps)
}

fn rich_in(mut app: App, caps: bool) -> App {
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    if caps {
        app.handle_event(Event::Capabilities {
            network: Network::Teams,
            caps: Capabilities::all(),
        });
    }
    press(&mut app, KeyCode::Enter);
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: rich_history(),
    });
    app
}

fn ctrl_up(app: &mut App) {
    press_mod(app, KeyCode::Up, KeyModifiers::CONTROL);
}

/// Selection on `id`: from the newest, Up past the messages that can be selected (the deleted
/// m5 is skipped).
fn select_msg(app: &mut App, id: &str) {
    let order = ["m1", "m2", "m3", "m4", "m6", "m7"];
    let at = order
        .iter()
        .position(|m| *m == id)
        .expect("a selectable id");
    ctrl_up(app);
    for _ in 0..order.len() - 1 - at {
        press(app, KeyCode::Up);
    }
}

#[test]
fn conversation_shows_quotes_reactions_edited_deleted_and_mentions() {
    both_sizes("rich_conversation", || rich_app(true));
}

#[test]
fn select_newest_message() {
    both_sizes("select_newest", || {
        let mut app = rich_app(true);
        ctrl_up(&mut app);
        app
    });
}

/// A message of yours with a reaction of yours: reply, react, edit and delete all apply.
#[test]
fn select_own_message_all_actions_apply() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m2");
    let got = render(&mut app, 100, 30);
    check("select_own", 100, 30, &got);
}

/// Someone else's message: edit and delete are dimmed in the footer.
#[test]
fn select_other_message_dims_edit_and_delete() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m3");
    let got = render(&mut app, 100, 30);
    check("select_other", 100, 30, &got);
}

/// Before the backend has sent its Capabilities every action is dimmed, but still works.
#[test]
fn select_before_capabilities_dims_everything() {
    let mut app = rich_app(false);
    select_msg(&mut app, "m2");
    let got = render(&mut app, 100, 30);
    check("select_no_caps", 100, 30, &got);
}

#[test]
fn select_in_the_vim_preset_says_select() {
    let mut app = rich_in(new_vim_app(), true);
    ctrl_up(&mut app);
    let got = render(&mut app, 100, 30);
    assert!(got.contains("-- SELECT --"), "{got}");
    check("select_vim", 100, 30, &got);
}

#[test]
fn select_on_a_narrow_terminal() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m3");
    let got = render(&mut app, 40, 12);
    check("select_narrow", 40, 12, &got);
}

#[test]
fn select_a_long_message_highlights_every_line() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m3");
    let got = render(&mut app, 60, 20);
    // The wrapped message is reverse video on each of its rows.
    assert!(got.matches("REVERSED").count() >= 3, "{got}");
    check("select_wrapped", 60, 20, &got);
}

#[test]
fn reply_line_above_the_message_box() {
    both_sizes_cursor_app("reply_target", || {
        let mut app = rich_app(true);
        select_msg(&mut app, "m3");
        press(&mut app, KeyCode::Char('r'));
        type_str(&mut app, "agreed");
        app
    });
}

#[test]
fn edit_line_above_the_message_box() {
    both_sizes_cursor_app("edit_target", || {
        let mut app = rich_app(true);
        select_msg(&mut app, "m4");
        press(&mut app, KeyCode::Char('E'));
        app
    });
}

#[test]
fn delete_question_on_the_footer() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m4");
    press(&mut app, KeyCode::Char('d'));
    let got = render(&mut app, 100, 30);
    assert!(got.contains("Delete for everyone? y/n"), "{got}");
    check("select_delete_confirm", 100, 30, &got);
}

#[test]
fn reaction_picker_opens_from_selection() {
    let mut app = rich_app(true);
    select_msg(&mut app, "m3");
    press(&mut app, KeyCode::Char('e'));
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains(" React "), "{got}");
    check("select_react_picker", 80, 24, &got);
}

fn both_sizes_cursor_app(name: &str, mut build: impl FnMut() -> App) {
    for (w, h) in [(100, 30), (80, 24)] {
        let mut app = build();
        let got = render_cursor(&mut app, w, h);
        check(name, w, h, &got);
    }
}

/// The group "Project Team" open with its members known.
fn group_app() -> App {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "proj".into(),
        messages: vec![ChatMessage {
            chat_id: "proj".into(),
            ..msg(
                "g1",
                ts(2026, 11, 16, 8, 2),
                false,
                "Sam Sample",
                "thanks, see you at the meeting",
            )
        }],
    });
    app.handle_event(Event::Members {
        network: Network::Teams,
        chat_id: "proj".into(),
        members: ["Sam Sample", "Alex Example", "Alexandra Placeholder"]
            .iter()
            .enumerate()
            .map(|(i, n)| PersonRef {
                id: format!("u{i}"),
                name: (*n).to_string(),
            })
            .collect(),
        error: None,
    });
    app
}

#[test]
fn mention_list_in_a_group() {
    both_sizes_cursor_app("mention_popup", || {
        let mut app = group_app();
        type_str(&mut app, "hello @al");
        app
    });
}

#[test]
fn mention_token_in_the_box_after_choosing() {
    let mut app = group_app();
    type_str(&mut app, "hello @al");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "please look");
    let got = render_cursor(&mut app, 80, 24);
    assert!(got.contains("> hello @Alex Example please look"), "{got}");
}

#[test]
fn chat_row_shows_that_unread_messages_mention_you() {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    let mut chats = teams_chats();
    chats[1].mentions = 3;
    app.handle_event(Event::Chats(Network::Teams, chats));
    let got = render(&mut app, 80, 24);
    assert!(got.contains("99+@"), "{got}");
    check("chat_row_mentions", 80, 24, &got);
}

#[test]
fn message_actions_never_panic_at_any_size() {
    for (w, h) in [(30, 8), (31, 9), (40, 12), (50, 16), (60, 20), (120, 50)] {
        let mut app = rich_app(true);
        let _ = render_cursor(&mut app, w, h);
        select_msg(&mut app, "m3");
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Char('r'));
        type_str(&mut app, "ok");
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Esc);
        select_msg(&mut app, "m4");
        press(&mut app, KeyCode::Char('E'));
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Esc);
        select_msg(&mut app, "m4");
        press(&mut app, KeyCode::Char('d'));
        let _ = render_cursor(&mut app, w, h);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('e'));
        let _ = render_cursor(&mut app, w, h);
    }
}

// ---- attachments: chips, thumbnails, choosing and sending a file (issues 16 to 21, 34 to 37) ----

use chatcli_core::model::{Attachment, Command, FetchRef, MediaKind};
use chatcli_tui::{decode_now, Depth};

fn media_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chatcli-snap-media-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 80x40 picture, four colours, with a white square in the top-left corner so a flipped or
/// mirrored thumbnail is obvious.
fn sample_png(dir: &std::path::Path) -> PathBuf {
    let img = image::RgbImage::from_fn(80, 40, |x, y| {
        if x < 10 && y < 10 {
            image::Rgb([255, 255, 255])
        } else {
            match (x < 40, y < 20) {
                (true, true) => image::Rgb([220, 40, 40]),
                (false, true) => image::Rgb([40, 180, 60]),
                (true, false) => image::Rgb([50, 80, 220]),
                (false, false) => image::Rgb([240, 200, 40]),
            }
        }
    });
    let path = dir.join("sunset.png");
    img.save_with_format(&path, image::ImageFormat::Png)
        .unwrap();
    path
}

fn attachment(kind: MediaKind, name: &str, size: u64) -> Attachment {
    Attachment {
        kind,
        name: Some(name.into()),
        mime: None,
        size: Some(size),
        fetch_ref: FetchRef::new(format!("ref-{name}")),
        open_url: None,
    }
}

fn media_msg(id: &str, at: i64, from_me: bool, text: &str, att: Vec<Attachment>) -> ChatMessage {
    ChatMessage {
        attachments: att,
        ..msg(
            id,
            at,
            from_me,
            if from_me { "You" } else { "Alex Example" },
            text,
        )
    }
}

fn media_history() -> Vec<ChatMessage> {
    vec![
        msg(
            "m1",
            ts(2026, 11, 16, 9, 0),
            false,
            "Alex Example",
            "Photos and files from the site visit:",
        ),
        media_msg(
            "m2",
            ts(2026, 11, 16, 9, 2),
            false,
            "[image] sunset from the lab window",
            vec![attachment(MediaKind::Image, "sunset.png", 52_000)],
        ),
        media_msg(
            "m3",
            ts(2026, 11, 16, 9, 4),
            false,
            "[document: notes.txt]",
            vec![attachment(MediaKind::Document, "notes.txt", 1_200)],
        ),
        media_msg(
            "m4",
            ts(2026, 11, 16, 9, 6),
            false,
            "[image]",
            vec![attachment(MediaKind::Image, "old-photo.jpg", 310_000)],
        ),
        media_msg(
            "m5",
            ts(2026, 11, 16, 9, 8),
            false,
            "[document: setup.exe]",
            vec![attachment(MediaKind::Document, "setup.exe", 8_400_000)],
        ),
        media_msg(
            "m6",
            ts(2026, 11, 16, 9, 10),
            true,
            "[image] [image] two at once",
            vec![
                attachment(MediaKind::Image, "a.png", 2_048),
                attachment(MediaKind::Image, "b.png", 3_100_000),
            ],
        ),
    ]
}

/// Teams, Alex Example open with the media history, capabilities all true, a fixed colour depth.
fn media_app(depth: Depth) -> App {
    let mut app = open_media_in(new_app(), depth);
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: media_history(),
    });
    app
}

fn open_media_in(mut app: App, depth: Depth) -> App {
    app.set_color_depth(depth);
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Capabilities {
        network: Network::Teams,
        caps: Capabilities::all(),
    });
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    press(&mut app, KeyCode::Enter);
    app
}

/// Selection on a message of `media_history`, counting up from the newest (m6).
fn select_media(app: &mut App, id: &str) {
    let order = ["m1", "m2", "m3", "m4", "m5", "m6"];
    let at = order.iter().position(|m| *m == id).expect("an id");
    ctrl_up(app);
    for _ in 0..order.len() - 1 - at {
        press(app, KeyCode::Up);
    }
}

fn media_ready(app: &mut App, id: &str, path: &std::path::Path) {
    app.handle_event(Event::MediaReady {
        network: Network::Teams,
        chat_id: "alex".into(),
        message_id: id.into(),
        index: 0,
        path: path.to_path_buf(),
    });
}

/// `render`, with RGB allowed (thumbnails).
fn render_media(app: &mut App, w: u16, h: u16) -> String {
    app.resize(w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    dump_with(&term, true)
}

/// The conversation with a chip per attachment, its markers gone from the text, and the states
/// an attachment can be in: idle, downloading, failed, opened.
#[test]
fn attachment_chips_and_their_states() {
    let dir = media_dir("chips");
    let png = sample_png(&dir);
    for (w, h) in [(100, 30), (80, 24)] {
        let mut app = media_app(Depth::True);
        // m3: download and open (the file arrives, the OS opens it).
        select_media(&mut app, "m3");
        press(&mut app, KeyCode::Char('o'));
        media_ready(&mut app, "m3", &png);
        let jobs = app.take_open_files();
        assert_eq!(jobs.len(), 1);
        app.open_done(&jobs[0], Ok(()));
        // m4: the link has expired.
        press(&mut app, KeyCode::Esc);
        select_media(&mut app, "m4");
        press(&mut app, KeyCode::Char('o'));
        app.handle_event(Event::MediaFailed {
            network: Network::Teams,
            chat_id: "alex".into(),
            message_id: "m4".into(),
            index: 0,
            reason: "MEDIA EXPIRED".into(),
            fallback_url: None,
        });
        // m2: still downloading.
        press(&mut app, KeyCode::Esc);
        select_media(&mut app, "m2");
        let out = press_out(&mut app, KeyCode::Char('o'));
        assert!(matches!(out[0].1, Command::FetchMedia { .. }));
        let got = render_media(&mut app, w, h);
        check("media_chips", w, h, &got);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn press_out(app: &mut App, code: KeyCode) -> chatcli_tui::Out {
    app.handle_key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// The executable message: `o` refuses it, with the reason in the notice line.
#[test]
fn an_executable_is_refused_with_a_reason() {
    let mut app = media_app(Depth::True);
    select_media(&mut app, "m5");
    let out = press_out(&mut app, KeyCode::Char('o'));
    assert!(out.is_empty());
    let got = render_media(&mut app, 100, 30);
    assert!(got.contains("BLOCKED: executable"), "{got}");
    check("media_blocked", 100, 30, &got);
}

/// `v` draws the thumbnail under the message, in the highlight too (never in reverse video).
#[test]
fn thumbnail_under_an_image() {
    let dir = media_dir("thumb");
    let png = sample_png(&dir);
    for (w, h) in [(100, 30), (80, 24)] {
        let mut app = media_app(Depth::True);
        select_media(&mut app, "m2");
        let out = press_out(&mut app, KeyCode::Char('v'));
        assert!(matches!(out[0].1, Command::FetchMedia { .. }));
        media_ready(&mut app, "m2", &png);
        assert!(app.take_open_files().is_empty());
        decode_now(&mut app);
        let got = render_media(&mut app, w, h);
        assert!(got.contains('\u{2580}'), "half blocks are drawn:\n{got}");
        check("media_thumbnail", w, h, &got);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thumbnail_in_a_256_colour_terminal() {
    let dir = media_dir("thumb256");
    let png = sample_png(&dir);
    let mut app = media_app(Depth::Indexed);
    select_media(&mut app, "m2");
    press(&mut app, KeyCode::Char('v'));
    media_ready(&mut app, "m2", &png);
    decode_now(&mut app);
    let got = render_media(&mut app, 80, 24);
    assert!(got.contains("Indexed("), "palette colours, not RGB");
    assert!(!got.contains("Rgb("));
    check("media_thumbnail_256", 80, 24, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A thumbnail partly scrolled out of the pane is cut with the lines: nothing is drawn outside.
#[test]
fn thumbnail_clips_when_scrolled_partly_out() {
    let dir = media_dir("clip");
    let png = sample_png(&dir);
    // The wheel moves the pane three lines a step: the thumbnail goes through the top edge, then
    // the bottom edge. Whatever is cut, nothing is drawn outside the pane.
    for steps in [0usize, 1, 2, 3, 4, 5] {
        let mut a = media_app(Depth::True);
        select_media(&mut a, "m2");
        press(&mut a, KeyCode::Char('v'));
        media_ready(&mut a, "m2", &png);
        decode_now(&mut a);
        // Leave selection: while a message is selected a resize keeps it in view.
        press(&mut a, KeyCode::Esc);
        a.resize(80, 16);
        for _ in 0..steps {
            a.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 50,
                row: 8,
                modifiers: KeyModifiers::NONE,
            });
        }
        let got = render_media(&mut a, 80, 16);
        check(&format!("media_thumbnail_scrolled{steps}"), 80, 16, &got);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Resizing narrower rescales the thumbnail to the pane; it never draws outside it.
#[test]
fn thumbnail_rescales_on_a_narrow_terminal() {
    let dir = media_dir("narrow");
    let png = sample_png(&dir);
    let mut app = media_app(Depth::True);
    select_media(&mut app, "m2");
    press(&mut app, KeyCode::Char('v'));
    media_ready(&mut app, "m2", &png);
    decode_now(&mut app);
    let got = render_media(&mut app, 50, 20);
    check("media_thumbnail_narrow", 50, 20, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A thumbnail that cannot be made says why, on the chip line.
#[test]
fn thumbnail_failure_shows_on_the_chip() {
    let dir = media_dir("fail");
    let bad = dir.join("sunset.png");
    std::fs::write(&bad, b"this is not a picture").unwrap();
    let mut app = media_app(Depth::True);
    select_media(&mut app, "m2");
    press(&mut app, KeyCode::Char('v'));
    media_ready(&mut app, "m2", &bad);
    decode_now(&mut app);
    let got = render_media(&mut app, 100, 30);
    assert!(got.contains("PREVIEW FAILED"), "{got}");
    check("media_thumbnail_failed", 100, 30, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `inline_images = false`: no thumbnail, and `v` points to `o`.
#[test]
fn inline_images_off_keeps_the_chip_only() {
    let mut s = Settings::default();
    s.media.inline_images = false;
    let mut app = open_media_in(new_app_with(s), Depth::True);
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: media_history(),
    });
    select_media(&mut app, "m2");
    let out = press_out(&mut app, KeyCode::Char('v'));
    assert!(out.is_empty());
    let got = render_media(&mut app, 100, 30);
    assert!(got.contains("INLINE IMAGES OFF"), "{got}");
    check("media_inline_off", 100, 30, &got);
}

// ---- choosing a file ----

fn attach_dir(tag: &str) -> PathBuf {
    let dir = media_dir(&format!("attach-{tag}"));
    std::fs::create_dir_all(dir.join("My Pictures")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("photo.png"), b"x").unwrap();
    std::fs::write(dir.join("plan.pdf"), b"x").unwrap();
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    dir
}

fn ctrl_o(app: &mut App) {
    press_mod(app, KeyCode::Char('o'), KeyModifiers::CONTROL);
}

#[test]
fn path_prompt_lists_folders_first() {
    let dir = attach_dir("path_prompt_lists_folder");
    let mut app = media_app(Depth::True);
    app.set_attach_dir(dir.clone());
    ctrl_o(&mut app);
    let got = render(&mut app, 100, 30);
    check("attach_prompt", 100, 30, &got);
    // Typing narrows the list; names with spaces complete.
    type_str(&mut app, "my p");
    let got = render(&mut app, 80, 24);
    assert!(got.contains("My Pictures/"), "{got}");
    check("attach_prompt_typed", 80, 24, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_prompt_with_nothing_matching() {
    let dir = attach_dir("path_prompt_with_nothing");
    let mut app = media_app(Depth::True);
    app.set_attach_dir(dir.clone());
    ctrl_o(&mut app);
    type_str(&mut app, "zzz");
    let got = render(&mut app, 80, 24);
    check("attach_prompt_nothing", 80, 24, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_prompt_on_a_narrow_terminal() {
    let dir = attach_dir("path_prompt_on_a_narrow_");
    let mut app = media_app(Depth::True);
    app.set_attach_dir(dir.clone());
    ctrl_o(&mut app);
    let got = render(&mut app, 40, 12);
    check("attach_prompt_narrow", 40, 12, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

fn attached_app(dir: &std::path::Path, name: &str) -> App {
    let mut app = media_app(Depth::True);
    app.set_attach_dir(dir.to_path_buf());
    ctrl_o(&mut app);
    type_str(&mut app, name);
    press(&mut app, KeyCode::Enter);
    app
}

/// The chip above the text, with a caption typed after it.
#[test]
fn chip_above_the_message_box() {
    let dir = attach_dir("chip_above_the_message_b");
    let mut app = attached_app(&dir, "photo.png");
    type_str(&mut app, "look at this");
    for (w, h) in [(100, 30), (80, 24)] {
        let got = render(&mut app, w, h);
        assert!(got.contains("[photo.png 1 B]"), "{got}");
        check("attach_chip", w, h, &got);
    }
    let got = render(&mut app, 40, 12);
    check("attach_chip_narrow", 40, 12, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Teams document asks about OneDrive first; an executable asks on every network.
#[test]
fn confirm_lines_before_a_file_goes() {
    let dir = attach_dir("confirm_lines_before_a_f");
    let mut app = attached_app(&dir, "plan.pdf");
    let out = press_out(&mut app, KeyCode::Enter);
    assert!(out.is_empty());
    let got = render(&mut app, 100, 30);
    assert!(got.contains("Upload plan.pdf to your OneDrive?"), "{got}");
    check("attach_confirm_onedrive", 100, 30, &got);
    std::fs::write(dir.join("setup.exe"), b"x").unwrap();
    let mut app = attached_app(&dir, "setup.exe");
    press(&mut app, KeyCode::Enter);
    let got = render(&mut app, 100, 30);
    assert!(got.contains("EXECUTABLE setup.exe"), "{got}");
    check("attach_confirm_executable", 100, 30, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file being sent: pending message with its chip and progress, and the footer's progress.
#[test]
fn upload_progress_on_the_message_and_in_the_footer() {
    let dir = attach_dir("upload_progress_on_the_m");
    // The PNG signature then filler: the TUI guesses "image" from the first bytes only.
    let mut photo = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    photo.resize(3_000_000, 0);
    std::fs::write(dir.join("photo.png"), photo).unwrap();
    let mut app = attached_app(&dir, "photo.png");
    type_str(&mut app, "caption");
    let out = press_out(&mut app, KeyCode::Enter);
    let Command::SendMedia { client_id, .. } = &out[0].1 else {
        panic!("expected SendMedia, got {out:?}");
    };
    app.handle_event(Event::SendProgress {
        network: Network::Teams,
        chat_id: "alex".into(),
        client_id: client_id.clone(),
        done: 1_350_000,
        total: 3_000_000,
    });
    for (w, h) in [(100, 30), (80, 24)] {
        let got = render(&mut app, w, h);
        assert!(got.contains("[sending 45%]"), "{got}");
        check("upload_progress", w, h, &got);
    }
    // And a failure: the reason on the message, the footer back to the hints.
    app.handle_event(Event::MessageFailed {
        network: Network::Teams,
        chat_id: "alex".into(),
        client_id: client_id.clone(),
        error: "IMAGE OVER 4 MB: pick a smaller picture.".into(),
    });
    let got = render(&mut app, 100, 30);
    assert!(got.contains("NOT SENT - IMAGE OVER 4 MB"), "{got}");
    check("upload_failed", 100, 30, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Vim preset: the chip, the path prompt and `o` / `v` all work from Insert and Select.
#[test]
fn vim_preset_has_the_attachment_keys() {
    let dir = attach_dir("vim_preset_has_the_attac");
    let mut app = open_media_in(new_vim_app(), Depth::True);
    app.set_attach_dir(dir.clone());
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: media_history(),
    });
    ctrl_o(&mut app);
    type_str(&mut app, "notes.txt");
    press(&mut app, KeyCode::Enter);
    let got = render(&mut app, 100, 30);
    assert!(got.contains("[notes.txt 1 B]"), "{got}");
    check("attach_chip_vim", 100, 30, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Everyday stage: themes (22), layout (23), older history (29, 30), unread (31), pin, mute and
// hide (32), search (33), typing (52).
// ---------------------------------------------------------------------------------------------

use chatcli_core::media::MediaCache;
use chatcli_core::model::{ChatFlags, MUTE_FOREVER};
use chatcli_core::runtime::UiCtx;
use chatcli_core::settings::{DateOrder, Density, SidebarWidth, TimeFormat};
use chatcli_core::store::Store;
use chatcli_tui::Theme;

fn settings_with(f: impl FnOnce(&mut Settings)) -> Settings {
    let mut s = Settings::default();
    f(&mut s);
    s
}

fn theme_settings(name: &str, overrides: &[(&str, &str)]) -> Settings {
    settings_with(|s| {
        s.theme.name = name.to_string();
        for (i, (k, v)) in overrides.iter().enumerate() {
            s.theme.overrides.insert((*k).to_string(), (*v).to_string());
            s.theme.lines.insert((*k).to_string(), i + 5);
        }
    })
}

/// Teams with chats and the Alex chat open (list focus), a failed message so the red shows.
fn themed(settings: Settings) -> App {
    let mut app = online_in(new_app_with(settings));
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::Online {
            live: false,
            detail: Some("phone is offline".into()),
        },
    ));
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "this one will fail");
    press(&mut app, KeyCode::Enter);
    app.handle_event(Event::MessageFailed {
        network: Network::Teams,
        chat_id: "alex".into(),
        client_id: format!("local-{}-1", ts(2026, 11, 16, 9, 30)),
        error: "demo: this chat refuses messages".into(),
    });
    press(&mut app, KeyCode::Esc);
    app
}

#[test]
fn theme_default_is_exactly_the_screen_without_a_theme() {
    for (w, h) in [(100, 30), (80, 24)] {
        let plain = render(&mut themed(Settings::default()), w, h);
        let named = render(&mut themed(theme_settings("default", &[])), w, h);
        assert_eq!(plain, named, "name = \"default\" changes nothing");
        check("theme_default", w, h, &named);
    }
}

#[test]
fn theme_high_contrast() {
    for (w, h) in [(100, 30), (80, 24)] {
        let got = render(&mut themed(theme_settings("high-contrast", &[])), w, h);
        // No grey and no dim text anywhere.
        assert!(!got.contains("DarkGray") && !got.contains("DIM"), "{got}");
        check("theme_high_contrast", w, h, &got);
    }
}

#[test]
fn theme_light_and_dark() {
    for name in ["light", "dark"] {
        let got = render(&mut themed(theme_settings(name, &[])), 100, 30);
        check(&format!("theme_{name}"), 100, 30, &got);
    }
}

#[test]
fn a_role_override_changes_only_that_role() {
    let base = render(&mut themed(Settings::default()), 100, 30);
    let changed = render(
        &mut themed(theme_settings("default", &[("own_name", "green")])),
        100,
        30,
    );
    assert_ne!(base, changed);
    // Only the colour of the "You" label moved: same text, same cells, Cyan became Green.
    assert_eq!(base.replace("Cyan", "Green"), changed);
    check("theme_override_own_name", 100, 30, &changed);
}

#[test]
fn rgb_and_index_overrides_are_fixed_colours() {
    let settings = theme_settings("default", &[("dim", "#808080"), ("accent", "33")]);
    let mut app = themed(settings);
    app.resize(100, 30);
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| draw(f, &app)).unwrap();
    let got = dump_with(&term, true);
    assert!(got.contains("Rgb(128, 128, 128)"), "{got}");
    check("theme_override_rgb", 100, 30, &got);
}

#[test]
fn no_color_removes_every_colour_and_keeps_bold_dim_and_reverse() {
    let (theme, problems) = Theme::from_settings(&Settings::default().theme, true);
    assert!(problems.is_empty());
    for (w, h) in [(100, 30), (80, 24)] {
        let mut app = themed(Settings::default());
        app.set_theme(theme.clone());
        app.resize(w, h);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer();
        for y in 0..h {
            for x in 0..w {
                let c = &buf[(x, y)];
                assert!(
                    c.fg == Color::Reset && c.bg == Color::Reset,
                    "colour at {x},{y}: {:?} on {:?}",
                    c.fg,
                    c.bg
                );
            }
        }
        let got = dump(&term);
        assert!(
            got.contains("REVERSED"),
            "the selected chat is still reverse video"
        );
        assert!(got.contains("DIM"), "secondary text is dim, not coloured");
        assert!(got.contains("NOT SENT"), "status stays text");
        check("theme_no_color", w, h, &got);
    }
}

#[test]
fn a_bad_theme_value_names_the_role_in_the_notice_bar() {
    let dir = std::env::temp_dir().join(format!("chatcli-snap-theme-{}", std::process::id()));
    let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
    let settings = theme_settings("default", &[("dim", "banana")]);
    let mut ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
    ctx.settings = std::sync::Arc::new(settings);
    let mut app = App::with_ctx(
        Clock {
            now_ms: ts(2026, 11, 16, 9, 30),
            tz: Some(zone()),
        },
        &ctx,
    );
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    let got = render(&mut app, 100, 30);
    assert!(
        got.contains("[theme] dim:") && got.contains("banana"),
        "{got}"
    );
    check("theme_bad_value", 100, 30, &got);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- layout ----

fn layout_app(f: impl FnOnce(&mut Settings)) -> App {
    open_chat_in(new_app_with(settings_with(f)))
}

#[test]
fn layout_sidebar_width_in_columns_and_percent() {
    both_sizes("layout_sidebar_cols", || {
        layout_app(|s| s.layout.sidebar_width = Some(SidebarWidth::Columns(20)))
    });
    both_sizes("layout_sidebar_percent", || {
        layout_app(|s| s.layout.sidebar_width = Some(SidebarWidth::Percent(50)))
    });
}

#[test]
fn layout_twelve_hour_times_and_date_orders() {
    both_sizes("layout_12h", || {
        layout_app(|s| s.layout.time_format = TimeFormat::H12)
    });
    both_sizes("layout_mdy", || {
        layout_app(|s| s.layout.date_order = DateOrder::Mdy)
    });
    let got = render(
        &mut layout_app(|s| {
            s.layout.date_order = DateOrder::Ymd;
            s.layout.time_format = TimeFormat::H12;
        }),
        100,
        30,
    );
    assert!(got.contains("2026-11-05"), "{got}");
    check("layout_ymd_12h", 100, 30, &got);
}

#[test]
fn layout_compact_density_and_no_previews() {
    both_sizes("layout_compact", || {
        layout_app(|s| s.layout.density = Density::Compact)
    });
    both_sizes("layout_no_previews", || {
        layout_app(|s| s.layout.show_previews = false)
    });
    // Narrow: the list alone, still one row per chat.
    let mut app = online_in(new_app_with(settings_with(|s| {
        s.layout.density = Density::Compact
    })));
    let got = render(&mut app, 50, 20);
    check("layout_compact_narrow", 50, 20, &got);
}

#[test]
fn default_layout_settings_give_todays_screen() {
    for (w, h) in [(100, 30), (80, 24)] {
        let today = render(&mut open_chat_app(), w, h);
        let explicit = render(
            &mut layout_app(|s| {
                s.layout.time_format = TimeFormat::H24;
                s.layout.date_order = DateOrder::Dmy;
                s.layout.density = Density::Cozy;
                s.layout.show_previews = true;
            }),
            w,
            h,
        );
        assert_eq!(today, explicit);
    }
}

// ---- pin, mute, hide ----

fn flagged_app() -> App {
    let mut app = online_in(new_app());
    let get = |id: &str| teams_chats().into_iter().find(|c| c.id == id).unwrap();
    let mut proj = get("proj");
    proj.flags = ChatFlags {
        muted_until: Some(MUTE_FOREVER),
        ..ChatFlags::default()
    };
    let mut sam = get("sam");
    sam.flags = ChatFlags {
        pinned: true,
        ..ChatFlags::default()
    };
    let mut rel = get("rel");
    rel.flags = ChatFlags {
        hidden: true,
        ..ChatFlags::default()
    };
    for c in [proj, sam, rel] {
        app.handle_event(Event::ChatUpdated(c));
    }
    app
}

#[test]
fn pinned_muted_and_hidden_chats() {
    both_sizes("chat_flags", flagged_app);
    both_sizes("chat_flags_hidden_shown", || {
        let mut app = flagged_app();
        press(&mut app, KeyCode::Char('H'));
        app
    });
    // The tab total leaves the muted 100 and the hidden chat out: 2 (Alex) + 1 (Sam).
    let got = render(&mut flagged_app(), 100, 30);
    assert!(got.contains("Teams 3"), "{got}");
}

// ---- unread ----

#[test]
fn jump_to_the_next_unread_chat_and_mark_all_read() {
    both_sizes("unread_jump", || {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Char('n'));
        app
    });
    both_sizes("mark_all_read_prompt", || {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Char('M'));
        app
    });
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Char('y'));
    let got = render(&mut app, 100, 30);
    assert!(!got.contains("Teams 3"), "the tab total is gone: {got}");
    check("mark_all_read_done", 100, 30, &got);
}

// ---- older history ----

fn many_messages_app() -> App {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Enter);
    let msgs: Vec<ChatMessage> = (0..40)
        .map(|i| {
            msg(
                &format!("x{i:02}"),
                ts(2026, 11, 16, 8, 0) + i * 60_000,
                i % 3 == 0,
                if i % 3 == 0 { "You" } else { "Alex Example" },
                &format!("message number {i}"),
            )
        })
        .collect();
    app.handle_event(Event::Messages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: msgs,
    });
    app
}

fn page_up(app: &mut App, times: usize) -> Vec<(Network, Command)> {
    let mut out = Vec::new();
    for _ in 0..times {
        out.extend(press_out(app, KeyCode::PageUp));
    }
    out
}

#[test]
fn loading_older_shows_a_line_at_the_top_and_the_start_says_so() {
    let mut app = many_messages_app();
    let out = page_up(&mut app, 10);
    assert_eq!(
        out.iter()
            .filter(|(_, c)| matches!(c, Command::LoadOlder { .. }))
            .count(),
        1
    );
    let got = render(&mut app, 100, 30);
    assert!(got.contains("loading older..."), "{got}");
    check("older_loading", 100, 30, &got);
    let got80 = render(&mut app, 80, 24);
    check("older_loading", 80, 24, &got80);

    let older: Vec<ChatMessage> = (0..5)
        .map(|i| {
            msg(
                &format!("o{i}"),
                ts(2026, 11, 16, 7, 0) + i * 60_000,
                false,
                "Alex Example",
                &format!("an older message {i}"),
            )
        })
        .collect();
    app.handle_event(Event::OlderMessages {
        network: Network::Teams,
        chat_id: "alex".into(),
        messages: older,
        at_start: true,
    });
    page_up(&mut app, 10);
    let got = render(&mut app, 100, 30);
    assert!(got.contains("start of conversation"), "{got}");
    assert!(!got.contains("loading older"), "{got}");
    check("older_start", 100, 30, &got);
}

#[test]
fn a_failed_older_load_is_a_notice() {
    let mut app = many_messages_app();
    page_up(&mut app, 10);
    app.handle_event(Event::OlderFailed {
        network: Network::Teams,
        chat_id: "alex".into(),
        error: "Teams is busy: wait a moment and scroll up again.".into(),
    });
    let got = render(&mut app, 100, 30);
    assert!(got.contains("Teams is busy"), "{got}");
    assert!(!got.contains("loading older"), "{got}");
    check("older_failed", 100, 30, &got);
}

// ---- search ----

fn search_app() -> App {
    let store = Store::open_in_memory().unwrap();
    store.upsert_chats(&teams_chats()).unwrap();
    let wa = ChatSummary {
        network: Network::WhatsApp,
        id: "riley".into(),
        title: "Riley Sample".into(),
        last_ts: Some(ts(2026, 11, 14, 9, 0)),
        ..ChatSummary::blank(Network::WhatsApp)
    };
    store.upsert_chats(std::slice::from_ref(&wa)).unwrap();
    let mut msgs = alex_history();
    msgs[2].text = "The budget notes: the summary page needs a shorter introduction.".into();
    let mut proj = msg(
        "p1",
        ts(2026, 11, 16, 8, 2),
        false,
        "Sam Sample",
        "budget review moved to Thursday",
    );
    proj.chat_id = "proj".into();
    let mut riley = msg(
        "r1",
        ts(2026, 11, 14, 9, 0),
        false,
        "Riley Sample",
        "did you see the BUDGET figures?",
    );
    riley.network = Network::WhatsApp;
    riley.chat_id = "riley".into();
    msgs.push(proj);
    msgs.push(riley);
    store.upsert_messages(&msgs).unwrap();
    let dir = std::env::temp_dir().join(format!("chatcli-snap-search-{}", std::process::id()));
    let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
    let ctx = UiCtx::new(store, media);
    let mut app = App::with_ctx(
        Clock {
            now_ms: ts(2026, 11, 16, 9, 30),
            tz: Some(zone()),
        },
        &ctx,
    );
    app.handle_event(Event::Status(
        Network::Teams,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::Teams, teams_chats()));
    app.handle_event(Event::Chats(Network::WhatsApp, vec![wa]));
    app
}

fn ctrl_f(app: &mut App) {
    press_mod(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
}

#[test]
fn search_box_empty_with_hits_and_without() {
    both_sizes("search_empty", || {
        let mut app = search_app();
        ctrl_f(&mut app);
        app
    });
    both_sizes("search_hits", || {
        let mut app = search_app();
        ctrl_f(&mut app);
        type_str(&mut app, "budget");
        press(&mut app, KeyCode::Down);
        app
    });
    let mut app = search_app();
    ctrl_f(&mut app);
    type_str(&mut app, "zzzz");
    let got = render(&mut app, 80, 24);
    assert!(got.contains("No messages match"), "{got}");
    check("search_no_match", 80, 24, &got);
}

#[test]
fn search_box_on_small_screens() {
    let mut app = search_app();
    ctrl_f(&mut app);
    type_str(&mut app, "budget");
    let got = render(&mut app, 40, 12);
    check("search_hits_narrow", 40, 12, &got);
    let got = render(&mut app, 30, 8);
    check("search_hits_tiny", 30, 8, &got);
}

#[test]
fn search_enter_opens_the_chat_of_the_hit() {
    let mut app = search_app();
    ctrl_f(&mut app);
    type_str(&mut app, "summary");
    let out = press_out(&mut app, KeyCode::Enter);
    // Like Enter on the chat in the list: open it, and read it (it had 2 unread).
    assert_eq!(
        out,
        vec![
            (
                Network::Teams,
                Command::OpenChat {
                    chat_id: "alex".into()
                }
            ),
            (
                Network::Teams,
                Command::MarkRead {
                    chat_id: "alex".into()
                }
            ),
        ]
    );
    let got = render(&mut app, 100, 30);
    check("search_opened_chat", 100, 30, &got);
}

// ---- typing ----

fn wa_chats() -> Vec<ChatSummary> {
    let mk = |id: &str, title: &str, kind: ChatKind, last: i64, preview: &str| ChatSummary {
        network: Network::WhatsApp,
        id: id.into(),
        title: title.into(),
        kind,
        last_ts: Some(last),
        last_preview: preview.into(),
        ..ChatSummary::blank(Network::WhatsApp)
    };
    vec![
        mk(
            "riley",
            "Riley Sample",
            ChatKind::Direct,
            ts(2026, 11, 16, 9, 20),
            "Riley Sample: see you there",
        ),
        mk(
            "fam",
            "Family Group",
            ChatKind::Group,
            ts(2026, 11, 16, 8, 0),
            "Sam Sample: dinner at 7",
        ),
    ]
}

fn wa_typing_app() -> App {
    let mut app = new_app();
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::WhatsApp, wa_chats()));
    press(&mut app, KeyCode::Tab);
    app
}

#[test]
fn someone_typing_shows_on_the_sidebar_row_and_above_the_message_box() {
    both_sizes("typing", || {
        let mut app = wa_typing_app();
        press(&mut app, KeyCode::Enter);
        app.handle_event(Event::Typing {
            network: Network::WhatsApp,
            chat_id: "riley".into(),
            sender: "Riley Sample".into(),
            active: true,
        });
        app.handle_event(Event::Typing {
            network: Network::WhatsApp,
            chat_id: "fam".into(),
            sender: "Sam Sample".into(),
            active: true,
        });
        app.handle_event(Event::Typing {
            network: Network::WhatsApp,
            chat_id: "fam".into(),
            sender: "Jordan Placeholder".into(),
            active: true,
        });
        app
    });
    // Compact rows have no preview line: the word goes after the title.
    let mut app = new_app_with(settings_with(|s| s.layout.density = Density::Compact));
    app.handle_event(Event::Status(
        Network::WhatsApp,
        Status::Online {
            live: true,
            detail: None,
        },
    ));
    app.handle_event(Event::Chats(Network::WhatsApp, wa_chats()));
    press(&mut app, KeyCode::Tab);
    app.handle_event(Event::Typing {
        network: Network::WhatsApp,
        chat_id: "fam".into(),
        sender: "Sam Sample".into(),
        active: true,
    });
    let got = render(&mut app, 100, 30);
    assert!(got.contains("typing"), "{got}");
    check("typing_compact", 100, 30, &got);
}

/// The new screens at many sizes: search, a loading chat, flags, typing, compact rows and a wide
/// sidebar setting. Nothing may panic or index out of range.
#[test]
fn everyday_screens_never_panic_at_any_size() {
    type Build = (&'static str, fn() -> App);
    let builds: Vec<Build> = vec![
        ("search with hits", || {
            let mut app = search_app();
            ctrl_f(&mut app);
            type_str(&mut app, "budget");
            app
        }),
        ("search empty", || {
            let mut app = search_app();
            ctrl_f(&mut app);
            app
        }),
        ("flags", || {
            let mut app = flagged_app();
            press(&mut app, KeyCode::Char('H'));
            app
        }),
        ("mark all read prompt", || {
            let mut app = online_in(new_app());
            press(&mut app, KeyCode::Char('M'));
            app
        }),
        ("loading older", || {
            let mut app = many_messages_app();
            page_up(&mut app, 10);
            app
        }),
        ("typing", || {
            let mut app = wa_typing_app();
            press(&mut app, KeyCode::Enter);
            app.handle_event(Event::Typing {
                network: Network::WhatsApp,
                chat_id: "riley".into(),
                sender: "Riley Sample \u{4eca}\u{65e5}".into(),
                active: true,
            });
            app
        }),
        ("compact twelve hour", || {
            layout_app(|s| {
                s.layout.density = Density::Compact;
                s.layout.time_format = TimeFormat::H12;
                s.layout.sidebar_width = Some(SidebarWidth::Percent(70));
            })
        }),
        ("high contrast", || {
            themed(theme_settings("high-contrast", &[]))
        }),
    ];
    for (name, build) in builds {
        for w in (1..=120).step_by(7) {
            for h in (1..=45).step_by(4) {
                let mut app = build();
                let _ = (name, render(&mut app, w, h));
            }
        }
    }
}

/// High-contrast has no dim text, so the footer tells what applies by weight: edit and delete on
/// someone else's message are plain, the actions that work are bold.
#[test]
fn select_other_message_in_high_contrast_still_marks_what_does_not_apply() {
    let mut app = rich_in(new_app_with(theme_settings("high-contrast", &[])), true);
    select_msg(&mut app, "m3");
    let got = render(&mut app, 100, 30);
    assert!(!got.contains("DIM"), "{got}");
    let last = got
        .lines()
        .rev()
        .find(|l| l.starts_with("row 29 "))
        .expect("a style run on the footer row");
    assert!(last.contains("BOLD"), "{last}");
    check("select_other_high_contrast", 100, 30, &got);
}

// ---- commands: the / bar, slash in the message box, prompts, F2 (S2) ----

fn at_sizes(name: &str, sizes: &[(u16, u16)], mut build: impl FnMut() -> App) {
    for &(w, h) in sizes {
        let mut app = build();
        let got = render(&mut app, w, h);
        check(name, w, h, &got);
    }
}

/// Teams online with chats, the `/` bar open with `text` typed.
fn bar_app(text: &str) -> App {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, text);
    app
}

#[test]
fn command_bar_empty() {
    at_sizes(
        "command_bar_empty",
        &[(100, 30), (80, 24), (40, 12)],
        || bar_app(""),
    );
}

#[test]
fn command_bar_filtered() {
    at_sizes(
        "command_bar_filtered",
        &[(100, 30), (80, 24), (40, 12)],
        || bar_app("se"),
    );
}

#[test]
fn command_bar_lists_chats_after_commands() {
    // "e" matches several commands and chats: both sections show.
    at_sizes("command_bar_chats", &[(100, 30), (80, 24)], || {
        bar_app("sam")
    });
}

#[test]
fn command_bar_no_match() {
    at_sizes("command_bar_no_match", &[(80, 24)], || bar_app("zzzz"));
}

#[test]
fn command_bar_scrolls_with_the_selection() {
    let mut app = bar_app("");
    app.resize(40, 12);
    for _ in 0..12 {
        press(&mut app, KeyCode::Down);
    }
    let got = render(&mut app, 40, 12);
    // The last command is chosen and on screen.
    assert!(got.contains("/quit"), "{got}");
    check("command_bar_scrolled", 40, 12, &got);
}

#[test]
fn command_bar_dims_what_cannot_run() {
    // Signed in: /signin cannot run and is drawn dim.
    let mut app = bar_app("sign");
    let got = render(&mut app, 80, 24);
    assert!(got.contains("/signin"), "{got}");
    assert!(got.contains("DIM"), "{got}");
}

#[test]
fn composer_slash_popup() {
    at_sizes("composer_slash_popup", &[(100, 30), (80, 24)], || {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "/s");
        app
    });
}

#[test]
fn unknown_command_notice() {
    at_sizes("unknown_command", &[(80, 24)], || {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "/foo");
        press(&mut app, KeyCode::Enter);
        app
    });
}

#[test]
fn signout_prompt() {
    at_sizes("signout_prompt", &[(80, 24)], || {
        let mut app = online_in(new_app());
        press(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "signout");
        press(&mut app, KeyCode::Enter);
        app
    });
}

fn demo_app() -> App {
    let dir = std::env::temp_dir().join(format!("chatcli-snap-demo-{}", std::process::id()));
    let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
    let mut ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
    ctx.demo = true;
    let mut app = App::with_ctx(
        Clock {
            now_ms: ts(2026, 11, 16, 9, 30),
            tz: Some(zone()),
        },
        &ctx,
    );
    let _ = std::fs::remove_dir_all(&dir);
    app.handle_event(Event::Status(
        Network::Teams,
        Status::NeedsLogin { reason: None },
    ));
    app
}

#[test]
fn demo_sign_in_panel() {
    at_sizes("demo_sign_in_panel", &[(80, 24)], demo_app);
    let got = render(&mut demo_app(), 80, 24);
    assert!(got.contains("demo: no browser"), "{got}");
    assert!(!got.contains("opens browser"), "{got}");
    // The real panel still says it opens the browser.
    let mut real = new_app();
    real.handle_event(Event::Status(
        Network::Teams,
        Status::NeedsLogin { reason: None },
    ));
    let got = render(&mut real, 80, 24);
    assert!(got.contains("opens browser"), "{got}");
    assert!(got.contains("Type / for commands"), "{got}");
}

#[test]
fn empty_pane_points_at_commands() {
    at_sizes("empty_pane_commands_hint", &[(100, 30)], || {
        online_in(new_app())
    });
    let got = render(&mut online_in(new_app()), 100, 30);
    assert!(got.contains("Type / for commands"), "{got}");
}

/// The footer tells a new user how to find commands and help, in every screen that has one.
#[test]
fn footers_point_at_commands_and_help() {
    let panel = || {
        let mut app = new_app();
        app.handle_event(Event::Status(
            Network::Teams,
            Status::NeedsLogin { reason: None },
        ));
        app
    };
    let select = || {
        let mut app = open_chat_app();
        press_mod(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        app
    };
    let screens: [(&str, App, &str); 5] = [
        ("list", online_in(new_app()), "? help"),
        ("composer", typing_app(), "F1 help"),
        ("panel", panel(), "? help"),
        // 80 columns keep the way out (Esc back) and the bar; help follows on wider screens.
        ("select", select(), "Esc back"),
        ("vim list", online_in(new_vim_app()), "? help"),
    ];
    for (what, mut app, help) in screens {
        let got = render(&mut app, 80, 24);
        // The screen is the first 24 lines of the dump: the footer is the last two.
        let screen: Vec<&str> = got.lines().take(24).collect();
        let footer = screen[22..].join(" ");
        assert!(footer.contains("/ commands"), "{what}:\n{got}");
        assert!(footer.contains(help), "{what}:\n{got}");
    }
}

/// Once something is typed the pointer goes: the keys matter more.
#[test]
fn the_commands_pointer_leaves_the_footer_when_you_type() {
    let mut app = typing_app();
    type_str(&mut app, "hi");
    let got = render(&mut app, 80, 24);
    assert!(!got.contains("/ commands"), "{got}");
}

/// Teams online with chats, the settings screen open on the section `downs` places below
/// Appearance, the cursor in its rows when `in_rows`.
fn settings_app(downs: usize, in_rows: bool) -> App {
    let mut app = online_in(new_app());
    press(&mut app, KeyCode::F(2));
    for _ in 0..downs {
        press(&mut app, KeyCode::Down);
    }
    if in_rows {
        press(&mut app, KeyCode::Enter);
    }
    app
}

#[test]
fn settings_appearance() {
    at_sizes(
        "settings_appearance",
        &[(100, 30), (80, 24), (40, 12)],
        || settings_app(0, true),
    );
}

#[test]
fn settings_layout() {
    both_sizes("settings_layout", || settings_app(1, true));
}

#[test]
fn settings_notifications() {
    both_sizes("settings_notifications", || settings_app(2, true));
}

#[test]
fn settings_media() {
    both_sizes("settings_media", || settings_app(3, true));
}

#[test]
fn settings_teams() {
    both_sizes("settings_teams", || settings_app(4, true));
}

#[test]
fn settings_whatsapp() {
    both_sizes("settings_whatsapp", || settings_app(5, true));
}

/// The Keys page with the cursor on the Chat list `refresh` row (a Down count: the capture
/// tests below check the footer names `refresh`, so a reordered page fails loudly).
fn keys_app() -> App {
    let mut app = settings_app(6, true);
    for _ in 0..20 {
        press(&mut app, KeyCode::Down);
    }
    app
}

#[test]
fn settings_keys() {
    at_sizes("settings_keys", &[(100, 30), (80, 24), (40, 12)], keys_app);
}

/// Waiting for the new key: the footer asks, Esc cancels.
#[test]
fn key_capture() {
    at_sizes("key_capture", &[(100, 30), (80, 24), (40, 12)], || {
        let mut app = keys_app();
        press(&mut app, KeyCode::Enter);
        let got = render(&mut app, 100, 30);
        assert!(got.contains("Press the new key for Refresh"), "{got}");
        app
    });
}

/// `p` already pins: Enter swaps, Esc cancels.
#[test]
fn key_conflict() {
    at_sizes("key_conflict", &[(100, 30), (80, 24)], || {
        let mut app = keys_app();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('p'));
        let got = render(&mut app, 100, 30);
        assert!(got.contains("p is already Pin"), "{got}");
        app
    });
}

fn no_colour(mut app: App) -> App {
    app.set_theme(Theme::from_settings(&Settings::default().theme, true).0);
    app
}

fn high_contrast(mut app: App) -> App {
    app.set_theme(Theme::builtin("high-contrast").unwrap());
    app
}

/// Selection must survive NO_COLOR and high contrast on the new screens: reverse video is
/// the only signal left.
#[test]
fn the_new_screens_under_no_colour_and_high_contrast() {
    at_sizes("settings_appearance_no_color", &[(80, 24)], || {
        no_colour(settings_app(0, true))
    });
    at_sizes("settings_keys_no_color", &[(80, 24)], || {
        no_colour(keys_app())
    });
    at_sizes("command_bar_no_color", &[(80, 24)], || {
        no_colour(bar_app("se"))
    });
    at_sizes("key_conflict_high_contrast", &[(80, 24)], || {
        let mut app = high_contrast(keys_app());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('p'));
        app
    });
}

/// A key the capture refuses: the red line says why and the footer keeps asking.
#[test]
fn key_capture_refused() {
    at_sizes("key_capture_refused", &[(80, 24)], || {
        // The first Everywhere row (quit): a plain key would stop typing everywhere.
        let mut app = settings_app(6, true);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('x'));
        let got = render(&mut app, 80, 24);
        assert!(got.contains("x types text here"), "{got}");
        app
    });
}

#[test]
fn keys_reset_all_prompt() {
    at_sizes("keys_reset_all_prompt", &[(80, 24)], || {
        let mut app = settings_app(6, true);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        let got = render(&mut app, 80, 24);
        assert!(got.contains("Reset every key to the preset? y/n"), "{got}");
        app
    });
}

/// The footer of the chat list follows the new key at once.
#[test]
fn key_rebound_footer() {
    at_sizes("key_rebound_footer", &[(80, 24)], || {
        let mut app = keys_app();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Esc);
        let got = render(&mut app, 80, 24);
        assert!(got.contains("F5 refresh"), "{got}");
        app
    });
}

/// The vim preset from the page: the footer shows the mode at once.
#[test]
fn key_preset_row_switches_to_vim() {
    at_sizes("key_preset_row_vim", &[(80, 24)], || {
        let mut app = settings_app(6, true);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Esc);
        let got = render(&mut app, 80, 24);
        assert!(got.contains("-- NORMAL --"), "{got}");
        app
    });
}

/// Capture, swap and refusal draw at every size without a panic.
#[test]
fn keys_ui_survives_every_terminal_size() {
    for variant in 0..4 {
        for w in (1..=120).step_by(7) {
            for h in (1..=45).step_by(4) {
                let mut app = keys_app();
                press(&mut app, KeyCode::Enter);
                match variant {
                    1 => {
                        press(&mut app, KeyCode::Char('p'));
                    }
                    2 => {
                        press(&mut app, KeyCode::Char('x'));
                    }
                    3 => {
                        press(&mut app, KeyCode::F(5));
                    }
                    _ => {}
                }
                let _ = render(&mut app, w, h);
            }
        }
    }
}

#[test]
fn settings_about() {
    both_sizes("settings_about", || settings_app(7, false));
}

#[test]
fn settings_sections_narrow() {
    at_sizes("settings_sections_narrow", &[(50, 20)], || {
        settings_app(0, false)
    });
}

/// A number refused: the red line says why and the box stays open.
#[test]
fn settings_number_error() {
    at_sizes("settings_number_error", &[(80, 24)], || {
        let mut app = settings_app(3, true);
        press(&mut app, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "0");
        press(&mut app, KeyCode::Enter);
        app
    });
}

/// A colour being typed on a role row (nothing applied yet).
#[test]
fn settings_colour_entry() {
    at_sizes("settings_colour_entry", &[(80, 24)], || {
        let mut app = settings_app(0, true);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "#12ab");
        app
    });
}

#[test]
fn settings_problems() {
    at_sizes("settings_problems", &[(100, 30)], || {
        let mut app = settings_app(1, true);
        app.settings_saved(vec![
            "settings.toml line 12: [layout] density: use \"cozy\" or \"compact\". Using the defaults for [layout]."
                .into(),
            "settings.toml line 20: [theme] dim: \"banana\" is not a colour.".into(),
        ]);
        app
    });
}

#[test]
fn settings_problems_about() {
    at_sizes("settings_problems_about", &[(100, 30)], || {
        let mut app = settings_app(7, false);
        app.settings_saved(vec![
            "settings.toml line 12: [layout] density: use \"cozy\" or \"compact\". Using the defaults for [layout]."
                .into(),
            "settings.toml line 20: [theme] dim: \"banana\" is not a colour.".into(),
        ]);
        app
    });
}

#[test]
fn settings_save_failed() {
    at_sizes("settings_save_failed", &[(80, 24)], || {
        let mut app = settings_app(1, true);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        app.settings_save_failed(
            "NOT SAVED - settings.toml line 3 has a syntax error: fix it in a text editor",
        );
        app
    });
}

/// The page closed after switching to the light theme: the whole app is in the new theme.
#[test]
fn theme_live_light() {
    at_sizes("theme_live_light", &[(100, 30)], || {
        let mut app = settings_app(0, true);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Esc);
        app
    });
}

/// Every page, in the rows, typing and with errors, draws at every size without a panic.
#[test]
fn settings_ui_survives_every_terminal_size() {
    for section in 0..8 {
        for variant in 0..3 {
            for w in (1..=120).step_by(7) {
                for h in (1..=45).step_by(4) {
                    let mut app = settings_app(section, variant > 0);
                    if variant == 2 {
                        press(&mut app, KeyCode::Enter);
                        type_str(&mut app, "x");
                        press(&mut app, KeyCode::Enter);
                    }
                    app.settings_save_failed("NOT SAVED - nothing");
                    let _ = render(&mut app, w, h);
                }
            }
        }
    }
}

/// The bar, the page and the popup draw at every size without a panic.
#[test]
fn commands_ui_survives_every_terminal_size() {
    type Build = fn() -> App;
    let builds: [(&str, Build); 5] = [
        ("bar", || bar_app("")),
        ("bar chats", || bar_app("a")),
        ("bar none", || bar_app("zzzz")),
        ("page", || {
            let mut app = online_in(new_app());
            press(&mut app, KeyCode::F(2));
            app
        }),
        ("popup", || {
            let mut app = online_in(new_app());
            press(&mut app, KeyCode::Enter);
            type_str(&mut app, "/");
            app
        }),
    ];
    for (name, build) in builds {
        for w in (1..=120).step_by(7) {
            for h in (1..=45).step_by(4) {
                let mut app = build();
                let _ = (name, render(&mut app, w, h));
            }
        }
    }
}

// ---- the tour (S5) ----

/// Teams online with chats, the tour at step 1.
fn tour_app() -> App {
    let mut app = online_in(new_app());
    app.start_tour();
    app
}

#[test]
fn tour_caption() {
    at_sizes("tour_caption", &[(100, 30), (80, 24), (40, 12)], tour_app);
}

#[test]
fn tour_paused() {
    at_sizes("tour_paused", &[(80, 24)], || {
        let mut app = tour_app();
        press(&mut app, KeyCode::Char(' '));
        app
    });
}

#[test]
fn tour_done() {
    at_sizes("tour_done", &[(80, 24)], || {
        let mut app = tour_app();
        let start = ts(2026, 11, 16, 9, 30);
        for round in 1..=4000 {
            if app.tour_done() {
                break;
            }
            app.tick(start + round * 250);
            let _ = app.fire_timers();
        }
        assert!(app.tour_done(), "the tour did not reach its last step");
        app
    });
}
