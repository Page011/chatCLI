//! chatcli entry point. No arguments: the full-screen client. Otherwise a subcommand.

mod cli;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::os::windows::fs::OpenOptionsExt;
use std::time::Duration;

use chatcli_core::model::{Command, Network};
use chatcli_core::store::Store;
use chatcli_core::{log, paths, Result};
use tokio::sync::mpsc;

/// Backends get this long to clean up (Teams deletes its Graph subscription) after the UI exits.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Opened with share mode 0, so a second chatcli cannot open it while this one runs. Windows
/// frees the handle when the process dies, so a crash never leaves a stale lock.
fn single_instance() -> Result<File> {
    let path = paths::data_dir()?.join("chatcli.lock");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&path)
        .map_err(|e| {
            let text = lock_error(&e, &path);
            if e.raw_os_error() != Some(32) {
                log::error("app", &text);
            }
            text
        })
}

/// ERROR_SHARING_VIOLATION means another chatcli holds the lock; anything else (no access, a
/// read-only folder) is a different problem the user must see (the caller logs it). Pure, so
/// its test never writes to the real log.
fn lock_error(e: &std::io::Error, path: &std::path::Path) -> String {
    if e.raw_os_error() == Some(32) {
        return "chatcli is already running".to_string();
    }
    let dir = path.parent().unwrap_or(path).display();
    format!("Cannot create chatcli.lock in {dir}: {e}. Check the folder is writable.")
}

fn require_terminal() -> Result<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(())
    } else {
        Err("chatcli needs an interactive terminal: run it from Windows Terminal or a console window.".to_string())
    }
}

async fn full_client() -> Result<()> {
    let db = paths::store_db()?;
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut commands = HashMap::new();
    let (teams_tx, teams_rx) = mpsc::unbounded_channel();
    let (wa_tx, wa_rx) = mpsc::unbounded_channel();
    commands.insert(Network::Teams, teams_tx.clone());
    commands.insert(Network::WhatsApp, wa_tx.clone());
    // One connection per backend: each task owns its Store.
    let teams = tokio::spawn(chatcli_teams::run(
        Store::open(&db)?,
        teams_rx,
        events_tx.clone(),
    ));
    let whatsapp = tokio::spawn(chatcli_whatsapp::run(Store::open(&db)?, wa_rx, events_tx));

    let result = chatcli_tui::run(events_rx, commands).await;

    let _ = teams_tx.send(Command::Shutdown);
    let _ = wa_tx.send(Command::Shutdown);
    let joined = tokio::time::timeout(SHUTDOWN_WAIT, async {
        let _ = teams.await;
        let _ = whatsapp.await;
    })
    .await;
    if joined.is_err() {
        log::error("app", "a backend did not stop within 5 s");
    }
    result
}

async fn demo() -> Result<()> {
    let (events, commands) = chatcli_tui::demo::spawn();
    chatcli_tui::run(events, commands).await
}

fn fail(e: &str) -> ! {
    log::error("app", e);
    eprintln!("error: {e}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--version"] => println!("chatcli {}", env!("CARGO_PKG_VERSION")),
        [] => {
            if let Err(e) = require_terminal() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            let _lock = single_instance().unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            });
            if let Err(e) = full_client().await {
                fail(&e);
            }
        }
        ["--demo"] => {
            if let Err(e) = require_terminal() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            if let Err(e) = demo().await {
                fail(&e);
            }
        }
        _ => match cli::run(&args).await {
            Some(Ok(())) => {}
            Some(Err(e)) => fail(&e),
            None => {
                for line in cli::USAGE {
                    eprintln!("{line}");
                }
                std::process::exit(2);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::lock_error;
    use std::io::Error;
    use std::path::Path;

    #[test]
    fn only_a_sharing_violation_means_already_running() {
        let p = Path::new("data/chatcli.lock");
        assert_eq!(
            lock_error(&Error::from_raw_os_error(32), p),
            "chatcli is already running"
        );
        let other = lock_error(&Error::from_raw_os_error(5), p);
        assert!(
            other.starts_with("Cannot create chatcli.lock in data"),
            "{other}"
        );
    }
}
