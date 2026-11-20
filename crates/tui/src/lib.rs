//! Terminal UI for chatcli: a tab per network, every chat in a sidebar, message pane and
//! composer. It talks to the backends only through the `Event` / `Command` channels in
//! `chatcli_core::model` and never touches a network.
//!
//! - `app`: all state and input handling (pure, tested without a terminal)
//! - `composer`: the draft, its cursor and editing keys; `emoji`, `popup`: `:shortcode:` completion
//!   and the emoji picker; `links`: web links in messages
//! - `media_view`, `attach`, `gfx`: attachments: chips and thumbnails, choosing a file to send,
//!   Sixel / Kitty / iTerm2 graphics
//! - `commands`, `palette`: the `/` command registry and the command bar; `settings_view`: the
//!   settings screen model
//! - `keymap`: named actions, key chords, the built-in keys, `[keys]` validation
//! - `tour`: the guided tour's script (`chatcli --demo --tour`); `app/tour.rs` plays it
//! - `ui`: drawing
//! - `text`: width-aware helpers and the clock
//! - `demo`: fake backends for `chatcli --demo`

mod app;
mod attach;
mod burst;
mod chats;
mod commands;
mod composer;
pub mod demo;
mod emoji;
mod gfx;
mod input;
mod keymap;
mod keys_edit;
mod links;
mod list;
mod media_view;
mod notify;
mod older;
mod overlay;
mod palette;
mod popup;
mod search;
mod settings_ui;
mod settings_view;
mod text;
mod theme;
mod tour;
mod typing;
mod ui;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chatcli_core::media::MAX_PIXELS;
use chatcli_core::model::{now_ms, Command, CommandTx, Event as BackendEvent, EventRx, Network};
use chatcli_core::runtime::UiCtx;
use chatcli_core::settings::{Settings, SettingsProblem};
use chatcli_core::{log, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as TermEvent, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc::unbounded_channel;

pub use app::media::FileJob;
pub use app::{App, Focus, Notice, Out};
pub use keymap::{parse_key, Action, Context, KeyChord, Keymap};
pub use media_view::{make_thumb, Depth, MediaKey, Thumb};
pub use text::Clock;
pub use theme::{parse_colour, Theme};
pub use ui::draw;

/// Problems the TUI finds in `settings.toml` that core cannot: every `[keys]` binding (unknown
/// action, unreadable key, same key twice, a key that would stop typing, no key left to quit or back out with). `chatcli settings check` prints these together with core's own problems.
pub fn validate_settings(settings: &Settings) -> Vec<SettingsProblem> {
    let mut problems = keymap::validate_keys(settings);
    problems.extend(theme::validate_theme(&settings.theme));
    problems
}

/// Writes the settings edits the screen queued to `path`, then tells the app how the file
/// reads now (or why it was not saved). True when there was anything to save.
fn save_pending(app: &mut App, path: &std::path::Path) -> bool {
    let edits = app.take_setting_edits();
    if edits.is_empty() {
        return false;
    }
    match chatcli_core::settings::save_edits(path, &edits) {
        Ok(()) => {
            let report = chatcli_core::settings::load_from(path);
            let mut problems: Vec<String> = report.problems.iter().map(|p| p.text()).collect();
            problems.extend(validate_settings(&report.settings).iter().map(|p| p.text()));
            app.settings_saved(problems);
        }
        Err(e) => app.settings_save_failed(&e),
    }
    true
}

/// Decodes every picture the app queued for a thumbnail and hands the results back, as the run
/// loop does on worker threads. For tests and scripts that drive an `App` without a terminal.
pub fn decode_now(app: &mut App) {
    let (depth, max) = (app.color_depth(), app.max_file_bytes());
    for job in app.take_decodes() {
        let result = make_thumb(&job.path, depth, max);
        app.thumb_ready(&job, result);
    }
}

/// True while chatcli has the kitty keyboard protocol turned on, so it is turned off once.
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);

/// Asks a Unix terminal to report modifiers on every key (the kitty keyboard protocol), so
/// Shift+Enter and the like arrive as themselves. Only where the terminal says it can: others
/// keep the plain keys. The Windows console reports them anyway. True when Shift+Enter works.
fn enable_modified_keys() -> bool {
    if cfg!(windows) {
        return true;
    }
    let pushed = matches!(supports_keyboard_enhancement(), Ok(true))
        && crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    KEYBOARD_PUSHED.store(pushed, Ordering::Relaxed);
    pushed
}

/// Puts the terminal back as the shell expects it. Safe to call twice.
fn restore_terminal() {
    // Before leaving the alternate screen: the terminal keeps the flags per screen.
    if KEYBOARD_PUSHED.swap(false, Ordering::Relaxed) {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

/// True while the full-screen view owns the terminal.
static UI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Only a panic on the UI thread (or one after the UI is gone) should reset the terminal. A
/// backend task that panics must not: the UI keeps running and would draw into a terminal
/// that is no longer in raw mode.
fn hook_restores_terminal(on_ui_thread: bool, ui_active: bool) -> bool {
    on_ui_thread || !ui_active
}

/// Raw mode, alternate screen, mouse and paste capture, undone on drop and on panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<TerminalGuard> {
        enable_raw_mode().map_err(|e| format!("Could not start the full-screen view ({e}): run chatcli in a normal terminal window."))?;
        let guard = TerminalGuard;
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange)
            .map_err(|e| format!("Could not start the full-screen view ({e}): run chatcli in a normal terminal window."))?;
        keymap::set_modified_enter(enable_modified_keys());
        // Restore before the panic message prints, or it lands in the alternate screen and is lost.
        // The hook stays installed after run() returns: the process is about to exit.
        // The UI future runs on the thread that called run(), so that thread's id marks it.
        let ui_thread = std::thread::current().id();
        UI_ACTIVE.store(true, Ordering::Relaxed);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let on_ui = std::thread::current().id() == ui_thread;
            if hook_restores_terminal(on_ui, UI_ACTIVE.load(Ordering::Relaxed)) {
                restore_terminal();
                previous(info);
            } else {
                // Location only: a panic message can carry message text. The dead task shows
                // up as "Backend stopped" on the next command to it.
                let at = info.location().map_or("unknown".to_string(), |l| {
                    format!("{}:{}", l.file(), l.line())
                });
                log::error("tui", &format!("a background task panicked at {at}"));
            }
        }));
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        UI_ACTIVE.store(false, Ordering::Relaxed);
        restore_terminal();
    }
}

/// Shortest sleep for a pending timer: caps wakeups at 10 a second.
const MIN_WAKE_MS: i64 = 100;

/// Marks the cells of `stale` boxes that no image covers now with a style no real cell has, so
/// the next frame rewrites each of them (and the terminal drops the image pixels there).
fn mark_stale(f: &mut ratatui::Frame, stale: &[Rect], images: &[ui::VisibleImage]) {
    use ratatui::style::{Modifier, Style};
    let area = f.area();
    let marker = Style::new().add_modifier(Modifier::HIDDEN);
    let buf = f.buffer_mut();
    for r in stale {
        let r = r.intersection(area);
        for y in r.top()..r.bottom() {
            for x in r.left()..r.right() {
                let p = ratatui::layout::Position::new(x, y);
                if images.iter().any(|v| v.rect.contains(p)) {
                    continue;
                }
                buf[(x, y)].set_symbol(" ").set_style(marker);
            }
        }
    }
}

fn deliver(app: &mut App, commands: &HashMap<Network, CommandTx>, out: Out) {
    for (net, cmd) in out {
        // A rejected send gives the command back, so a Send can be failed instead of left
        // as pending for ever.
        let rejected = match commands.get(&net) {
            Some(tx) => tx.send(cmd).err().map(|e| e.0),
            None => Some(cmd),
        };
        if let Some(cmd) = rejected {
            log::error("tui", &format!("{} backend is not running", net.key()));
            app.command_undelivered(net, &cmd);
        }
    }
}

/// Opens a file in its app. The demo never does: it only says so.
fn open_file_or_demo(demo: bool, path: &std::path::Path) -> chatcli_core::Result<()> {
    if demo {
        Ok(())
    } else {
        chatcli_core::open::open_file(path)
    }
}

/// Opens a confirmed link in the browser. A failure is shown, never swallowed. The demo has
/// no real links, so it says what would happen instead.
fn open_link(app: &mut App, demo: bool, url: &str) {
    if demo {
        app.push_global_notice("demo: a real run opens that link in the browser.", false);
    } else if let Err(e) = chatcli_core::open::open_url(url) {
        app.push_global_notice(&e, true);
    }
}

/// Hands a toast to the desktop notifier. The demo has none, so it says what a real run would
/// show. A failure is logged once, never shown in a loop and never a crash. The log holds no
/// toast text.
fn show_toast(app: &mut App, ctx: &UiCtx, failed: &mut bool, toast: chatcli_core::runtime::Toast) {
    if ctx.demo {
        let text = format!("demo toast: {} - {}", toast.title, toast.body);
        app.push_global_notice(&text, false);
    } else if let Err(e) = ctx.notifier.notify(&toast) {
        if !std::mem::replace(failed, true) {
            log::error(
                "tui",
                &format!("desktop notification failed (later failures are not logged): {e}"),
            );
        }
    }
}

/// Seconds after start that the search index is read into the OS cache (`Store::warm_search`).
const WARM_AFTER_SECS: u64 = 3;

/// What a worker thread sends back to the loop.
enum Internal {
    /// A thumbnail was decoded (or could not be).
    Thumb(FileJob, std::result::Result<Thumb, String>),
    /// The clipboard picture was saved to a temp file (or could not be): network and chat.
    Clipboard(Network, String, std::result::Result<PathBuf, String>),
    /// An image was encoded for the terminal's graphics protocol (or could not be).
    Encoded(
        gfx::GfxKey,
        std::result::Result<ratatui_image::sliced::SlicedProtocol, String>,
    ),
    /// A search query ran: its box, its number, and the hits or why it failed.
    Search(u64, u64, std::result::Result<search::Found, String>),
}

/// What the user reads when a worker thread dies before it answers: the chip or the clipboard
/// must not wait for ever on an answer that will never come.
const LOST: &str = "The picture worker stopped: press o to open it instead, or restart chatcli.";

/// Runs `work` on a blocking thread and sends `done(its result)` to the loop. A panic in `work`
/// (a decoder bug on a hostile file) becomes `lost()`, never silence.
fn spawn_work<R: Send + 'static>(
    tx: tokio::sync::mpsc::UnboundedSender<Internal>,
    work: impl FnOnce() -> R + Send + 'static,
    lost: impl FnOnce() -> R + Send + 'static,
    done: impl FnOnce(R) -> Internal + Send + 'static,
) {
    tokio::spawn(async move {
        let result = match tokio::task::spawn_blocking(work).await {
            Ok(r) => r,
            Err(_) => {
                log::error("tui", "a media worker thread panicked");
                lost()
            }
        };
        let _ = tx.send(done(result));
    });
}

/// `y`: puts `text` on the clipboard, two ways at once. `clipboard` (the native one) first, except
/// over SSH, where it would be the remote machine's. Then the terminal is asked too (OSC 52):
/// most terminals do it (Ghostty, kitty, foot, WezTerm, Windows Terminal), it reaches the right
/// machine over SSH, and a terminal without it ignores the sequence. Last, so where it works the
/// terminal holds the text: on Wayland the native copy is served by chatcli and would go when it
/// quits. The `Ok` text is the notice.
fn copy_text(
    clipboard: &mut Option<arboard::Clipboard>,
    text: &str,
) -> std::result::Result<&'static str, String> {
    use std::io::Write;
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let mut native = false;
    if !ssh {
        if clipboard.is_none() {
            *clipboard = arboard::Clipboard::new().ok();
        }
        native = clipboard.as_mut().is_some_and(|c| c.set_text(text).is_ok());
    }
    let mut out = io::stdout();
    let terminal = out
        .write_all(osc52(text).as_bytes())
        .and_then(|()| out.flush());
    if native {
        return Ok("Copied.");
    }
    match terminal {
        Ok(()) => Ok("Copied through the terminal (it may need to allow that)."),
        Err(e) => Err(format!("Could not copy ({e}).")),
    }
}

/// The OSC 52 sequence that asks the terminal to put `text` on the clipboard.
fn osc52(text: &str) -> String {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07")
}

/// Saves the clipboard's picture as a PNG in the data folder's `tmp`. Blocking: run it on a
/// worker thread. Clipboard access fails over remote sessions: that is a notice, never a crash.
fn clipboard_to_temp() -> std::result::Result<PathBuf, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|_| {
        "Cannot reach the clipboard here: attach the file with Ctrl+O instead.".to_string()
    })?;
    let img = clipboard.get_image().map_err(|_| {
        "The clipboard holds no picture: copy one first, or attach a file with Ctrl+O.".to_string()
    })?;
    let (w, h) = (img.width as u64, img.height as u64);
    if w * h > MAX_PIXELS {
        return Err("IMAGE TOO LARGE: pick a picture under 25 megapixels.".to_string());
    }
    let rgba = image::RgbaImage::from_raw(w as u32, h as u32, img.bytes.into_owned()).ok_or_else(
        || "The clipboard picture is not readable: attach a file instead.".to_string(),
    )?;
    let dir = chatcli_core::media::temp_dir()?;
    let path = dir.join(format!("paste-{}.png", now_ms()));
    rgba.save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| {
            format!("Could not save the clipboard picture ({e}): check the data folder.")
        })?;
    Ok(path)
}

/// Deletes a clipboard picture the app is done with. Only a file the paste made (its name
/// starts `paste-`, in the temp folder) is ever deleted, whatever the app asks for.
fn remove_temp(path: &Path, temp_dir: &Path) {
    let ours = path.parent() == Some(temp_dir)
        && path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("paste-"));
    if ours {
        let _ = std::fs::remove_file(path);
    } else {
        log::error("tui", "refused to delete a file outside the temp folder");
    }
}

type Input = tokio::sync::mpsc::UnboundedReceiver<io::Result<TermEvent>>;

/// Terminal events, read on a thread of their own and sent over a channel. The loop can then
/// take every event that is already waiting without any wait (`gather_burst`), which crossterm's
/// `EventStream` cannot do without leaving a stale waker behind. The thread ends when the
/// receiver is gone or the terminal fails; it may sit in `read` until the process exits.
fn spawn_input() -> Input {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || loop {
        let ev = crossterm::event::read();
        let failed = ev.is_err();
        if tx.send(ev).is_err() || failed {
            break;
        }
    });
    rx
}

/// The next input event: one set aside by `gather_burst` first, else the terminal's.
async fn next_input(
    backlog: &mut VecDeque<io::Result<TermEvent>>,
    input: &mut Input,
) -> Option<io::Result<TermEvent>> {
    match backlog.pop_front() {
        Some(e) => Some(e),
        None => input.recv().await,
    }
}

/// `first` plus every key press already waiting behind it (never waits). The first event that is
/// not a key ends the burst and is kept for the next round. Windows reports a paste as keys, so
/// the app needs them together (`burst`).
fn gather_burst(
    first: crossterm::event::KeyEvent,
    backlog: &mut VecDeque<io::Result<TermEvent>>,
    input: &mut Input,
) -> Vec<crossterm::event::KeyEvent> {
    let mut keys = vec![first];
    while keys.len() < burst::BURST_MAX {
        match input.try_recv() {
            Ok(Ok(TermEvent::Key(k))) => keys.push(k),
            Ok(other) => {
                backlog.push_back(other);
                break;
            }
            Err(_) => break,
        }
    }
    keys
}

/// Runs the full-screen app until the user quits. Restores the terminal on every way out.
pub async fn run(
    ctx: UiCtx,
    mut events: EventRx,
    commands: HashMap<Network, CommandTx>,
) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))
            .map_err(|e| format!("Terminal setup failed ({e}): restart chatcli."))?;
    let mut app = App::with_ctx(Clock::system(), &ctx);
    // Search runs on a worker thread here, so typing never waits on the store.
    app.use_search_worker();
    // The first search should not wait on the disk: read what it reads once, a moment after
    // start (so it does not slow it down), into the OS cache.
    // The wait is async, so quitting before it ends drops it rather than waiting for it.
    if let Some(store) = app.search_warm_store() {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(WARM_AFTER_SECS)).await;
            let warmed = tokio::task::spawn_blocking(move || store.warm_search()).await;
            if let Ok(Err(e)) = warmed {
                log::error("tui", &format!("warming the search index: {e}"));
            }
        });
    }
    let size = terminal
        .size()
        .map_err(|e| format!("Could not read the terminal size ({e}): restart chatcli."))?;
    app.resize(size.width, size.height);

    // Ask the terminal what it can draw: after the screen is set up, before any key is read.
    let mut graphics = gfx::detect(&ctx.settings.media).map(gfx::Graphics::new);
    let (internal_tx, mut internal_rx) = unbounded_channel::<Internal>();

    let mut input = spawn_input();
    let mut backlog: VecDeque<io::Result<TermEvent>> = VecDeque::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut backend_open = true;
    let mut toast_failed = false;
    let mut dirty = true;
    let mut result = Ok(());
    // One clipboard for the whole run: on Linux the copied text is served by this handle, so it
    // must stay alive after `y` (made on the first copy).
    let mut clipboard: Option<arboard::Clipboard> = None;

    while !app.should_quit() {
        if dirty {
            let area = match terminal.size() {
                Ok(s) => Rect::new(0, 0, s.width, s.height),
                Err(_) => Rect::new(0, 0, size.width, size.height),
            };
            // Images the terminal draws itself: encode what is on screen, and clear the screen
            // when the set of images changed so no Sixel pixels are left behind.
            let visible = if graphics.is_some() {
                let lay = app.layout_in(area);
                let vis = ui::visible_images(&app, &lay);
                // Photos for the avatars on screen, once per person (no graphics, no asking).
                let asks = app.avatar_requests(&vis.people);
                deliver(&mut app, &commands, asks);
                vis.images
            } else {
                Vec::new()
            };
            if let Some(g) = graphics.as_mut() {
                for (key, picker) in g.wanted(&visible) {
                    let max = app.max_file_bytes();
                    let k = key.clone();
                    spawn_work(
                        internal_tx.clone(),
                        move || gfx::encode(&picker, &k.path, k.cols, k.rows, max),
                        || Err(LOST.to_string()),
                        move |r| Internal::Encoded(key, r),
                    );
                }
            }
            let stale = graphics
                .as_mut()
                .map_or_else(Vec::new, |g| g.stale(&visible));
            // The whole frame is one update: a terminal with synchronized output (Windows
            // Terminal, Kitty, iTerm2, WezTerm) never shows a half-drawn screen; others ignore it.
            let _ = crossterm::execute!(io::stdout(), BeginSynchronizedUpdate);
            if !stale.is_empty() {
                // Cells an image left behind: a frame with them marked first, so the real frame
                // below differs there and every one of them is written again. Clearing the whole
                // screen instead made every scroll and chat change flash.
                let _ = terminal.draw(|f| {
                    draw(f, &app);
                    if let Some(g) = graphics.as_ref() {
                        g.render(f, &visible);
                    }
                    mark_stale(f, &stale, &visible);
                });
            }
            let drawn = terminal.draw(|f| {
                draw(f, &app);
                if let Some(g) = graphics.as_ref() {
                    g.render(f, &visible);
                }
            });
            let _ = crossterm::execute!(io::stdout(), EndSynchronizedUpdate);
            if let Err(e) = drawn {
                log::error("tui", &format!("draw failed: {e}"));
                result = Err(format!("Could not draw the screen ({e}): restart chatcli."));
                break;
            }
            dirty = false;
        }
        let mut out: Out = Vec::new();
        let wake = app
            .next_deadline()
            .map(|due| Duration::from_millis((due - now_ms()).max(MIN_WAKE_MS) as u64));
        tokio::select! {
            ev = events.recv(), if backend_open => match ev {
                Some(first) => {
                    // Drain the burst (a chat list plus updates) so it costs one redraw.
                    out.extend(app.handle_event(first));
                    while let Ok(more) = events.try_recv() {
                        out.extend(app.handle_event(more));
                    }
                    dirty = true;
                }
                None => {
                    // Without this the closed channel would wake select! in a tight loop.
                    backend_open = false;
                    log::error("tui", "backend event channel closed");
                    app.handle_event(BackendEvent::Notice {
                        network: Network::Teams,
                        text: "All backends stopped: restart chatcli.".to_string(),
                        is_error: true,
                    });
                    dirty = true;
                }
            },
            ev = next_input(&mut backlog, &mut input) => match ev {
                // The clock only moves on ticks; set it first so a timer started by this
                // input counts from now and not from the last tick. Keys that are already
                // waiting come along: Windows delivers a paste as keys.
                Some(Ok(TermEvent::Key(k))) => {
                    app.tick(now_ms());
                    let keys = gather_burst(k, &mut backlog, &mut input);
                    out = app.handle_key_burst(keys);
                    dirty = true;
                }
                Some(Ok(TermEvent::Mouse(m))) => { app.tick(now_ms()); out = app.handle_mouse(m); dirty = true; }
                Some(Ok(TermEvent::Paste(s))) => { out = app.handle_paste(&s); dirty = true; }
                // A terminal that never reports focus leaves the window counted as unfocused.
                Some(Ok(TermEvent::FocusGained)) => { app.set_focus(true); dirty = true; }
                Some(Ok(TermEvent::FocusLost)) => { app.set_focus(false); dirty = true; }
                Some(Ok(TermEvent::Resize(w, h))) => { app.tick(now_ms()); app.resize(w, h); dirty = true; }
                Some(Err(e)) => {
                    log::error("tui", &format!("terminal input failed: {e}"));
                    result = Err(format!("Terminal input failed ({e}): restart chatcli."));
                    break;
                }
                None => break,
            },
            // Answers from worker threads (a thumbnail, the clipboard, an encoded image). The
            // loop holds a sender, so this never reports a closed channel.
            Some(msg) = internal_rx.recv() => {
                match msg {
                    Internal::Thumb(job, r) => app.thumb_ready(&job, r),
                    Internal::Clipboard(net, chat, r) => app.clipboard_done(net, &chat, r),
                    Internal::Search(id, seq, r) => app.search_done(id, seq, r),
                    Internal::Encoded(key, r) => {
                        if let Some(g) = graphics.as_mut() {
                            g.store(key, r);
                        }
                    }
                }
                dirty = true;
            },
            _ = tick.tick() => {
                if app.tick(now_ms()) {
                    dirty = true;
                }
            }
            // Armed only while a preview timer is pending; the floor keeps it under 10 wakeups
            // a second, and `fire_timers` clears whatever is due so it cannot spin.
            _ = async {
                match wake {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            } => {
                app.tick(now_ms());
                out = app.fire_timers();
                dirty = true;
            }
        }
        // A steady stream of backend events (history sync, Teams catch-up) can win every
        // select! round and starve the sleep branch above, so a due timer is also checked here,
        // after whichever branch ran.
        if app.refill_due() || app.next_deadline().is_some_and(|due| due <= now_ms()) {
            app.tick(now_ms());
            out.extend(app.fire_timers());
            dirty = true;
        }
        // Toasts for new messages: shown here because App is pure.
        for toast in app.take_toasts() {
            show_toast(&mut app, &ctx, &mut toast_failed, toast);
            dirty = true;
        }
        // Links the user confirmed: opened here because App is pure.
        for url in app.take_open_urls() {
            open_link(&mut app, ctx.demo, &url);
            dirty = true;
        }
        // Files to open with the OS, pictures to decode, the clipboard, finished temp files.
        for job in app.take_open_files() {
            let r = open_file_or_demo(ctx.demo, &job.path);
            app.open_done(&job, r);
            if ctx.demo {
                app.push_global_notice("demo: a real run opens that file in its app.", false);
            }
            dirty = true;
        }
        let (depth, max) = (app.color_depth(), app.max_file_bytes());
        for job in app.take_decodes() {
            let path = job.path.clone();
            spawn_work(
                internal_tx.clone(),
                move || make_thumb(&path, depth, max),
                || Err(LOST.to_string()),
                move |r| Internal::Thumb(job, r),
            );
            dirty = true;
        }
        if let Some((net, text)) = app.take_copy() {
            let r = copy_text(&mut clipboard, &text);
            app.copy_done(net, r);
            dirty = true;
        }
        // The search box's next query, one at a time (`App::take_search`).
        if let Some(job) = app.take_search() {
            let (id, seq) = (job.id, job.seq);
            spawn_work(
                internal_tx.clone(),
                move || job.run(),
                || Err("The search stopped: type again to retry.".to_string()),
                move |r| Internal::Search(id, seq, r),
            );
        }
        if let Some((net, chat)) = app.take_clipboard_request() {
            spawn_work(
                internal_tx.clone(),
                clipboard_to_temp,
                || Err(LOST.to_string()),
                move |r| Internal::Clipboard(net, chat, r),
            );
        }
        // Settings changed on the screen: written here because App is pure. Without a file
        // (tests) the edits are dropped.
        match &ctx.settings_path {
            Some(path) => dirty |= save_pending(&mut app, path),
            None => drop(app.take_setting_edits()),
        }
        let trash = app.take_trash();
        if !trash.is_empty() {
            if let Ok(dir) = chatcli_core::media::temp_dir() {
                trash.iter().for_each(|p| remove_temp(p, &dir));
            }
        }
        if !out.is_empty() {
            deliver(&mut app, &commands, out);
            dirty = true;
        }
    }

    // Lets Teams delete its Graph subscription; harmless if a backend already ended.
    for tx in commands.values() {
        let _ = tx.send(Command::Shutdown);
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn stale_cells_are_marked_except_under_a_current_image() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::style::Modifier;
        let mut term = ratatui::Terminal::new(TestBackend::new(20, 8)).unwrap();
        let now = [crate::ui::VisibleImage {
            path: "a.png".into(),
            rect: Rect::new(2, 2, 4, 2),
            rows: 2,
            skip: 0,
        }];
        // The image moved down one row: its old box was rows 1-2.
        term.draw(|f| super::mark_stale(f, &[Rect::new(2, 1, 4, 2)], &now))
            .unwrap();
        let buf = term.backend().buffer();
        let marked = |x, y| buf[(x, y)].modifier.contains(Modifier::HIDDEN);
        assert!(marked(2, 1) && marked(5, 1), "the row the image left");
        assert!(
            !marked(2, 2) && !marked(5, 2),
            "not under the image's new box"
        );
        assert!(
            !marked(1, 1) && !marked(6, 1) && !marked(2, 0),
            "nothing outside the old box"
        );
    }

    use super::{
        hook_restores_terminal, open_file_or_demo, remove_temp, spawn_work, validate_settings,
        Internal,
    };
    use std::path::PathBuf;

    #[test]
    fn osc52_carries_the_text_as_base64() {
        assert_eq!(crate::osc52("hi"), "\x1b]52;c;aGk=\x07");
        assert_eq!(crate::osc52(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn the_demo_never_opens_a_file_in_an_app() {
        let missing = PathBuf::from("Z:/no/such/file.txt");
        assert!(open_file_or_demo(true, &missing).is_ok());
        assert!(open_file_or_demo(false, &missing).is_err());
    }

    /// An app with one queued edit: density set to compact on the Layout page.
    fn app_with_a_density_edit() -> crate::App {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = crate::App::new(crate::Clock {
            now_ms: 1_800_000_000_000,
            tz: chrono::FixedOffset::east_opt(0),
        });
        for code in [
            KeyCode::F(2),
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Right,
        ] {
            app.handle_key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        }
        app
    }

    fn temp_settings(name: &str, text: Option<&str>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chatcli-save-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        if let Some(t) = text {
            std::fs::write(&path, t).unwrap();
        }
        path
    }

    #[test]
    fn save_pending_writes_the_queued_edits_and_keeps_the_rest_of_the_file() {
        let path = temp_settings(
            "ok",
            Some("# mine\n[layout]\ntime_format = \"12h\" # keep\n"),
        );
        let mut app = app_with_a_density_edit();
        assert!(super::save_pending(&mut app, &path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine") && text.contains("# keep"), "{text}");
        assert!(text.contains("density = \"compact\""), "{text}");
        let report = chatcli_core::settings::load_from(&path);
        assert!(report.problems.is_empty());
        assert_eq!(
            report.settings.layout.density,
            chatcli_core::settings::Density::Compact
        );
        assert!(
            !super::save_pending(&mut app, &path),
            "the queue is empty now"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_pending_creates_a_missing_file() {
        let path = temp_settings("new", None);
        let mut app = app_with_a_density_edit();
        assert!(super::save_pending(&mut app, &path));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("density = \"compact\""));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_pending_never_writes_a_broken_file_and_says_so() {
        let broken = "[layout\ndensity = \n";
        let path = temp_settings("broken", Some(broken));
        let mut app = app_with_a_density_edit();
        assert!(super::save_pending(&mut app, &path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let notice = app.current_notice().expect("a notice");
        assert!(
            notice.is_error && notice.text.starts_with("NOT SAVED"),
            "{notice:?}"
        );
        assert!(app.take_setting_edits().is_empty(), "not retried forever");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn settings_check_reports_theme_and_key_problems_together() {
        let mut s = chatcli_core::settings::Settings::default();
        s.theme.overrides.insert("dim".into(), "banana".into());
        s.keys
            .bindings
            .entry("list".into())
            .or_default()
            .insert("ctrl+n".into(), "lst_down".into());
        let texts: Vec<String> = validate_settings(&s).iter().map(|p| p.text()).collect();
        assert!(
            texts.iter().any(|t| t.contains("[theme] dim:")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("[keys.list] ctrl+n:")),
            "{texts:?}"
        );
        assert!(validate_settings(&chatcli_core::settings::Settings::default()).is_empty());
    }

    #[test]
    fn backend_panics_leave_the_terminal_alone() {
        assert!(hook_restores_terminal(true, true), "UI thread panic");
        assert!(
            !hook_restores_terminal(false, true),
            "backend panic while the UI runs"
        );
        assert!(
            hook_restores_terminal(false, false),
            "any panic after the UI ended"
        );
    }

    #[tokio::test]
    async fn a_panicking_worker_still_answers() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_work(
            tx.clone(),
            || -> std::result::Result<chatcli_core::model::Network, String> {
                panic!("decoder bug")
            },
            || Err("lost".to_string()),
            |r| {
                Internal::Clipboard(
                    chatcli_core::model::Network::Teams,
                    "a".into(),
                    r.map(|_| PathBuf::new()),
                )
            },
        );
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Some(Internal::Clipboard(_, _, r))) => assert_eq!(r, Err("lost".to_string())),
            other => panic!("no answer: {}", other.is_ok()),
        }
        // And a worker that works says what it made.
        spawn_work(
            tx,
            || Ok::<_, String>(PathBuf::from("x.png")),
            || Err("lost".to_string()),
            |r| Internal::Clipboard(chatcli_core::model::Network::Teams, "a".into(), r),
        );
        match rx.recv().await {
            Some(Internal::Clipboard(_, _, r)) => assert_eq!(r, Ok(PathBuf::from("x.png"))),
            _ => panic!("no answer"),
        }
    }

    #[test]
    fn only_paste_files_in_the_temp_folder_are_ever_deleted() {
        let dir = std::env::temp_dir().join(format!("chatcli-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let other = dir.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let mine = dir.join("paste-1.png");
        let not_mine = dir.join("holiday.png");
        let outside = other.join("paste-2.png");
        for p in [&mine, &not_mine, &outside] {
            std::fs::write(p, b"x").unwrap();
        }
        for p in [&mine, &not_mine, &outside] {
            remove_temp(p, &dir);
        }
        assert!(!mine.exists(), "a paste file in the temp folder goes");
        assert!(not_mine.exists(), "another name stays");
        assert!(outside.exists(), "another folder stays");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
