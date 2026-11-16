//! Plain command-line subcommands (`chatcli teams ...`). Kept beside the TUI for scripting
//! and for debugging one network without the full screen.

use std::io::Write;
use std::path::Path;

use chatcli_core::media::MediaCache;
use chatcli_core::model::Network;
use chatcli_core::settings::{self, LoadReport, SettingsProblem, Severity};
use chatcli_core::store::{SearchHit, Store};
use chatcli_core::{config, paths, Result};
use chatcli_teams::{auth, graph::Graph, graph::MAX_PAGE};

/// Most hits `chatcli search` prints.
const SEARCH_LIMIT: u32 = 50;

pub const USAGE: &[&str] = &[
    "usage:",
    "  chatcli                                  open the full-screen chat client",
    "  chatcli --demo [--tour]                  try the screen with fake data (no store, no network); --tour plays a guided tour",
    "  chatcli --version                        print the version",
    "  chatcli relay setup <url> [--rotate]     make the relay secrets (refuses to replace working ones)",
    "  chatcli relay url <url>                  change the relay URL, keep the secrets",
    "  chatcli relay status                     show the relay URL and whether its secrets exist",
    "  chatcli teams setup <client-id> <tenant> save your Entra app registration ids",
    "  chatcli teams login                      sign in through the browser",
    "  chatcli teams chats                      1:1 and group chats, newest first",
    "  chatcli teams messages <chat-id> [n]     last n messages (default 20, max 50)",
    "  chatcli teams send <chat-id> <text...>   send a message (asks to confirm)",
    "  chatcli teams logout                     forget the stored sign-in",
    "  chatcli settings path                    print where settings.toml lives",
    "  chatcli settings check                   check settings.toml (exit 1 on errors)",
    "  chatcli doctor [--online] [--json]       check setup and print fixes (exit 1 on a failure)",
    "  chatcli search <text...>                 search every stored message on this PC",
    "  chatcli media clear                      delete the downloaded media cache",
    "Times are UTC.",
];

/// The last line of `--help`: the log file of this OS, without creating the data folder.
pub fn log_line() -> String {
    match paths::log_file_path() {
        Ok(p) => format!("Log: {}", p.display()),
        Err(_) => "Log: chatcli.log in the data folder".to_string(),
    }
}

/// Commands that read or write the shared sign-in, or delete cache files a running client may
/// be writing: they must not run beside the full client (main.rs takes its lock first). Read-only
/// ones (`relay status`, `settings`, `search`, `doctor`) never need it, and `doctor` reports the
/// lock itself.
pub fn needs_instance_lock(args: &[&str]) -> bool {
    matches!(
        args,
        ["relay", "setup", ..]
            | ["relay", "url", ..]
            | ["teams", "setup", ..]
            | [
                "teams",
                "login" | "logout" | "chats" | "messages" | "send",
                ..
            ]
            | ["media", "clear"]
    )
}

/// Runs a subcommand. None = the arguments are not a known subcommand.
pub async fn run(args: &[&str]) -> Option<Result<()>> {
    Some(match args {
        ["relay", "setup", url] => relay_setup(url, false),
        ["relay", "setup", url, "--rotate"] | ["relay", "setup", "--rotate", url] => {
            relay_setup(url, true)
        }
        ["relay", "url", url] => relay_url(url),
        ["relay", "status"] => relay_status(),
        ["media", "clear"] => media_clear(),
        ["search", words @ ..] if !words.is_empty() => search(&words.join(" ")),
        ["teams", "setup", client_id, tenant] => teams_setup(client_id, tenant),
        ["doctor", flags @ ..] => crate::doctor::run(flags).await,
        ["settings", "path"] => settings_path(),
        ["settings", "check"] => settings_check(),
        ["teams", "login"] => login().await,
        ["teams", "logout"] => auth::sign_out().map(|()| println!("Signed out of Teams.")),
        ["teams", "chats"] => chats().await,
        ["teams", "messages", chat_id] => messages(chat_id, 20).await,
        ["teams", "messages", chat_id, n] => match n.parse::<u32>() {
            Ok(n) if (1..=MAX_PAGE).contains(&n) => messages(chat_id, n).await,
            _ => Err(format!("n must be a number from 1 to {MAX_PAGE}")),
        },
        ["teams", "send", chat_id, text @ ..] if !text.is_empty() => {
            send(chat_id, &text.join(" ")).await
        }
        _ => return None,
    })
}

fn settings_path() -> Result<()> {
    println!("{}", paths::settings_file()?.display());
    Ok(())
}

fn settings_check() -> Result<()> {
    check_settings_file(&paths::settings_file()?)
}

/// Core's problems plus the ones only the TUI can see (`[keys]` bindings, `[theme]` roles), so
/// `settings check` and `doctor` report what the screen would show at start.
pub fn with_tui_problems(mut report: LoadReport) -> LoadReport {
    report
        .problems
        .extend(chatcli_tui::validate_settings(&report.settings));
    report
}

/// Prints every problem in the file and fails when at least one is an error (exit code 1);
/// warnings alone pass (exit code 0).
fn check_settings_file(path: &Path) -> Result<()> {
    let report = with_tui_problems(settings::load_from(path));
    let (lines, errors) = problem_lines(path, &report.problems);
    for line in lines {
        println!("{line}");
    }
    if errors > 0 {
        return Err(format!(
            "{errors} error(s) in settings.toml: fix them and run chatcli settings check again."
        ));
    }
    Ok(())
}

/// One line per problem, then a summary line; and how many of them are errors. Pure.
fn problem_lines(path: &Path, problems: &[SettingsProblem]) -> (Vec<String>, usize) {
    let errors = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .count();
    let mut lines: Vec<String> = problems
        .iter()
        .map(|p| {
            let level = match p.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            format!("{level}: {}", p.text())
        })
        .collect();
    let warnings = problems.len() - errors;
    lines.push(if problems.is_empty() {
        format!("{}: no problems.", path.display())
    } else {
        format!(
            "{}: {errors} error(s), {warnings} warning(s).",
            path.display()
        )
    });
    (lines, errors)
}

fn teams_setup(client_id: &str, tenant: &str) -> Result<()> {
    let (client_id, tenant) = (client_id.trim(), tenant.trim());
    if !config::is_guid(client_id) {
        return Err(format!(
            "'{client_id}' is not an Application (client) ID: expected 8-4-4-4-12 hex digits"
        ));
    }
    if !config::is_tenant(tenant) {
        return Err(format!(
            "'{tenant}' is not a Directory (tenant) ID or domain"
        ));
    }
    let mut cfg = config::load()?;
    cfg.teams_client_id = Some(client_id.to_string());
    cfg.teams_tenant_id = Some(tenant.to_string());
    config::save(&cfg)?;
    println!("Saved. Next: chatcli teams login");
    Ok(())
}

fn relay_setup(url: &str, rotate: bool) -> Result<()> {
    // Stored secrets without --rotate come back as the fixed "use --rotate" sentence: main
    // prints it and exits 1, and nothing was changed.
    let made = chatcli_teams::relay_setup(url, rotate)?;
    println!(
        "Relay URL saved and two secrets made (kept in {}).",
        chatcli_core::secrets::store_name()
    );
    println!("Next, from the chatCLI folder:");
    println!("  1. cd relay");
    println!("  2. bunx wrangler secret put RELAY_TOKEN");
    println!("     paste this value when asked: {}", made.token);
    println!("  3. bunx wrangler secret put CLIENT_STATE");
    println!("     paste this value when asked: {}", made.client_state);
    println!("  4. restart chatcli");
    println!("These values are shown once. Do not share them or paste them anywhere else.");
    println!("Clear this screen or close the terminal when you are done.");
    Ok(())
}

fn relay_url(url: &str) -> Result<()> {
    chatcli_teams::relay_set_url(url)?;
    println!("Relay URL changed. The two secrets are kept: nothing to redo on the Worker.");
    Ok(())
}

fn media_clear() -> Result<()> {
    let cache = MediaCache::open(&paths::media_dir()?, &settings::load().settings.media)?;
    let freed = cache.clear()?;
    println!("Media cache emptied: {}.", megabytes(freed));
    Ok(())
}

/// "0.0 MB" style size for the cache summary.
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn search(text: &str) -> Result<()> {
    let db = paths::store_db()?;
    if !db.exists() {
        return Err("No store yet: start chatcli once, then search.".to_string());
    }
    let hits = Store::open_read_only(&db)?.search(text, None, SEARCH_LIMIT)?;
    for line in hit_lines(&hits) {
        println!("{line}");
    }
    Ok(())
}

/// One line per hit, newest first as the store returns them: `time network chat: sender: text`.
/// The matched words sit in square brackets. Printed to the terminal only, never logged. Pure.
fn hit_lines(hits: &[SearchHit]) -> Vec<String> {
    if hits.is_empty() {
        return vec!["No messages match.".to_string()];
    }
    let mut lines: Vec<String> = hits
        .iter()
        .map(|h| {
            let net = match h.network {
                Network::Teams => "Teams",
                Network::WhatsApp => "WhatsApp",
            };
            let snippet: String = h
                .snippet
                .iter()
                .map(|p| {
                    if p.hit {
                        format!("[{}]", p.text)
                    } else {
                        p.text.clone()
                    }
                })
                .collect();
            format!(
                "{} {net} {}: {}: {}",
                utc_minute(h.ts),
                clip(&h.chat_title, 30),
                clip(&h.sender, 20),
                snippet
            )
        })
        .collect();
    if hits.len() as u32 >= SEARCH_LIMIT {
        lines.push(format!(
            "Newest {SEARCH_LIMIT} shown: use more words to narrow it."
        ));
    }
    lines
}

/// Milliseconds since 1970 as "2026-11-16 01:02Z". Command-line times are UTC.
fn utc_minute(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_else(|| "(unknown time)".to_string())
}

fn relay_status() -> Result<()> {
    use chatcli_core::secrets;
    use chatcli_teams::relay::{CLIENT_STATE_NAME, TOKEN_NAME};
    let url = config::load()?.relay_url;
    let has = |name| secrets::read(secrets::SERVICE, name).map(|v| v.is_some());
    println!("relay_url:    {}", url.as_deref().unwrap_or("(not set)"));
    println!(
        "relay token:  {}",
        if has(TOKEN_NAME)? {
            "stored"
        } else {
            "missing"
        }
    );
    println!(
        "client state: {}",
        if has(CLIENT_STATE_NAME)? {
            "stored"
        } else {
            "missing"
        }
    );
    if url.is_none() {
        println!("Next: chatcli relay setup <url>");
    }
    Ok(())
}

async fn connect() -> Result<Graph> {
    let http = auth::http_client()?;
    let token = auth::access_token(&http).await?;
    Ok(Graph::new(http, token))
}

async fn login() -> Result<()> {
    let http = auth::http_client()?;
    let token = auth::sign_in(&http, |url| {
        println!("Sign in to Teams in the browser window. If none opened, paste this link into a browser:");
        println!("{url}");
    })
    .await?;
    let me = Graph::new(http, token).me().await?;
    println!(
        "Signed in to Teams as {}.",
        me.display_name.as_deref().unwrap_or("(no display name)")
    );
    Ok(())
}

async fn chats() -> Result<()> {
    let graph = connect().await?;
    let me = graph.me().await?;
    let all = graph.chats().await?;
    let shown: Vec<_> = all
        .iter()
        .filter(|c| c.chat_type == "oneOnOne" || c.chat_type == "group")
        .collect();
    if shown.is_empty() {
        println!("No 1:1 or group chats in your newest {}.", all.len());
        return Ok(());
    }
    for chat in &shown {
        let preview = chat.last_message_preview.as_ref();
        let when = preview
            .and_then(|p| p.created_date_time.as_deref())
            .map(short_time)
            .unwrap_or_default();
        let last = preview
            .map(|p| {
                let text = if p.is_deleted == Some(true) {
                    "[deleted]".to_string()
                } else if p.message_type.as_deref().is_some_and(|t| t != "message") {
                    "[system event]".to_string()
                } else {
                    p.body.as_ref().map(|b| b.text()).unwrap_or_default()
                };
                let who = p.from.as_ref().map_or("(system)", |f| f.name());
                format!("{who}: {text}")
            })
            .unwrap_or_default();
        let kind = if chat.chat_type == "group" {
            "group"
        } else {
            "1:1"
        };
        let unread = if chat.is_unread() { "*" } else { " " };
        println!(
            "{unread} {when:<17} {kind:<5} {}  |  {}",
            clip(&chat.title(&me.id), 40),
            clip(&last, 70)
        );
        println!("    {}", chat.id);
    }
    println!(
        "{} chats shown (* = unread). {} meeting chats hidden. Newest {} only.",
        shown.len(),
        all.len() - shown.len(),
        MAX_PAGE
    );
    Ok(())
}

async fn messages(chat_id: &str, n: u32) -> Result<()> {
    let graph = connect().await?;
    let me = graph.me().await?;
    let chat = graph.chat(chat_id).await?;
    println!("== {} ==", chat.title(&me.id));
    for m in graph.messages(chat_id, n).await? {
        let when = m
            .created_date_time
            .as_deref()
            .map(short_time)
            .unwrap_or_default();
        println!("{when} {}: {}", m.sender(), m.text());
    }
    Ok(())
}

async fn send(chat_id: &str, text: &str) -> Result<()> {
    let graph = connect().await?;
    let me = graph.me().await?;
    // Look the chat up first: proves the id and shows a name to confirm against, so a
    // pasted id from the wrong line cannot send to the wrong person silently.
    let chat = graph.chat(chat_id).await?;
    println!("To:   {} ({})", chat.title(&me.id), chat.chat_type);
    println!("Text: {text}");
    print!("Type y and press Enter to send: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    if answer.trim() != "y" {
        println!("Not sent.");
        return Ok(());
    }
    let sent = graph.send_text(chat_id, text).await?;
    println!(
        "Sent at {} (message id {}).",
        sent.created_date_time
            .as_deref()
            .map(short_time)
            .unwrap_or_default(),
        sent.id
    );
    Ok(())
}

/// "2026-11-16T01:02:03.456Z" -> "2026-11-16 01:02Z"
fn short_time(iso: &str) -> String {
    match iso.get(..16) {
        Some(minute) => format!("{}Z", minute.replacen('T', " ", 1)),
        None => iso.to_string(),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, text: Option<&str>) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chatcli-cli-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    #[test]
    fn settings_check_exit_codes() {
        // Missing file and a clean file: 0.
        assert!(check_settings_file(&temp_file("missing", None)).is_ok());
        assert!(check_settings_file(&temp_file(
            "clean",
            Some(
                "[layout]
density = \"compact\"
"
            )
        ))
        .is_ok());
        // Warnings only (an unknown key): still 0.
        assert!(check_settings_file(&temp_file(
            "warn",
            Some(
                "[layout]
densty = 1
"
            )
        ))
        .is_ok());
        // A bad value, a syntax error, or both: 1.
        for (tag, text) in [
            (
                "value",
                "[layout]
density = \"tiny\"
",
            ),
            (
                "syntax",
                "[media]
cache_max_mb = = 1
",
            ),
            (
                "both",
                "[layout]
densty = 1
[teams]
files_read = 3
",
            ),
        ] {
            let err = check_settings_file(&temp_file(tag, Some(text))).unwrap_err();
            assert!(err.contains("error(s) in settings.toml"), "{tag}: {err}");
        }
    }

    #[test]
    fn only_sign_in_and_cache_commands_take_the_instance_lock() {
        for locked in [
            &["teams", "login"][..],
            &["teams", "logout"],
            &["teams", "chats"],
            &["teams", "messages", "id", "5"],
            &["teams", "send", "id", "hi"],
            &["teams", "setup", "a", "b"],
            &["relay", "setup", "https://x", "--rotate"],
            &["relay", "url", "https://x"],
            &["media", "clear"],
        ] {
            assert!(needs_instance_lock(locked), "{locked:?}");
        }
        for free in [
            &["relay", "status"][..],
            &["settings", "check"],
            &["settings", "path"],
            &["doctor", "--online"],
            &["search", "word"],
        ] {
            assert!(!needs_instance_lock(free), "{free:?}");
        }
    }

    #[test]
    fn tui_problems_join_the_core_ones() {
        let report = with_tui_problems(settings::parse(
            "[keys.list]\n\"ctrl+q\" = \"lst_down\"\n",
            Path::new("settings.toml"),
        ));
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.message.contains("list_down")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn search_lines_mark_the_match_and_say_when_nothing_matches() {
        use chatcli_core::store::SnippetPart;
        let part = |text: &str, hit| SnippetPart {
            text: text.to_string(),
            hit,
        };
        let hit = SearchHit {
            network: Network::Teams,
            chat_id: "c".into(),
            chat_title: "Alex Example".into(),
            message_id: "m".into(),
            ts: 1_700_000_000_000,
            sender: "Sam Sample".into(),
            snippet: vec![
                part("the ", false),
                part("draft", true),
                part(" is done", false),
            ],
        };
        let lines = hit_lines(&[hit]);
        assert_eq!(
            lines,
            vec!["2023-11-14 22:13Z Teams Alex Example: Sam Sample: the [draft] is done"]
        );
        assert_eq!(hit_lines(&[]), vec!["No messages match."]);
    }

    #[test]
    fn a_full_page_of_hits_says_it_was_cut() {
        let hit = SearchHit {
            network: Network::WhatsApp,
            chat_id: "c".into(),
            chat_title: "t".into(),
            message_id: "m".into(),
            ts: 0,
            sender: "s".into(),
            snippet: Vec::new(),
        };
        let lines = hit_lines(&vec![hit; SEARCH_LIMIT as usize]);
        assert_eq!(lines.len(), SEARCH_LIMIT as usize + 1);
        assert!(lines.last().unwrap().starts_with("Newest 50 shown"));
    }

    #[test]
    fn problem_lines_name_level_line_and_totals() {
        let report = settings::parse(
            "[layout]
density = \"tiny\"
[teams]
files_reed = true
",
            Path::new("settings.toml"),
        );
        let (lines, errors) = problem_lines(Path::new("settings.toml"), &report.problems);
        assert_eq!(errors, 1);
        assert!(
            lines[0].starts_with("error: settings.toml line 2: [layout] density:"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("warning: settings.toml line 4: unknown setting"),
            "{lines:?}"
        );
        assert_eq!(lines[2], "settings.toml: 1 error(s), 1 warning(s).");
        let (clean, none) = problem_lines(Path::new("s.toml"), &[]);
        assert_eq!((clean, none), (vec!["s.toml: no problems.".to_string()], 0));
    }
}
