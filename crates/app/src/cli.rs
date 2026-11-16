//! Plain command-line subcommands (`chatcli teams ...`). Kept beside the TUI for scripting
//! and for debugging one network without the full screen.

use std::io::Write;

use chatcli_core::{config, Result};
use chatcli_teams::{auth, graph::Graph, graph::MAX_PAGE};

pub const USAGE: [&str; 13] = [
    "usage:",
    "  chatcli                                  open the full-screen chat client",
    "  chatcli --demo                           try the screen with fake data (no store, no network)",
    "  chatcli --version                        print the version",
    "  chatcli relay setup <url>                make the relay secrets for your Worker URL",
    "  chatcli relay status                     show the relay URL and whether its secrets exist",
    "  chatcli teams setup <client-id> <tenant> save your Entra app registration ids",
    "  chatcli teams login                      sign in through the browser",
    "  chatcli teams chats                      1:1 and group chats, newest first",
    "  chatcli teams messages <chat-id> [n]     last n messages (default 20, max 50)",
    "  chatcli teams send <chat-id> <text...>   send a message (asks to confirm)",
    "  chatcli teams logout                     forget the stored sign-in",
    "Times are UTC. Log: %LOCALAPPDATA%\\chatcli\\chatcli.log",
];

/// Runs a subcommand. None = the arguments are not a known subcommand.
pub async fn run(args: &[&str]) -> Option<Result<()>> {
    Some(match args {
        ["relay", "setup", url] => relay_setup(url),
        ["relay", "status"] => relay_status(),
        ["teams", "setup", client_id, tenant] => teams_setup(client_id, tenant),
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

fn relay_setup(url: &str) -> Result<()> {
    let made = chatcli_teams::relay_setup(url)?;
    println!("Relay URL saved and two secrets made (kept in Windows Credential Manager).");
    println!("Next, from the chatCLI folder:");
    println!("  1. cd relay");
    println!("  2. bunx wrangler secret put RELAY_TOKEN");
    println!("     paste this value when asked: {}", made.token);
    println!("  3. bunx wrangler secret put CLIENT_STATE");
    println!("     paste this value when asked: {}", made.client_state);
    println!("  4. restart chatcli");
    println!("These values are shown once. Do not share them or paste them anywhere else.");
    Ok(())
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
