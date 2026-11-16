//! The guided tour, played to the end against the real demo backend on a paused clock: every
//! step runs under a TestBackend, the screen is drawn each round, and the tour must finish
//! without a panic, without a browser or an external app, and leave the settings as they
//! started. `TOUR_FRAMES=<folder>` also writes the screen at the end of each step, for reading.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use chatcli_core::media::MediaCache;
use chatcli_core::model::{now_ms, Command, CommandTx, EventRx, Network};
use chatcli_core::runtime::UiCtx;
use chatcli_core::settings::{load_from, save_edits, Settings};
use chatcli_core::store::Store;
use chatcli_tui::{demo, draw, App, Clock};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const ROUND_MS: i64 = 250;
const MAX_ROUNDS: usize = 6000;

struct Run {
    app: App,
    rx: EventRx,
    tx: HashMap<Network, CommandTx>,
    term: Terminal<TestBackend>,
    now: i64,
    /// Name of every command the tour sent to a backend.
    sent: Vec<&'static str>,
    /// Every settings edit, in order: replayed into a file at the end.
    edits: Vec<chatcli_core::settings::SettingEdit>,
    opened: Vec<String>,
    dir: PathBuf,
}

fn name(c: &Command) -> &'static str {
    match c {
        Command::Login => "Login",
        Command::OpenChat { .. } => "OpenChat",
        Command::Send { .. } => "Send",
        Command::React { .. } => "React",
        Command::Edit { .. } => "Edit",
        Command::Delete { .. } => "Delete",
        _ => "other",
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn start(width: u16, height: u16, tag: &str) -> Run {
    let dir = std::env::temp_dir().join(format!("chatcli-tour-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let media = MediaCache::open(&dir.join("media"), &Settings::default().media).unwrap();
    let store = Store::open_in_memory().unwrap();
    let (rx, tx) = demo::spawn(store.clone(), media.clone());
    let mut ctx = UiCtx::new(store, media);
    ctx.demo = true;
    ctx.tour = true;
    let now = now_ms();
    let mut app = App::with_ctx(
        Clock {
            now_ms: now,
            tz: None,
        },
        &ctx,
    );
    app.resize(width, height);
    Run {
        app,
        rx,
        tx,
        term: Terminal::new(TestBackend::new(width, height)).unwrap(),
        now,
        sent: Vec::new(),
        edits: Vec::new(),
        opened: Vec::new(),
        dir,
    }
}

impl Run {
    fn deliver(&mut self, out: chatcli_tui::Out) {
        for (net, cmd) in out {
            self.sent.push(name(&cmd));
            let _ = self.tx[&net].send(cmd);
        }
    }

    fn screen(&mut self) -> String {
        self.term.draw(|f| draw(f, &self.app)).unwrap();
        let buf = self.term.backend().buffer();
        let w = buf.area.width;
        let mut text = String::new();
        for y in 0..buf.area.height {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            text.push_str(line.trim_end());
            text.push('\n');
        }
        text
    }

    /// One round: the clock moves, timers fire, the backend answers, the screen is drawn.
    async fn round(&mut self) {
        self.now += ROUND_MS;
        self.app.tick(self.now);
        let out = self.app.fire_timers();
        self.deliver(out);
        tokio::time::advance(Duration::from_millis(ROUND_MS as u64)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        while let Ok(ev) = self.rx.try_recv() {
            let out = self.app.handle_event(ev);
            self.deliver(out);
        }
        // Nothing the tour does may reach outside the program.
        let step = self.app.tour_step();
        self.opened.extend(
            self.app
                .take_open_urls()
                .into_iter()
                .map(|u| format!("step {step:?} url {u}")),
        );
        self.opened.extend(
            self.app
                .take_open_files()
                .into_iter()
                .map(|j| format!("step {step:?} file {:?}", j.path)),
        );
        // The demo turns a toast into a notice line (lib.rs `show_toast`), never a desktop toast.
        let _ = self.app.take_toasts();
        let _ = self.app.take_decodes();
        self.edits.extend(self.app.take_setting_edits());
        self.screen();
    }
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

#[tokio::test(start_paused = true)]
async fn the_tour_plays_every_step_to_the_end() {
    let mut r = start(80, 24, "full");
    assert!(r.app.tour_running());
    let frames = std::env::var_os("TOUR_FRAMES").map(PathBuf::from);
    let mut seen_steps = HashSet::new();
    let mut last = String::new();
    let mut last_step = 1;
    for _ in 0..MAX_ROUNDS {
        if r.app.tour_done() {
            break;
        }
        let step = r.app.tour_step().unwrap();
        seen_steps.insert(step);
        assert!(r.app.next_deadline().is_some(), "step {step} has no timer");
        if step != last_step {
            if let Some(dir) = &frames {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(format!("step{last_step:02}.txt")), &last).unwrap();
            }
            last_step = step;
        }
        r.round().await;
        let screen = r.screen();
        let first = screen.lines().next().unwrap_or("");
        assert!(
            first.starts_with("TOUR "),
            "no caption on step {step}: {first}"
        );
        if let Some(dir) = &frames {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("all.txt"))
                .unwrap();
            writeln!(
                f,
                "##### step {step} t={}
{screen}",
                r.now
            )
            .unwrap();
        }
        last = screen;
    }
    assert!(r.app.tour_done(), "tour did not finish");
    // 25 steps play; the 26th is the done screen the loop stops on.
    assert_eq!(seen_steps.len(), 25, "steps played: {seen_steps:?}");
    assert_eq!(r.app.tour_step(), Some(26));
    if let Some(dir) = &frames {
        std::fs::write(dir.join("done.txt"), r.screen()).unwrap();
    }
    assert!(r.screen().contains("Tour done"));
    assert!(r.app.next_deadline().is_none());

    // What the tour did, by what it asked the backends for.
    for want in ["Login", "OpenChat", "Send", "React", "Edit", "Delete"] {
        assert!(
            r.sent.contains(&want),
            "tour never sent {want}: {:?}",
            r.sent
        );
    }
    assert!(r.sent.iter().filter(|c| **c == "Send").count() >= 2);
    assert!(
        r.opened.is_empty(),
        "the tour opened something: {:?}",
        r.opened
    );

    // Theme, layout and keys end where they began: replay every saved edit into a file.
    let path = r.dir.join("settings.toml");
    save_edits(&path, &r.edits).unwrap();
    assert_eq!(load_from(&path).settings, Settings::default());

    // Any key on the last step leaves the tour, and the app answers keys again.
    r.app.handle_key(key(KeyCode::Char('x')));
    assert!(!r.app.tour_running());
    let _ = std::fs::remove_dir_all(&r.dir);
}

#[tokio::test(start_paused = true)]
async fn space_pauses_n_skips_and_esc_ends_with_the_app_usable() {
    let mut r = start(80, 24, "keys");
    r.round().await;
    assert!(r.app.next_deadline().is_some());

    r.app.handle_key(key(KeyCode::Char(' ')));
    assert!(r.app.tour_paused());
    assert_eq!(r.app.next_deadline(), None, "a paused tour holds no timer");
    assert!(r.screen().contains("PAUSED"));
    let before = r.app.tour_step();
    for _ in 0..20 {
        r.round().await;
    }
    assert_eq!(r.app.tour_step(), before, "a paused tour moved on");
    r.app.handle_key(key(KeyCode::Char(' ')));
    assert!(!r.app.tour_paused() && r.app.next_deadline().is_some());

    // n: the rest of the step at once, then the next step starts.
    r.app.tick(r.now);
    let out = r.app.handle_key(key(KeyCode::Char('n')));
    r.deliver(out);
    assert_eq!(r.app.tour_step(), Some(2), "n did not finish step 1");

    // Keys that are not tour keys do nothing.
    r.app.handle_key(key(KeyCode::Char('q')));
    assert!(!r.app.should_quit());

    // Esc ends the tour; the caption goes and the demo answers keys.
    r.app.handle_key(key(KeyCode::Esc));
    assert!(!r.app.tour_running());
    assert!(!r.screen().starts_with("TOUR "));
    r.app.handle_key(ctrl_c());
    assert!(r.app.should_quit(), "the app ignores keys after the tour");
    let _ = std::fs::remove_dir_all(&r.dir);
}

#[tokio::test(start_paused = true)]
async fn the_caption_bar_fits_a_small_terminal() {
    let mut r = start(40, 12, "small");
    r.round().await;
    let screen = r.screen();
    assert!(
        screen.lines().next().unwrap().starts_with("TOUR 1/26 - "),
        "{screen}"
    );
    // The tour must still play on a small screen without a panic.
    for _ in 0..400 {
        r.round().await;
    }
    let _ = std::fs::remove_dir_all(&r.dir);
}

#[tokio::test(start_paused = true)]
async fn caps_lock_ctrl_c_quits_while_the_tour_plays() {
    let mut r = start(80, 24, "capsc");
    r.round().await;
    assert!(r.app.tour_running());
    let mut ev = key(KeyCode::Char('C'));
    ev.modifiers = KeyModifiers::CONTROL;
    r.app.handle_key(ev);
    assert!(r.app.should_quit());
    let _ = std::fs::remove_dir_all(&r.dir);
}

#[tokio::test(start_paused = true)]
async fn esc_during_the_key_capture_step_leaves_nothing_listening() {
    let mut r = start(80, 24, "esccap");
    let mut listening = false;
    for _ in 0..MAX_ROUNDS {
        r.round().await;
        if r.screen().contains("Press the new key") {
            listening = true;
            break;
        }
    }
    assert!(listening, "the tour never reached the key capture");
    r.app.handle_key(key(KeyCode::Esc));
    assert!(!r.app.tour_running());
    assert!(!r.screen().contains("Press the new key"));
    // The next key is the demo's own: Ctrl+C quits, it does not rebind anything.
    r.app.handle_key(ctrl_c());
    assert!(r.app.should_quit());
    assert!(r
        .app
        .take_setting_edits()
        .iter()
        .all(|e| e.key != "q" && e.table != "keys.list" || e.key != "q"));
    let _ = std::fs::remove_dir_all(&r.dir);
}
