//! Microsoft sign-in for a desktop (public) client: authorization code with PKCE through the
//! system browser, a one-shot loopback listener for the redirect, and the refresh token kept
//! in Windows Credential Manager. Access tokens live in memory only.
//!
//! The app registration is the user's own: its client and tenant ids come from `config.json`
//! (`chatcli teams setup <client-id> <tenant-id>`), never from the binary.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use chatcli_core::settings::TeamsSettings;
use chatcli_core::{chain, config, log, secrets, settings, Result};

/// Delegated Graph scopes every sign-in asks for. Chat.ReadWrite covers reading, sending,
/// reactions, edits, deletes and mark-as-read in 1:1 and group chats. ChatMessage.Send is the
/// only permission `replyWithQuote` accepts. offline_access returns the refresh token.
const BASE_SCOPES: &str = "offline_access User.Read Chat.ReadWrite ChatMessage.Send";
/// What chatcli 0.1 asked for. A refresh falls back to it for a user who has not signed in
/// again since ChatMessage.Send was added: replies then say so, everything else keeps working.
const LEGACY_SCOPES: &str = "offline_access User.Read Chat.ReadWrite";
/// Optional: download files linked in messages (`[teams] files_read`).
const FILES_READ_SCOPE: &str = "Files.Read.All";
/// Optional: upload files to the user's OneDrive to send them (`[teams] files_write`).
const FILES_WRITE_SCOPE: &str = "Files.ReadWrite";
/// Optional: profile photos beside messages (`[teams] photos`). Needs no admin consent.
pub const PHOTOS_SCOPE: &str = "User.ReadBasic.All";
const REFRESH_TOKEN_NAME: &str = "teams-refresh-token";
/// How long the browser sign-in may take before the listener gives up.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// End of the HTTP request headers: CR LF CR LF. Bytes, not an escaped string literal, so no
/// editor or tool can turn it into plain newlines (a 4-byte window compared with a 2-byte
/// literal never matches, and every callback would wait out the read timeout).
const HEADER_END: [u8; 4] = [13, 10, 13, 10];
const CRLF: [u8; 2] = [13, 10];

pub const NOT_SET_UP: &str =
    "Teams is not set up. Register an app (see README, Teams setup), then run: chatcli teams setup <client-id> <tenant-id>";

/// The user's Entra app registration.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub client_id: String,
    pub tenant_id: String,
}

/// Reads and checks the app ids from config. Values go into URLs, so they are validated.
pub fn app() -> Result<App> {
    let cfg = config::load()?;
    match (cfg.teams_client_id, cfg.teams_tenant_id) {
        (Some(client_id), Some(tenant_id)) => {
            if !config::is_guid(&client_id) {
                return Err(format!(
                    "Teams client id '{client_id}' in config.json is not a GUID. {NOT_SET_UP}"
                ));
            }
            if !config::is_tenant(&tenant_id) {
                return Err(format!("Teams tenant '{tenant_id}' in config.json is not a GUID or domain. {NOT_SET_UP}"));
            }
            Ok(App {
                client_id,
                tenant_id,
            })
        }
        _ => Err(NOT_SET_UP.into()),
    }
}

fn authorize_url(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize")
}

fn token_url(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    /// Seconds. A number from the v2 endpoint; tolerate a numeric string too.
    expires_in: Option<serde_json::Value>,
    /// The scopes Microsoft actually granted (logged: shows a missing consent at a glance).
    scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
    suberror: Option<String>,
}

/// An access token and how long Microsoft says it lives.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub access: String,
    pub expires_in_secs: u64,
    /// The delegated scopes this token really carries (from the token's own `scp` claim, else
    /// Microsoft's `scope` field). Empty = unknown, which counts as nothing granted.
    pub granted: Vec<String>,
}

/// Which optional scopes the user's settings ask for, on top of the base ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet {
    pub files_read: bool,
    pub files_write: bool,
    pub photos: bool,
}

impl ScopeSet {
    pub fn from_settings(s: &TeamsSettings) -> ScopeSet {
        ScopeSet {
            files_read: s.files_read,
            files_write: s.files_write,
            photos: s.photos,
        }
    }

    fn optional(&self) -> bool {
        self.files_read || self.files_write || self.photos
    }

    /// The scope string of a sign-in or a refresh that wants everything the settings ask for.
    pub fn scope_string(&self) -> String {
        let mut s = BASE_SCOPES.to_string();
        if self.files_read {
            s.push(' ');
            s.push_str(FILES_READ_SCOPE);
        }
        if self.files_write {
            s.push(' ');
            s.push_str(FILES_WRITE_SCOPE);
        }
        if self.photos {
            s.push(' ');
            s.push_str(PHOTOS_SCOPE);
        }
        s
    }

    /// Scope strings for a refresh, most wanted first: everything, then without the optional
    /// ones, then what 0.1 asked for. A refresh for scopes the user never consented to fails
    /// with a rejection, and the next, smaller set still gets a working token.
    pub fn refresh_attempts(&self) -> Vec<String> {
        let mut all = vec![self.scope_string()];
        if self.optional() {
            all.push(BASE_SCOPES.to_string());
        }
        all.push(LEGACY_SCOPES.to_string());
        all
    }
}

/// The scopes a token carries: its `scp` claim, else Microsoft's `scope` field.
fn granted_scopes(access_token: &str, reported: Option<&str>) -> Vec<String> {
    crate::map::scopes_from_jwt(access_token).unwrap_or_else(|| {
        reported
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    })
}

/// A sign-in that failed while asking for file scopes may need an admin: say how to back out.
fn add_scope_hint(error: String, scopes: &ScopeSet) -> String {
    let needs_admin = error.contains("AADSTS65001")
        || error.contains("AADSTS90094")
        || error.to_ascii_lowercase().contains("admin");
    if scopes.optional() && needs_admin {
        format!("{error} If an admin must approve file access, set files_read, files_write and photos to false in settings.toml and sign in again.")
    } else {
        error
    }
}

fn settings_scopes() -> ScopeSet {
    ScopeSet::from_settings(&settings::load().settings.teams)
}

/// Why a token request failed. `Rejected` means Microsoft refused the stored sign-in, so only
/// a new interactive sign-in helps. `Other` is anything else (network, 5xx, bad response):
/// the stored sign-in may be fine and a retry can work.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthFailure {
    Rejected(String),
    Other(String),
}

impl From<AuthFailure> for String {
    fn from(f: AuthFailure) -> String {
        match f {
            AuthFailure::Rejected(m) | AuthFailure::Other(m) => m,
        }
    }
}

/// Used when Microsoft sends no `expires_in`: short, so a missing field never hides expiry.
const DEFAULT_EXPIRES_SECS: u64 = 600;

fn expires_secs(value: Option<&serde_json::Value>) -> u64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|secs| *secs > 0)
    .unwrap_or(DEFAULT_EXPIRES_SECS)
}

/// Errors that mean "this sign-in cannot be refreshed silently": a dead token, or one that
/// needs MFA / consent / a conditional-access prompt (Entra reports those as invalid_grant
/// or interaction_required too).
fn is_rejection(status: u16, error: &str) -> bool {
    (status == 400 || status == 401)
        && matches!(
            error,
            "invalid_grant" | "interaction_required" | "consent_required" | "login_required"
        )
}

/// One HTTP client for Entra and Graph. rustls with the operating system's root store, so a
/// private root installed in Windows (for a network that re-signs HTTPS) is trusted too.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("chatcli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("HTTP client: {}", chain(&e)))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_token() -> String {
    let mut bytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

pub fn sign_out() -> Result<()> {
    log::info("teams-auth", "signed out: stored refresh token cleared");
    secrets::clear(secrets::SERVICE, REFRESH_TOKEN_NAME)
}

/// True when a refresh token is stored (says nothing about whether Microsoft still accepts it).
pub fn has_stored_sign_in() -> Result<bool> {
    Ok(secrets::read(secrets::SERVICE, REFRESH_TOKEN_NAME)?.is_some())
}

/// A fresh access token from the stored refresh token. Entra rotates the refresh token on
/// each use; the new one replaces the old in the credential store.
pub async fn access_token(http: &reqwest::Client) -> Result<String> {
    refresh(http).await.map(|t| t.access).map_err(String::from)
}

/// Same as `access_token`, with the expiry and the reason class of a failure. Asks for the
/// scopes `settings.toml` wants (read from the file).
pub async fn refresh(http: &reqwest::Client) -> std::result::Result<Token, AuthFailure> {
    refresh_with(http, &settings_scopes()).await
}

/// A fresh token for the stored sign-in. When Microsoft refuses the scopes asked for (a new
/// scope the user never consented to), it tries the smaller sets in `refresh_attempts` and
/// returns the first that works: a new optional scope never signs the user out. If every set is
/// refused, the sign-in itself is dead and the last answer is the reason. `Token::granted` says
/// what the token can do.
pub async fn refresh_with(
    http: &reqwest::Client,
    scopes: &ScopeSet,
) -> std::result::Result<Token, AuthFailure> {
    let app = app().map_err(AuthFailure::Other)?;
    let stored = secrets::read(secrets::SERVICE, REFRESH_TOKEN_NAME).map_err(AuthFailure::Other)?;
    let Some(refresh) = stored else {
        return Err(AuthFailure::Rejected(
            "Not signed in to Teams. Sign in first.".into(),
        ));
    };
    let mut last = None;
    for (i, scope) in scopes.refresh_attempts().iter().enumerate() {
        let form = [
            ("client_id", app.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("scope", scope.as_str()),
        ];
        match exchange(http, &app, &form).await {
            Ok(token) => {
                if i > 0 {
                    log::info(
                        "teams-auth",
                        "refreshed with fewer scopes: sign in again to get the new ones",
                    );
                }
                return Ok(token);
            }
            Err(AuthFailure::Rejected(reason)) => last = Some(AuthFailure::Rejected(reason)),
            Err(other) => return Err(other),
        }
    }
    Err(last.unwrap_or_else(|| AuthFailure::Other("Microsoft sign-in: no scope to ask for".into())))
}

async fn exchange(
    http: &reqwest::Client,
    app: &App,
    form: &[(&str, &str)],
) -> std::result::Result<Token, AuthFailure> {
    let refreshing = form.contains(&("grant_type", "refresh_token"));
    let step = if refreshing {
        "token refresh"
    } else {
        "code exchange"
    };
    let other = |message: String| {
        log::error("teams-auth", &message);
        AuthFailure::Other(message)
    };
    let response = http
        .post(token_url(&app.tenant_id))
        .form(form)
        .send()
        .await
        .map_err(|e| other(format!("Microsoft sign-in ({step}): {}", chain(&e))))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| other(format!("Microsoft sign-in ({step}): {}", chain(&e))))?;
    if !status.is_success() {
        // Entra's error_description carries an AADSTS code and trace ids, never a token.
        let parsed = serde_json::from_str::<TokenError>(&body).ok();
        let message = match &parsed {
            Some(e) => {
                let sub = e
                    .suberror
                    .as_deref()
                    .map(|s| format!("/{s}"))
                    .unwrap_or_default();
                format!(
                    "{} ({}{sub})",
                    e.error_description.as_deref().unwrap_or_default().trim(),
                    e.error
                )
            }
            None => format!("HTTP {status}"),
        };
        log::error(
            "teams-auth",
            &format!("{step} failed: HTTP {status}: {message}"),
        );
        // The stored refresh token is NOT deleted on a rejection. invalid_grant also covers
        // "needs MFA", "consent missing" and other cases where the token itself is fine, and
        // deleting it turned those into a permanent sign-out with the reason lost. A new
        // sign-in overwrites it; a dead one just keeps showing Microsoft's reason.
        if refreshing
            && parsed
                .as_ref()
                .is_some_and(|e| is_rejection(status.as_u16(), &e.error))
        {
            return Err(AuthFailure::Rejected(format!(
                "Teams sign-in was rejected. Sign in again. Microsoft said: {message}"
            )));
        }
        return Err(AuthFailure::Other(format!(
            "Microsoft sign-in failed ({step}): {message}"
        )));
    }
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|e| {
        other(format!(
            "Microsoft sign-in ({step}): unexpected token response: {e}"
        ))
    })?;
    let expires_in_secs = expires_secs(parsed.expires_in.as_ref());
    log::info(
        "teams-auth",
        &format!(
            "{step}: access token lives {expires_in_secs} s, granted scopes: {}",
            parsed.scope.as_deref().unwrap_or("(not reported)")
        ),
    );
    match parsed.refresh_token {
        Some(refresh) => {
            // Failing to keep the new token is loud: the old one may already be rotated out.
            secrets::store(secrets::SERVICE, REFRESH_TOKEN_NAME, &refresh)
                .map_err(|e| other(format!("Could not save the Teams sign-in: {e}")))?;
            log::info(
                "teams-auth",
                &format!("{step} ok, refresh token stored ({} chars)", refresh.len()),
            );
        }
        // A refresh may legitimately keep the old token. A first sign-in without one would
        // "work" once and then fail on the next start: refuse it loudly instead.
        None if !refreshing => {
            return Err(other(
                "Microsoft returned no refresh token, so the sign-in cannot be kept. Check the app registration grants offline_access."
                    .into(),
            ));
        }
        None => log::info(
            "teams-auth",
            "token refresh ok, no new refresh token issued",
        ),
    }
    let granted = granted_scopes(&parsed.access_token, parsed.scope.as_deref());
    Ok(Token {
        access: parsed.access_token,
        expires_in_secs,
        granted,
    })
}

/// Runs the whole browser flow and returns an access token. `show_link` receives the sign-in
/// URL (the CLI prints it, the TUI shows it) in case the browser does not open. Waits until
/// the browser comes back or SIGN_IN_TIMEOUT passes. The browser tab shows the real outcome:
/// it is answered only after the code has been exchanged for tokens.
pub async fn sign_in(http: &reqwest::Client, show_link: impl FnOnce(&str)) -> Result<String> {
    sign_in_token(http, show_link).await.map(|t| t.access)
}

/// Same as `sign_in`, with the token's lifetime and scopes. Asks for the scopes
/// `settings.toml` wants (read from the file).
pub async fn sign_in_token(http: &reqwest::Client, show_link: impl FnOnce(&str)) -> Result<Token> {
    sign_in_token_with(http, &settings_scopes(), show_link).await
}

/// The browser sign-in for exactly these scopes. A failure while file scopes were asked for
/// says how to back out when the organisation needs an admin to approve them.
pub async fn sign_in_token_with(
    http: &reqwest::Client,
    scopes: &ScopeSet,
    show_link: impl FnOnce(&str),
) -> Result<Token> {
    browser_sign_in(http, &scopes.scope_string(), show_link)
        .await
        .map_err(|e| add_scope_hint(e, scopes))
}

async fn browser_sign_in(
    http: &reqwest::Client,
    scope: &str,
    show_link: impl FnOnce(&str),
) -> Result<Token> {
    let app = app()?;
    let verifier = random_token();
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_token();

    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("sign-in listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("sign-in listener: {e}"))?
        .port();
    // Entra ignores the port on loopback redirects but not the path, so the plain
    // `http://localhost` registration under "Mobile and desktop" matches this.
    let redirect = format!("http://localhost:{port}");

    let mut url = reqwest::Url::parse(&authorize_url(&app.tenant_id)).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("client_id", &app.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_mode", "query")
        .append_pair("scope", scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");

    log::info(
        "teams-auth",
        &format!("browser sign-in started, loopback port {port}"),
    );
    open_browser(url.as_str());
    show_link(url.as_str());

    let expected_state = state.clone();
    // Dropping this future (the user quit during sign-in) sets the flag, so the blocking
    // thread ends at once instead of holding the process open until the timeout.
    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancel_flag = cancel.0.clone();
    let (code, mut browser) =
        tokio::task::spawn_blocking(move || wait_for_code(listener, &expected_state, &cancel_flag))
            .await
            .map_err(|e| format!("sign-in task: {e}"))?
            .inspect_err(|e| log::error("teams-auth", e))?;

    let form = [
        ("client_id", app.client_id.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier.as_str()),
        ("scope", scope),
    ];
    let result = exchange(http, &app, &form).await.map_err(String::from);
    let page = match &result {
        Ok(_) => result_page(
            "200 OK",
            "Signed in to chatcli",
            "Close this tab and go back to the terminal.",
        ),
        Err(_) => result_page(
            "400 Bad Request",
            "Sign-in did not complete",
            "The terminal shows the reason.",
        ),
    };
    let _ = browser.write_all(&page);
    let _ = browser.flush();
    result
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn result_page(status: &str, heading: &str, text: &str) -> Vec<u8> {
    http_response(
        status,
        &format!("<!doctype html><title>chatcli</title><body style='font-family:system-ui;padding:40px'><h2>{heading}</h2><p>{text}</p>"),
    )
}

/// Opens the sign-in page through `chatcli_core::open`: the `opener` crate calls ShellExecuteW on
/// Windows (xdg-open / open elsewhere) with the URL as one argument, so the `&` in the query is
/// never parsed by cmd, the same guarantee the old rundll32 call gave. Failure is fine: the link
/// is also printed. The error text holds no URL, so it is safe to log.
fn open_browser(url: &str) {
    if let Err(e) = chatcli_core::open::open_url(url) {
        log::error("teams-auth", &e);
    }
}

/// Accepts connections on the loopback listener until one carries the authorization code.
/// Returns the code with the browser's connection still open, so the caller can answer it
/// with the real result of the token exchange. Ends early when `cancel` is set.
fn wait_for_code(
    listener: TcpListener,
    expected_state: &str,
    cancel: &AtomicBool,
) -> Result<(String, TcpStream)> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("sign-in listener: {e}"))?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // On Windows an accepted socket inherits the listener's non-blocking mode, so
                // without this the read returns WouldBlock and every callback looks empty.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let request = read_request_head(&mut stream);
                if request.is_empty() {
                    // Browsers open spare connections and close them unused; not a callback.
                    continue;
                }
                match parse_callback(&request, expected_state) {
                    Ok(code) => return Ok((code, stream)),
                    Err(e) => {
                        let _ = stream.write_all(&result_page(
                            "400 Bad Request",
                            "Sign-in did not complete",
                            "The terminal shows the reason.",
                        ));
                        let _ = stream.flush();
                        // Microsoft said no: stop and show why.
                        if e.starts_with("Microsoft refused") {
                            return Err(e);
                        }
                        // A stray request (favicon, an old tab): keep waiting for the real one.
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Sign-in cancelled.".into());
                }
                if Instant::now() > deadline {
                    return Err(
                        "Sign-in timed out after 5 minutes. Quit chatcli, then run: chatcli teams login".into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("sign-in listener: {e}")),
        }
    }
}

fn http_response(status: &str, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 160);
    for line in [
        format!("HTTP/1.1 {status}"),
        "Content-Type: text/html; charset=utf-8".to_string(),
        format!("Content-Length: {}", body.len()),
        "Connection: close".to_string(),
        String::new(),
    ] {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(&CRLF);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

fn has_header_end(buf: &[u8]) -> bool {
    buf.windows(HEADER_END.len()).any(|w| w == HEADER_END)
}

/// Reads until the end of the request headers (or 8 KiB, or the read times out).
fn read_request_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if has_header_end(&buf) {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Pulls `code` out of `GET /?code=...&state=...`, checking the state. Entra appends a
/// trailing slash to a path-less redirect, so the callback lands on the root path.
fn parse_callback(request: &str, expected_state: &str) -> Result<String> {
    let line = request.lines().next().unwrap_or_default();
    let path = line.split_whitespace().nth(1).unwrap_or_default();
    if !(path == "/" || path.starts_with("/?")) {
        return Err("callback: unexpected path".into());
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}")).map_err(|e| e.to_string())?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => error_description = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = error {
        return Err(format!(
            "Microsoft refused the sign-in: {} ({e})",
            error_description.unwrap_or_default()
        ));
    }
    if state.as_deref() != Some(expected_state) {
        return Err("callback: state does not match (old tab or replay)".into());
    }
    code.ok_or_else(|| "callback: no code".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_code_stops_when_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let err = wait_for_code(listener, "s", &cancel).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let flag = Arc::new(AtomicBool::new(false));
        drop(CancelOnDrop(flag.clone()));
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback("GET /?code=abc&state=s1 HTTP/1.1", "s1").unwrap(),
            "abc"
        );
        assert!(parse_callback("GET /?code=abc&state=other HTTP/1.1", "s1").is_err());
        assert!(parse_callback("GET /favicon.ico HTTP/1.1", "s1").is_err());
        assert!(parse_callback("GET / HTTP/1.1", "s1").is_err(), "no code");
        let refused = parse_callback(
            "GET /?error=access_denied&error_description=User+said+no&state=s1 HTTP/1.1",
            "s1",
        )
        .unwrap_err();
        assert!(refused.starts_with("Microsoft refused"), "{refused}");
        assert!(refused.contains("User said no"), "{refused}");
    }

    /// The end-of-headers check must match a real CR LF CR LF, or every callback stalls.
    #[test]
    fn header_end_is_detected() {
        let mut request = b"GET /?code=abc HTTP/1.1".to_vec();
        request.extend_from_slice(&CRLF);
        request.extend_from_slice(b"Host: localhost");
        assert!(!has_header_end(&request), "headers not finished yet");
        request.extend_from_slice(&HEADER_END);
        assert!(has_header_end(&request));
    }

    #[test]
    fn response_has_crlf_header_lines() {
        let response = http_response("200 OK", "hi");
        let text = String::from_utf8(response).unwrap();
        let crlf = String::from_utf8(CRLF.to_vec()).unwrap();
        assert!(text.starts_with(&format!("HTTP/1.1 200 OK{crlf}")));
        assert!(text.ends_with(&format!("Connection: close{crlf}{crlf}hi")));
    }

    #[test]
    fn expiry_accepts_number_or_string_and_never_zero() {
        use serde_json::json;
        assert_eq!(expires_secs(Some(&json!(3599))), 3599);
        assert_eq!(expires_secs(Some(&json!("3599"))), 3599);
        assert_eq!(expires_secs(Some(&json!(0))), DEFAULT_EXPIRES_SECS);
        assert_eq!(expires_secs(Some(&json!("soon"))), DEFAULT_EXPIRES_SECS);
        assert_eq!(expires_secs(None), DEFAULT_EXPIRES_SECS);
    }

    #[test]
    fn rejection_covers_mfa_and_consent_but_not_outages() {
        assert!(is_rejection(400, "invalid_grant"));
        assert!(is_rejection(400, "interaction_required"));
        assert!(!is_rejection(500, "invalid_grant"));
        assert!(!is_rejection(400, "temporarily_unavailable"));
        assert!(!is_rejection(429, "invalid_grant"));
    }

    #[test]
    fn scopes_add_the_optional_ones_only_when_the_settings_ask() {
        let none = ScopeSet::default();
        assert_eq!(
            none.scope_string(),
            "offline_access User.Read Chat.ReadWrite ChatMessage.Send"
        );
        let read = ScopeSet {
            files_read: true,
            ..ScopeSet::default()
        };
        assert!(read.scope_string().ends_with(" Files.Read.All"));
        assert!(!read.scope_string().contains("Files.ReadWrite"));
        let both = ScopeSet {
            files_read: true,
            files_write: true,
            ..ScopeSet::default()
        };
        assert!(both
            .scope_string()
            .ends_with(" Files.Read.All Files.ReadWrite"));
        let from = ScopeSet::from_settings(&TeamsSettings {
            files_read: false,
            files_write: true,
            photos: true,
        });
        assert_eq!(
            from,
            ScopeSet {
                files_read: false,
                files_write: true,
                photos: true
            }
        );
        let photos = ScopeSet {
            photos: true,
            ..ScopeSet::default()
        };
        assert!(photos.scope_string().ends_with(" User.ReadBasic.All"));
        let attempts = photos.refresh_attempts();
        assert_eq!(attempts.len(), 3);
        assert!(attempts[0].contains("User.ReadBasic.All"));
        assert!(!attempts[1].contains("User.ReadBasic.All"));
    }

    /// A refresh for a scope the user never consented to must fall back, never end the sign-in.
    #[test]
    fn refresh_falls_back_to_fewer_scopes_down_to_what_0_1_asked_for() {
        let none = ScopeSet::default().refresh_attempts();
        assert_eq!(
            none,
            [
                "offline_access User.Read Chat.ReadWrite ChatMessage.Send",
                "offline_access User.Read Chat.ReadWrite"
            ]
        );
        let both = ScopeSet {
            files_read: true,
            files_write: true,
            ..ScopeSet::default()
        }
        .refresh_attempts();
        assert_eq!(both.len(), 3);
        assert!(both[0].contains("Files.ReadWrite"));
        assert_eq!(
            both[1],
            "offline_access User.Read Chat.ReadWrite ChatMessage.Send"
        );
        assert_eq!(both[2], "offline_access User.Read Chat.ReadWrite");
    }

    #[test]
    fn granted_scopes_come_from_the_token_then_the_scope_field() {
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let jwt = format!(
            "{}.{}.sig",
            enc("{}"),
            enc(r#"{"scp":"Chat.ReadWrite User.Read"}"#)
        );
        assert_eq!(
            granted_scopes(&jwt, Some("ignored")),
            ["Chat.ReadWrite", "User.Read"]
        );
        assert_eq!(granted_scopes("opaque", Some("A B")), ["A", "B"]);
        assert!(granted_scopes("opaque", None).is_empty());
    }

    #[test]
    fn a_failed_sign_in_with_file_scopes_says_how_to_back_out() {
        let files = ScopeSet {
            files_read: true,
            ..ScopeSet::default()
        };
        let hint = add_scope_hint("Need admin approval (AADSTS90094)".into(), &files);
        assert!(
            hint.contains("set files_read, files_write and photos to false"),
            "{hint}"
        );
        // No hint when no optional scope was asked for, or for an unrelated failure.
        let plain = add_scope_hint(
            "Need admin approval (AADSTS90094)".into(),
            &ScopeSet::default(),
        );
        assert!(!plain.contains("settings.toml"));
        assert_eq!(add_scope_hint("timed out".into(), &files), "timed out");
    }

    #[test]
    fn auth_failure_text_is_the_message() {
        assert_eq!(String::from(AuthFailure::Rejected("a".into())), "a");
        assert_eq!(String::from(AuthFailure::Other("b".into())), "b");
    }

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        // RFC 7636 appendix B.
        let challenge = b64url(&Sha256::digest(
            b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// Can this machine reach Entra at all (DNS, routing, TLS through any proxy)? A made-up
    /// app id in a made-up tenant must still come back as an HTTP 400 from Microsoft.
    /// Run with: cargo test token_endpoint -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs network access to login.microsoftonline.com"]
    async fn token_endpoint_answers() {
        let http = http_client().unwrap();
        // Deliberately not exchange(): invalid_grant there would clear a real stored token.
        let form = [
            ("client_id", "00000000-0000-0000-0000-000000000000"),
            ("grant_type", "refresh_token"),
            ("refresh_token", "definitely-not-a-token"),
            ("scope", BASE_SCOPES),
        ];
        let response = http
            .post(token_url("contoso.onmicrosoft.com"))
            .form(&form)
            .send()
            .await
            .unwrap_or_else(|e| panic!("no answer from Entra: {}", chain(&e)));
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        println!(
            "HTTP {status}: {}",
            body.chars().take(300).collect::<String>()
        );
        assert_eq!(status.as_u16(), 400, "expected invalid_grant, got {status}");
    }
}
