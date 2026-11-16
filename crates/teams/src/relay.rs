//! Client for the user's own relay Worker: the WebSocket that carries Graph change
//! notifications (ids only, never message text) to the laptop, and the secrets that tie the
//! Worker, Graph and chatcli together. Protocol: docs/DESIGN.md, "Relay protocol".

use std::path::Path;
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

/// Said when setup is run again over working secrets. The Worker holds the same two values, so
/// new ones on this side alone would silently break the relay (the Teams tab then shows
/// `NOT LIVE`, HTTP 401).
pub const SECRETS_EXIST: &str =
    "Stored relay secrets exist. Use --rotate to make new ones, then redo both wrangler secret put steps.";

/// Where the relay secrets are kept: the Windows credential store in the app, a map in tests.
trait SecretIo {
    fn read(&self, name: &str) -> Result<Option<String>>;
    fn store(&self, name: &str, secret: &str) -> Result<()>;
    fn clear(&self, name: &str) -> Result<()>;
}

/// The credential store under one service name.
struct OsSecrets<'a>(&'a str);

impl SecretIo for OsSecrets<'_> {
    fn read(&self, name: &str) -> Result<Option<String>> {
        secrets::read(self.0, name)
    }
    fn store(&self, name: &str, secret: &str) -> Result<()> {
        secrets::store(self.0, name, secret)
    }
    fn clear(&self, name: &str) -> Result<()> {
        secrets::clear(self.0, name)
    }
}

/// Generates the two relay secrets, keeps them in Credential Manager and saves the URL to
/// `config.json`. The caller prints the two `wrangler secret put` steps with the result.
///
/// With working secrets already stored this changes nothing and fails with `SECRETS_EXIST`,
/// unless `rotate` is set. `relay_set_url` is the way to change only the URL.
pub fn relay_setup(url: &str, rotate: bool) -> Result<RelaySecrets> {
    setup_with(
        &OsSecrets(secrets::SERVICE),
        &chatcli_core::paths::config_file()?,
        url,
        rotate,
        &config::save_to,
    )
}

/// Changes only the relay URL; the stored secrets stay. Use it when the Worker moved.
pub fn relay_set_url(url: &str) -> Result<()> {
    set_url_at(&chatcli_core::paths::config_file()?, url)
}

fn set_url_at(cfg_path: &Path, url: &str) -> Result<()> {
    let url = config::normalize_relay_url(url)?;
    let mut cfg = config::load_from(cfg_path)?;
    cfg.relay_url = Some(url);
    config::save_to(cfg_path, &cfg)?;
    log::info("relay", "relay URL changed, secrets kept");
    Ok(())
}

/// Writes the secrets that were there before back (or removes them when there were none).
/// True when every step worked.
fn put_back(io: &dyn SecretIo, old: &(Option<String>, Option<String>)) -> bool {
    let mut all_ok = true;
    for (name, value) in [(TOKEN_NAME, &old.0), (CLIENT_STATE_NAME, &old.1)] {
        let done = match value {
            Some(v) => io.store(name, v),
            None => io.clear(name),
        };
        // Both are tried even if the first fails.
        all_ok &= done.is_ok();
    }
    all_ok
}

fn setup_with(
    io: &dyn SecretIo,
    cfg_path: &Path,
    url: &str,
    rotate: bool,
    save: &dyn Fn(&Path, &config::Config) -> Result<()>,
) -> Result<RelaySecrets> {
    let url = config::normalize_relay_url(url)?;
    // Everything that can fail without side effects comes first: a broken config.json must not
    // leave new secrets behind that no URL goes with.
    let mut cfg = config::load_from(cfg_path)?;
    let old = (io.read(TOKEN_NAME)?, io.read(CLIENT_STATE_NAME)?);
    if old.0.is_some() && old.1.is_some() && !rotate {
        return Err(SECRETS_EXIST.into());
    }
    let made = RelaySecrets {
        token: random_secret(),
        client_state: random_secret(),
    };
    if let Err(e) = io
        .store(TOKEN_NAME, &made.token)
        .and_then(|()| io.store(CLIENT_STATE_NAME, &made.client_state))
    {
        let back = put_back(io, &old);
        return Err(failed_setup(&e, back));
    }
    cfg.relay_url = Some(url);
    if let Err(e) = save(cfg_path, &cfg) {
        let back = put_back(io, &old);
        return Err(failed_setup(&e, back));
    }
    log::info("relay", "relay secrets generated and URL saved");
    Ok(made)
}

fn failed_setup(error: &str, secrets_put_back: bool) -> String {
    if secrets_put_back {
        format!(
            "Relay setup failed ({error}). Nothing changed: the earlier secrets are still stored."
        )
    } else {
        format!("Relay setup failed ({error}), and the earlier secrets could not be put back. Run: chatcli relay setup <url> --rotate, then redo both wrangler secret put steps.")
    }
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
                "relay token has characters a header cannot hold: quit chatcli, then run chatcli relay setup again"
                    .into(),
            )
        }
    }
    let socket = match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Err(_) => return retry("relay connection timed out", None),
        Ok(Err(WsError::Http(response))) if response.status().as_u16() == 401 => {
            return End::Stop(
                "relay rejected the token (HTTP 401): quit chatcli, run chatcli relay setup again and put both secrets on the Worker".into(),
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

    /// Secrets in a map, with a switch that makes the nth store fail.
    #[derive(Default)]
    struct Mem {
        map: std::cell::RefCell<std::collections::HashMap<String, String>>,
        stores: std::cell::Cell<usize>,
        fail_store_number: std::cell::Cell<Option<usize>>,
    }

    impl Mem {
        fn with(token: Option<&str>, state: Option<&str>) -> Mem {
            let m = Mem::default();
            if let Some(t) = token {
                m.map.borrow_mut().insert(TOKEN_NAME.into(), t.into());
            }
            if let Some(s) = state {
                m.map
                    .borrow_mut()
                    .insert(CLIENT_STATE_NAME.into(), s.into());
            }
            m
        }

        fn get(&self, name: &str) -> Option<String> {
            self.map.borrow().get(name).cloned()
        }
    }

    impl SecretIo for Mem {
        fn read(&self, name: &str) -> Result<Option<String>> {
            Ok(self.get(name))
        }
        fn store(&self, name: &str, secret: &str) -> Result<()> {
            let n = self.stores.get() + 1;
            self.stores.set(n);
            if self.fail_store_number.get() == Some(n) {
                return Err("credential store: disk full".into());
            }
            self.map.borrow_mut().insert(name.into(), secret.into());
            Ok(())
        }
        fn clear(&self, name: &str) -> Result<()> {
            self.map.borrow_mut().remove(name);
            Ok(())
        }
    }

    const URL: &str = "https://chatcli-relay.example.workers.dev";

    fn temp_config(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatcli-relay-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    fn url_in(path: &Path) -> Option<String> {
        config::load_from(path).unwrap().relay_url
    }

    fn setup(io: &Mem, cfg: &Path, rotate: bool) -> Result<RelaySecrets> {
        setup_with(io, cfg, URL, rotate, &config::save_to)
    }

    #[test]
    fn first_setup_makes_both_secrets_and_saves_the_url() {
        let (io, cfg) = (Mem::default(), temp_config("first"));
        let made = setup_with(
            &io,
            &cfg,
            " https://chatcli-relay.example.workers.dev/ ",
            false,
            &config::save_to,
        )
        .unwrap();
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some(made.token.as_str()));
        assert_eq!(
            io.get(CLIENT_STATE_NAME).as_deref(),
            Some(made.client_state.as_str())
        );
        assert_eq!(url_in(&cfg).as_deref(), Some(URL));
    }

    /// Issue 26: running setup again used to overwrite working secrets without a word.
    #[test]
    fn a_second_setup_without_rotate_changes_nothing() {
        let (io, cfg) = (Mem::default(), temp_config("again"));
        let first = setup(&io, &cfg, false).unwrap();
        let before = std::fs::read(&cfg).unwrap();
        let err = setup_with(
            &io,
            &cfg,
            "https://other.example.workers.dev",
            false,
            &config::save_to,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Stored relay secrets exist. Use --rotate to make new ones, then redo both wrangler secret put steps."
        );
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some(first.token.as_str()));
        assert_eq!(
            io.get(CLIENT_STATE_NAME).as_deref(),
            Some(first.client_state.as_str())
        );
        assert_eq!(
            std::fs::read(&cfg).unwrap(),
            before,
            "not even the URL changed"
        );
    }

    #[test]
    fn rotate_makes_new_secrets_and_keeps_the_two_step_reminder_possible() {
        let (io, cfg) = (Mem::default(), temp_config("rotate"));
        let first = setup(&io, &cfg, false).unwrap();
        let second = setup(&io, &cfg, true).unwrap();
        assert_ne!(first.token, second.token);
        assert_ne!(first.client_state, second.client_state);
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some(second.token.as_str()));
        assert_eq!(
            io.get(CLIENT_STATE_NAME).as_deref(),
            Some(second.client_state.as_str())
        );
    }

    #[test]
    fn half_stored_secrets_are_repaired_without_rotate() {
        let (io, cfg) = (Mem::with(Some("only-the-token"), None), temp_config("half"));
        let made = setup(&io, &cfg, false).unwrap();
        assert_eq!(
            io.get(CLIENT_STATE_NAME).as_deref(),
            Some(made.client_state.as_str())
        );
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some(made.token.as_str()));
    }

    /// CodeRabbit 5: a broken config.json used to be found only after the secrets were replaced.
    #[test]
    fn a_broken_config_leaves_the_working_secrets_alone() {
        let (io, cfg) = (
            Mem::with(Some("old-token"), Some("old-state")),
            temp_config("broken"),
        );
        std::fs::write(&cfg, "{ not json").unwrap();
        let err = setup(&io, &cfg, true).unwrap_err();
        assert!(err.contains("not valid settings JSON"), "{err}");
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some("old-token"));
        assert_eq!(io.get(CLIENT_STATE_NAME).as_deref(), Some("old-state"));
        assert_eq!(io.stores.get(), 0, "no secret was written");
    }

    #[test]
    fn a_failed_second_write_puts_the_old_secrets_back() {
        let (io, cfg) = (
            Mem::with(Some("old-token"), Some("old-state")),
            temp_config("midway"),
        );
        io.fail_store_number.set(Some(2));
        let err = setup(&io, &cfg, true).unwrap_err();
        assert!(err.contains("Nothing changed"), "{err}");
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some("old-token"));
        assert_eq!(io.get(CLIENT_STATE_NAME).as_deref(), Some("old-state"));
        assert_eq!(url_in(&cfg), None, "no URL was saved either");
    }

    #[test]
    fn a_failed_first_ever_setup_leaves_no_secrets_behind() {
        let (io, cfg) = (Mem::default(), temp_config("fresh-fail"));
        io.fail_store_number.set(Some(2));
        assert!(setup(&io, &cfg, false).is_err());
        assert_eq!(
            (io.get(TOKEN_NAME), io.get(CLIENT_STATE_NAME)),
            (None, None)
        );
    }

    #[test]
    fn a_failed_config_save_puts_the_old_secrets_back() {
        let (io, cfg) = (
            Mem::with(Some("old-token"), Some("old-state")),
            temp_config("save"),
        );
        let failing = |_: &Path, _: &config::Config| -> Result<()> { Err("disk full".into()) };
        let err = setup_with(&io, &cfg, URL, true, &failing).unwrap_err();
        assert!(
            err.contains("disk full") && err.contains("Nothing changed"),
            "{err}"
        );
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some("old-token"));
        assert_eq!(io.get(CLIENT_STATE_NAME).as_deref(), Some("old-state"));
    }

    #[test]
    fn a_bad_url_is_refused_before_any_secret_is_made() {
        let (io, cfg) = (Mem::default(), temp_config("badurl"));
        assert!(setup_with(&io, &cfg, "http://bad.example", false, &config::save_to).is_err());
        assert_eq!(io.stores.get(), 0);
        assert!(!cfg.exists());
    }

    #[test]
    fn relay_url_changes_only_the_url() {
        let (io, cfg) = (Mem::default(), temp_config("url"));
        let made = setup(&io, &cfg, false).unwrap();
        set_url_at(&cfg, " https://moved.example.workers.dev/ ").unwrap();
        assert_eq!(
            url_in(&cfg).as_deref(),
            Some("https://moved.example.workers.dev")
        );
        assert_eq!(io.get(TOKEN_NAME).as_deref(), Some(made.token.as_str()));
        assert_eq!(
            io.get(CLIENT_STATE_NAME).as_deref(),
            Some(made.client_state.as_str())
        );
        // Other settings in config.json survive.
        let mut c = config::load_from(&cfg).unwrap();
        c.teams_client_id = Some("00000000-0000-0000-0000-000000000000".into());
        config::save_to(&cfg, &c).unwrap();
        set_url_at(&cfg, URL).unwrap();
        assert!(config::load_from(&cfg).unwrap().teams_client_id.is_some());
    }

    #[test]
    fn relay_url_refuses_a_bad_url_and_a_broken_config() {
        let cfg = temp_config("url-bad");
        set_url_at(&cfg, URL).unwrap();
        assert!(set_url_at(&cfg, "http://bad.example").is_err());
        assert_eq!(url_in(&cfg).as_deref(), Some(URL));
        std::fs::write(&cfg, "{ not json").unwrap();
        assert!(set_url_at(&cfg, URL).is_err());
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "{ not json",
            "the file is not replaced"
        );
    }

    /// The real Windows credential store under its own test service name, and a temp config.
    /// Run with: cargo test -p chatcli-teams relay_setup_in_the_real -- --ignored
    #[test]
    #[ignore = "writes to the Windows credential store"]
    fn relay_setup_in_the_real_credential_store() {
        const TEST_SERVICE: &str = "chatcli (test relay setup)";
        let cfg = temp_config("os");
        let os = OsSecrets(TEST_SERVICE);
        let made = setup_with(&os, &cfg, URL, true, &config::save_to).unwrap();
        assert_eq!(
            os.read(TOKEN_NAME).unwrap().as_deref(),
            Some(made.token.as_str())
        );
        assert_eq!(
            setup_with(&os, &cfg, URL, false, &config::save_to).unwrap_err(),
            SECRETS_EXIST
        );
        os.clear(TOKEN_NAME).unwrap();
        os.clear(CLIENT_STATE_NAME).unwrap();
    }
}
