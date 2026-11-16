//! Terminal UI for chatcli: a tab per network, every chat in a sidebar, message pane and
//! composer. It talks to the backends only through the `Event` / `Command` channels in
//! `chatcli_core::model` and never touches a network.
//!
//! - `app`: all state and input handling (pure, tested without a terminal)
//! - `ui`: drawing
//! - `text`: width-aware helpers and the clock
//! - `demo`: fake backends for `chatcli --demo`

mod app;
pub mod demo;
mod text;
mod ui;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chatcli_core::model::{now_ms, Command, CommandTx, Event as BackendEvent, EventRx, Network};
use chatcli_core::{log, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub use app::{App, Focus, Notice, Out};
pub use text::Clock;
pub use ui::draw;

/// Puts the terminal back as the shell expects it. Safe to call twice.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
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
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
            .map_err(|e| format!("Could not start the full-screen view ({e}): run chatcli in a normal terminal window."))?;
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

fn deliver(app: &mut App, commands: &HashMap<Network, CommandTx>, out: Out) {
    for (net, cmd) in out {
        let delivered = commands.get(&net).is_some_and(|tx| tx.send(cmd).is_ok());
        if !delivered {
            log::error("tui", &format!("{} backend is not running", net.key()));
            app.push_notice(net, "Backend stopped: restart chatcli.", true);
        }
    }
}

/// Runs the full-screen app until the user quits. Restores the terminal on every way out.
pub async fn run(mut events: EventRx, commands: HashMap<Network, CommandTx>) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))
            .map_err(|e| format!("Terminal setup failed ({e}): restart chatcli."))?;
    let mut app = App::new(Clock::system());
    let size = terminal
        .size()
        .map_err(|e| format!("Could not read the terminal size ({e}): restart chatcli."))?;
    app.resize(size.width, size.height);

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut backend_open = true;
    let mut dirty = true;
    let mut result = Ok(());

    while !app.should_quit() {
        if dirty {
            if let Err(e) = terminal.draw(|f| draw(f, &app)) {
                log::error("tui", &format!("draw failed: {e}"));
                result = Err(format!("Could not draw the screen ({e}): restart chatcli."));
                break;
            }
            dirty = false;
        }
        let mut out: Out = Vec::new();
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
            ev = input.next() => match ev {
                Some(Ok(TermEvent::Key(k))) => { out = app.handle_key(k); dirty = true; }
                Some(Ok(TermEvent::Mouse(m))) => { out = app.handle_mouse(m); dirty = true; }
                Some(Ok(TermEvent::Paste(s))) => { app.handle_paste(&s); dirty = true; }
                Some(Ok(TermEvent::Resize(w, h))) => { app.resize(w, h); dirty = true; }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    log::error("tui", &format!("terminal input failed: {e}"));
                    result = Err(format!("Terminal input failed ({e}): restart chatcli."));
                    break;
                }
                None => break,
            },
            _ = tick.tick() => {
                if app.tick(now_ms()) {
                    dirty = true;
                }
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
    use super::hook_restores_terminal;

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
}
