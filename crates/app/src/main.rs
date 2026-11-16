//! chatcli entry point. No arguments: the full-screen client. Otherwise a subcommand.

mod cli;
mod doctor;
mod toast;

use std::collections::HashMap;
use std::fs::File;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use chatcli_core::media::{self, MediaCache};
use chatcli_core::model::{Command, Network};
use chatcli_core::runtime::{BackendCtx, NoNotifier, StartupNotice, UiCtx};
use chatcli_core::settings::{self, SettingsProblem, Severity};
use chatcli_core::store::Store;
use chatcli_core::{lock, log, paths, Result};
use tokio::sync::mpsc;

/// Backends get this long to clean up (Teams deletes its Graph subscription) after the UI exits.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

const ALREADY_RUNNING: &str = "chatcli is already running";

/// What a command that changes sign-in state says when the full client holds the lock.
const COMMAND_BLOCKED: &str =
    "chatcli is running: quit it (q or Ctrl+C), then run this command again.";

/// Holds an exclusive OS file lock while this chatcli runs, so a second one stops. The OS frees
/// the lock when the process dies, so a crash never leaves a stale lock.
fn single_instance() -> Result<File> {
    let path = paths::data_dir()?.join("chatcli.lock");
    match lock::try_acquire(&path) {
        Ok(Some(file)) => Ok(file),
        Ok(None) => Err(ALREADY_RUNNING.to_string()),
        Err(e) => {
            // No access or a read-only folder is a different problem the user must see.
            let text = lock_error(&e, &path);
            log::error("app", &text);
            Err(text)
        }
    }
}

/// The lock for a command that reads or writes the shared sign-in (`teams login`, `relay setup`,
/// ...). A running client may be mid token refresh: a logout in between would delete the
/// credential and the refresh would then store a new one, so the sign-in would come back after
/// "Signed out". Held for the whole command.
fn lock_for_command() -> Result<File> {
    single_instance().map_err(|e| command_lock_message(&e))
}

/// Pure so the wording is tested: only "already running" changes, other lock errors pass through.
fn command_lock_message(e: &str) -> String {
    if e == ALREADY_RUNNING {
        COMMAND_BLOCKED.to_string()
    } else {
        e.to_string()
    }
}

/// The message for a lock file that could not be opened at all (not "already running"). Pure, so
/// its test never writes to the real log.
fn lock_error(e: &std::io::Error, path: &std::path::Path) -> String {
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

/// The options of the full-screen client: `chatcli [--demo] [--tour]`, any order.
#[derive(Debug, Default, PartialEq, Eq)]
struct RunArgs {
    demo: bool,
    /// `--tour`: the guided tour. Implies the demo.
    tour: bool,
}

/// None = these arguments are not the full-screen client's (a subcommand, or a typo: the usage
/// text is printed).
fn parse_run_args(args: &[&str]) -> Option<RunArgs> {
    let mut out = RunArgs::default();
    for arg in args {
        match *arg {
            "--demo" => out.demo = true,
            "--tour" => {
                out.tour = true;
                out.demo = true;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One start-up notice per `settings.toml` problem from core. The TUI adds its own `[keys]`
/// and `[theme]` problems and folds everything into one notice line.
fn startup_notices(problems: &[SettingsProblem]) -> Vec<StartupNotice> {
    problems
        .iter()
        .map(|p| StartupNotice {
            text: p.text(),
            is_error: p.severity == Severity::Error,
        })
        .collect()
}

/// Start-up upkeep: age and size eviction of the media cache, and old pasted pictures. A failure
/// is logged and never stops the client.
fn housekeeping(cache: &MediaCache) {
    let now = now_ms();
    if let Err(e) = cache.evict(now) {
        log::error("app", &format!("media cache cleanup failed: {e}"));
    }
    if let Err(e) = media::clean_temp(now) {
        log::error("app", &format!("temp folder cleanup failed: {e}"));
    }
}

async fn full_client() -> Result<()> {
    let report = settings::load();
    let settings = Arc::new(report.settings.clone());
    let db = paths::store_db()?;
    // The UI's connection opens first: it runs the schema migration once, before the two
    // backends open theirs.
    let ui_store = Store::open(&db)?;
    let cache = MediaCache::open(&paths::media_dir()?, &settings.media)?;
    housekeeping(&cache);

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut commands = HashMap::new();
    let (teams_tx, teams_rx) = mpsc::unbounded_channel();
    let (wa_tx, wa_rx) = mpsc::unbounded_channel();
    commands.insert(Network::Teams, teams_tx.clone());
    commands.insert(Network::WhatsApp, wa_tx.clone());
    // One connection per task: each backend and the UI own their Store.
    let backend_ctx = |store: Store| BackendCtx {
        store,
        settings: settings.clone(),
        media: cache.clone(),
    };
    let teams = tokio::spawn(chatcli_teams::run(
        backend_ctx(Store::open(&db)?),
        teams_rx,
        events_tx.clone(),
    ));
    let whatsapp = tokio::spawn(chatcli_whatsapp::run(
        backend_ctx(Store::open(&db)?),
        wa_rx,
        events_tx,
    ));

    let ctx = UiCtx {
        settings: settings.clone(),
        startup_notices: startup_notices(&report.problems),
        store: ui_store,
        media: cache,
        notifier: toast::system_notifier(),
        demo: false,
        settings_path: (!report.path.as_os_str().is_empty()).then(|| report.path.clone()),
        tour: false,
    };
    let result = chatcli_tui::run(ctx, events_rx, commands).await;

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

/// What the demo's own settings file starts with: the tour starts from the defaults, a plain
/// demo from a copy of the user's file (`real` = its text, None when missing), so themes, layout
/// and keys can be tried. Pure.
fn demo_settings_text(tour: bool, real: Option<String>) -> String {
    if tour {
        String::new()
    } else {
        real.unwrap_or_default()
    }
}

/// The demo keeps its messages in memory and its media cache and `settings.toml` copy in a temp
/// folder that is deleted on exit. It only ever reads the real `settings.toml`: changes made on
/// the settings screen go to the copy. It never touches the real store, the real cache or the
/// network.
async fn demo(tour: bool) -> Result<()> {
    let real_path = settings::load().path;
    let dir = std::env::temp_dir().join(format!("chatcli-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let real = std::fs::read_to_string(&real_path).ok();
    let copy = dir.join("settings.toml");
    std::fs::write(&copy, demo_settings_text(tour, real))
        .map_err(|e| format!("cannot write {}: {e}", copy.display()))?;
    let report = settings::load_from(&copy);
    let settings = Arc::new(report.settings.clone());
    let cache = MediaCache::open(&dir.join("media"), &settings.media)?;
    let store = Store::open_in_memory()?;
    let (events, commands) = chatcli_tui::demo::spawn(store.clone(), cache.clone());
    let ctx = UiCtx {
        settings,
        startup_notices: startup_notices(&report.problems),
        store,
        media: cache,
        notifier: Arc::new(NoNotifier),
        demo: true,
        settings_path: Some(copy),
        tour,
    };
    let result = chatcli_tui::run(ctx, events, commands).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
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
    if let ["--version"] = args.as_slice() {
        println!("chatcli {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let ["--help"] | ["-h"] | ["help"] = args.as_slice() {
        for line in cli::USAGE {
            println!("{line}");
        }
        println!("{}", cli::log_line());
        return;
    }
    if let Some(run) = parse_run_args(&args) {
        if let Err(e) = require_terminal() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        let result = if run.demo {
            demo(run.tour).await
        } else {
            let _lock = single_instance().unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            });
            full_client().await
        };
        if let Err(e) = result {
            fail(&e);
        }
        return;
    }
    // Commands that touch the shared sign-in or the media cache hold the same lock as the full
    // client, so they never run beside it.
    let _lock = if cli::needs_instance_lock(&args) {
        Some(lock_for_command().unwrap_or_else(|e| fail(&e)))
    } else {
        None
    };
    match cli::run(&args).await {
        Some(Ok(())) => {}
        Some(Err(e)) => fail(&e),
        None => {
            for line in cli::USAGE {
                eprintln!("{line}");
            }
            eprintln!("{}", cli::log_line());
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Error;
    use std::path::Path;

    #[test]
    fn an_unopenable_lock_file_names_the_folder() {
        let p = Path::new("data/chatcli.lock");
        let text = lock_error(&Error::from_raw_os_error(5), p);
        assert!(
            text.starts_with("Cannot create chatcli.lock in data"),
            "{text}"
        );
    }

    #[test]
    fn run_args_accept_the_flags() {
        assert_eq!(parse_run_args(&[]), Some(RunArgs::default()));
        assert_eq!(
            parse_run_args(&["--demo"]),
            Some(RunArgs {
                demo: true,
                tour: false
            })
        );
    }

    #[test]
    fn the_demo_settings_copy_is_the_users_file_or_empty_for_the_tour() {
        let real = || {
            Some(
                "[layout]
density = \"compact\"
"
                .to_string(),
            )
        };
        assert_eq!(demo_settings_text(true, real()), "");
        assert_eq!(demo_settings_text(false, real()), real().unwrap());
        assert_eq!(demo_settings_text(false, None), "");
    }

    #[test]
    fn the_tour_flag_implies_the_demo_in_any_order() {
        let tour = Some(RunArgs {
            demo: true,
            tour: true,
        });
        assert_eq!(parse_run_args(&["--tour"]), tour);
        assert_eq!(parse_run_args(&["--demo", "--tour"]), tour);
        assert_eq!(parse_run_args(&["--tour", "--demo"]), tour);
        assert_eq!(parse_run_args(&["--tour", "extra"]), None);
    }

    #[test]
    fn run_args_reject_everything_else() {
        // Key presets are gone, and with them `--keys`.
        assert_eq!(parse_run_args(&["--keys"]), None);
        assert_eq!(parse_run_args(&["--keys", "vim"]), None);
        assert_eq!(parse_run_args(&["--demo", "extra"]), None);
        assert_eq!(parse_run_args(&["doctor"]), None);
        assert_eq!(parse_run_args(&["teams", "login"]), None);
    }

    #[test]
    fn a_command_blocked_by_the_client_says_what_to_do() {
        assert_eq!(command_lock_message(ALREADY_RUNNING), COMMAND_BLOCKED);
        assert_eq!(
            command_lock_message("Cannot create chatcli.lock in x: denied."),
            "Cannot create chatcli.lock in x: denied."
        );
    }

    #[test]
    fn each_settings_problem_becomes_one_notice_with_its_severity() {
        let report = settings::parse(
            "[layout]\ndensity = \"tiny\"\n[teams]\nfiles_reed = true\n",
            Path::new("settings.toml"),
        );
        let notices = startup_notices(&report.problems);
        assert_eq!(notices.len(), 2);
        assert!(notices[0].is_error && notices[0].text.contains("[layout] density"));
        assert!(!notices[1].is_error && notices[1].text.contains("unknown setting"));
    }
}
