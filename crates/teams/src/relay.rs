//! Client for the user's own relay Worker: the WebSocket that carries Graph change
//! notifications (ids only, never message text) to the laptop, and the secrets that tie the
//! Worker, Graph and chatcli together. Protocol: docs/DESIGN.md, "Relay protocol".

use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use chatcli_core::{chain, config, log, secrets, Result};

pub const TOKEN_NAME: &str = "relay-token";
pub const CLIENT_STATE_NAME: &str = "relay-client-state";

const PING_EVERY: Duration = Duration::from_secs(30);
/// No frame at all (pong included) for this long means the socket is half-dead.
const DEAD_AFTER: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MAX_SECS: u64 = 60;
/// A session shorter than this does not reset the backoff, so a Worker that accepts and
/// immediately drops cannot be hammered once a second.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// The Worker closes the old socket with this code when a new one connects.
const CLOSE_REPLACED: u16 = 4000;

#[derive(Debug, Clone, PartialEq)]
pub struct RelaySecrets {
    pub token: String,
    pub client_state: String,
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generates the two relay secrets, keeps them in Credential Manager and saves the URL to
/// `config.json`. The caller prints the two `wrangler secret put` steps with the result.
pub fn relay_setup(url: &str) -> Result<RelaySecrets> {
    setup_in(secrets::SERVICE, url)
}

fn setup_in(service: &str, url: &str) -> Result<RelaySecrets> {
    let url = config::normalize_relay_url(url)?;
    let made = RelaySecrets {
        token: random_secret(),
        client_state: random_secret(),
    };
    secrets::store(service, TOKEN_NAME, &made.token)?;
    secrets::store(service, CLIENT_STATE_NAME, &made.client_state)?;
    let mut cfg = config::load()?;
    cfg.relay_url = Some(url);
    config::save(&cfg)?;
    log::info("relay", "relay secrets generated and URL saved");
    Ok(made)
}

/// The stored secrets, or None when `chatcli relay setup` has not been run.
pub fn load_secrets() -> Result<Option<RelaySecrets>> {
    let token = secrets::read(secrets::SERVICE, TOKEN_NAME)?;
    let client_state = secrets::read(secrets::SERVICE, CLIENT_STATE_NAME)?;
    Ok(match (token, client_state) {
        (Some(token), Some(client_state)) => Some(RelaySecrets {
            token,
            client_state,
        }),
        _ => None,
    })
}

/// `https://host` -> `wss://host/ws`.
pub fn ws_url(relay_url: &str) -> String {
    format!(
        "wss://{}/ws",
        relay_url.strip_prefix("https://").unwrap_or(relay_url)
    )
}

// --- frames --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    /// `dropped`: notifications that arrived while no socket was connected.
    Hello {
        #[serde(default)]
        dropped: u64,
    },
    Notify {
        items: Vec<Notification>,
    },
    Lifecycle {
        items: Vec<LifecycleItem>,
    },
    Alert {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    #[serde(default)]
    pub subscription_id: String,
    #[serde(default)]
    pub change_type: String,
    pub resource: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleItem {
    #[serde(default)]
    pub subscription_id: String,
    pub lifecycle_event: String,
}

pub fn parse_frame(text: &str) -> std::result::Result<Frame, String> {
    serde_json::from_str(text).map_err(|e| format!("unreadable relay frame: {e}"))
}

/// The chat and message a notification points at.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRef {
    pub chat_id: String,
    pub message_id: String,
}

/// Parses `chats('<chatId>')/messages('<messageId>')`. Ids may contain `:`, `@` and `.`, and
/// may arrive percent-encoded; a leading `/` or `users('..')/` is tolerated. Anything else is
/// None, never a guess.
pub fn parse_resource(resource: &str) -> Option<ResourceRef> {
    let from_chats = &resource[resource.find("chats('")? + "chats('".len()..];
    let (chat, rest) = from_chats.split_once("')/messages('")?;
    let message = rest.strip_suffix("')")?;
    let (chat_id, message_id) = (percent_decode(chat)?, percent_decode(message)?);
    let sane = |s: &str| !s.is_empty() && !s.contains(['\'', '/', '\\']);
    (sane(&chat_id) && sane(&message_id)).then_some(ResourceRef {
        chat_id,
        message_id,
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// --- socket client -------------------------------------------------------------------------

/// What the socket task tells the backend.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    Up,
    /// Socket gone, with the reason shown in the NOT LIVE chip. The task keeps retrying
    /// unless it has stopped for good (check `Client::is_finished`).
    Down(String),
    Frame(Frame),
}

/// `1 s, 2 s, 4 s ... 60 s`.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(
        1u64.checked_shl(attempt)
            .unwrap_or(u64::MAX)
            .min(BACKOFF_MAX_SECS),
    )
}

/// The attempt counter after a session: reset only when the socket stayed up.
fn next_attempt(attempt: u32, was_up_for: Option<Duration>) -> u32 {
    match was_up_for {
        Some(d) if d >= STABLE_AFTER => 0,
        _ => attempt.saturating_add(1),
    }
}

/// The running socket task. Dropping it stops the task.
pub struct Client {
    task: tokio::task::JoinHandle<()>,
}

impl Client {
    pub fn start(relay_url: &str, token: String, tx: UnboundedSender<RelayEvent>) -> Client {
        Client {
            task: tokio::spawn(run_client(ws_url(relay_url), token, tx)),
        }
    }

    /// True when the task gave up for good (rejected token, replaced by another instance).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum End {
    /// Reconnect after the backoff. `up_for` is Some when the socket had connected.
    Retry {
        reason: String,
        up_for: Option<Duration>,
    },
    /// Do not reconnect.
    Stop(String),
}

async fn run_client(url: String, token: String, tx: UnboundedSender<RelayEvent>) {
    let mut attempt = 0u32;
    loop {
        let end = session(&url, &token, &tx).await;
        let (reason, up_for, stop) = match end {
            End::Retry { reason, up_for } => (reason, up_for, false),
            End::Stop(reason) => (reason, None, true),
        };
        log::error("relay", &format!("relay socket down: {reason}"));
        if tx.send(RelayEvent::Down(reason)).is_err() || stop {
            return;
        }
        attempt = next_attempt(attempt, up_for);
        tokio::time::sleep(backoff(attempt.saturating_sub(1))).await;
    }
}

fn retry(reason: impl Into<String>, up_for: Option<Duration>) -> End {
    End::Retry {
        reason: reason.into(),
        up_for,
    }
}

async fn session(url: &str, token: &str, tx: &UnboundedSender<RelayEvent>) -> End {
    let mut request = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return End::Stop(format!("bad relay URL: {e}")),
    };
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(v) => {
            request.headers_mut().insert("Authorization", v);
        }
        Err(_) => {
            return End::Stop(
                "relay token has characters a header cannot hold: run chatcli relay setup again"
                    .into(),
            )
        }
    }
    let socket = match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Err(_) => return retry("relay connection timed out", None),
        Ok(Err(WsError::Http(response))) if response.status().as_u16() == 401 => {
            return End::Stop(
                "relay rejected the token (HTTP 401): run chatcli relay setup again and put both secrets on the Worker".into(),
            )
        }
        Ok(Err(WsError::Http(response))) => {
            return retry(format!("relay answered HTTP {}", response.status().as_u16()), None)
        }
        Ok(Err(e)) => return retry(chain(&e), None),
        Ok(Ok((socket, _))) => socket,
    };
    let began = Instant::now();
    let (mut sink, mut stream) = socket.split();
    if tx.send(RelayEvent::Up).is_err() {
        return End::Stop(String::new());
    }
    let mut ticker = tokio::time::interval(PING_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_alive = Instant::now();
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                None => return retry("relay closed the connection", Some(began.elapsed())),
                Some(Err(e)) => return retry(chain(&e), Some(began.elapsed())),
                Some(Ok(Message::Text(text))) => {
                    last_alive = Instant::now();
                    if text.as_str() == "pong" {
                        continue;
                    }
                    match parse_frame(text.as_str()) {
                        Ok(frame) => {
                            if tx.send(RelayEvent::Frame(frame)).is_err() {
                                return End::Stop(String::new());
                            }
                        }
                        Err(e) => log::error("relay", &e),
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.as_ref().map(|f| u16::from(f.code));
                    if code == Some(CLOSE_REPLACED) {
                        return End::Stop(
                            "another chatcli took the relay connection: close it, then refresh".into(),
                        );
                    }
                    let why = frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty());
                    return retry(
                        why.unwrap_or_else(|| "relay closed the connection".into()),
                        Some(began.elapsed()),
                    );
                }
                // Binary, protocol ping/pong: any traffic proves the socket is alive.
                Some(Ok(_)) => last_alive = Instant::now(),
            },
            _ = ticker.tick() => {
                if last_alive.elapsed() > DEAD_AFTER {
                    return retry("no answer from the relay for 90 s", Some(began.elapsed()));
                }
                if sink.send(Message::text("ping")).await.is_err() {
                    return retry("could not send ping to the relay", Some(began.elapsed()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_with_colons_at_signs_and_dots() {
        let r = parse_resource("chats('19:abc123@thread.v2')/messages('1612289765949')").unwrap();
        assert_eq!(r.chat_id, "19:abc123@thread.v2");
        assert_eq!(r.message_id, "1612289765949");
        let one_on_one =
            parse_resource("chats('19:aaaa-bbbb_cccc@unq.gbl.spaces')/messages('1')").unwrap();
        assert_eq!(one_on_one.chat_id, "19:aaaa-bbbb_cccc@unq.gbl.spaces");
    }

    #[test]
    fn resource_percent_encoded_and_prefixed() {
        let r = parse_resource("/chats('19%3Aabc%40thread.v2')/messages('99')").unwrap();
        assert_eq!(r.chat_id, "19:abc@thread.v2");
        let r = parse_resource("users('u1')/chats('19:x@thread.v2')/messages('5')").unwrap();
        assert_eq!(
            (r.chat_id.as_str(), r.message_id.as_str()),
            ("19:x@thread.v2", "5")
        );
    }

    #[test]
    fn resource_rejects_everything_else() {
        for bad in [
            "",
            "chats('19:x')",
            "chats('19:x')/messages('')",
            "chats('')/messages('1')",
            "chats('19:x')/messages('1')/replies('2')",
            "chats('19:x')/messages('1'",
            "teams('t')/channels('c')/messages('1')",
            "chats('19:x%2Fy')/messages('1')",
            "chats('19:x%ZZ')/messages('1')",
            "chats('19:x%')/messages('1')",
            "chats('%FF')/messages('1')",
        ] {
            assert_eq!(parse_resource(bad), None, "{bad}");
        }
    }

    #[test]
    fn frames_parse() {
        assert_eq!(
            parse_frame(r#"{"type":"hello","dropped":3}"#).unwrap(),
            Frame::Hello { dropped: 3 }
        );
        assert_eq!(
            parse_frame(r#"{"type":"hello"}"#).unwrap(),
            Frame::Hello { dropped: 0 }
        );
        let notify = parse_frame(
            r#"{"type":"notify","items":[{"subscriptionId":"s1","changeType":"created","resource":"chats('19:x')/messages('1')"}]}"#,
        )
        .unwrap();
        let Frame::Notify { items } = notify else {
            panic!("not notify")
        };
        assert_eq!(items[0].subscription_id, "s1");
        assert_eq!(items[0].change_type, "created");
        let life = parse_frame(
            r#"{"type":"lifecycle","items":[{"subscriptionId":"s1","lifecycleEvent":"reauthorizationRequired"}]}"#,
        )
        .unwrap();
        let Frame::Lifecycle { items } = life else {
            panic!("not lifecycle")
        };
        assert_eq!(items[0].lifecycle_event, "reauthorizationRequired");
        assert_eq!(
            parse_frame(r#"{"type":"alert","text":"bad clientState"}"#).unwrap(),
            Frame::Alert {
                text: "bad clientState".into()
            }
        );
    }

    #[test]
    fn bad_frames_are_errors_not_panics() {
        assert!(parse_frame("pong").is_err());
        assert!(parse_frame(r#"{"type":"mystery"}"#).is_err());
        assert!(parse_frame(r#"{"type":"notify"}"#).is_err());
        assert!(parse_frame(r#"{"type":"notify","items":[{"subscriptionId":"s"}]}"#).is_err());
    }

    #[test]
    fn backoff_doubles_to_sixty_seconds() {
        let secs: Vec<u64> = (0..9).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(backoff(200).as_secs(), 60, "shift overflow must not panic");
    }

    #[test]
    fn backoff_resets_only_after_a_stable_session() {
        assert_eq!(next_attempt(5, Some(Duration::from_secs(120))), 0);
        assert_eq!(
            next_attempt(5, Some(Duration::from_secs(2))),
            6,
            "up but flapping"
        );
        assert_eq!(next_attempt(5, None), 6, "never connected");
        assert_eq!(next_attempt(u32::MAX, None), u32::MAX);
    }

    #[test]
    fn ws_url_from_relay_url() {
        assert_eq!(
            ws_url("https://chatcli-relay.example.workers.dev"),
            "wss://chatcli-relay.example.workers.dev/ws"
        );
    }

    #[test]
    fn secrets_are_32_random_bytes_in_base64url() {
        let (a, b) = (random_secret(), random_secret());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    /// Real Windows credential store (own service name) and a temp data folder.
    /// Run with: cargo test -p chatcli-teams relay_setup -- --ignored
    #[test]
    #[ignore = "writes to the Windows credential store"]
    fn relay_setup_stores_secrets_and_url() {
        const TEST_SERVICE: &str = "chatcli (test relay setup)";
        let dir = std::env::temp_dir().join(format!("chatcli-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CHATCLI_DATA_DIR", &dir);
        assert!(
            setup_in(TEST_SERVICE, "http://bad.example").is_err(),
            "http is refused before any secret is made"
        );
        assert_eq!(secrets::read(TEST_SERVICE, TOKEN_NAME).unwrap(), None);
        let made = setup_in(TEST_SERVICE, " https://chatcli-relay.example.workers.dev/ ").unwrap();
        assert_eq!(
            secrets::read(TEST_SERVICE, TOKEN_NAME).unwrap().as_deref(),
            Some(made.token.as_str())
        );
        assert_eq!(
            secrets::read(TEST_SERVICE, CLIENT_STATE_NAME)
                .unwrap()
                .as_deref(),
            Some(made.client_state.as_str())
        );
        assert_eq!(
            config::load().unwrap().relay_url.as_deref(),
            Some("https://chatcli-relay.example.workers.dev")
        );
        secrets::clear(TEST_SERVICE, TOKEN_NAME).unwrap();
        secrets::clear(TEST_SERVICE, CLIENT_STATE_NAME).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
