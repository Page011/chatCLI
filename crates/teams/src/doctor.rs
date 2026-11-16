//! `chatcli doctor`, the Teams half: what can be checked about sign-in and the relay without
//! sending anything but harmless reads. Every check has a level, one line of detail and, when it
//! is not fine, the fix. Nothing here prints a token, a client id or a message.
//!
//! The authenticated relay probe is a plain GET to `/ws` with no WebSocket upgrade: the Worker
//! answers 426 for a good token and 401 for a bad one. A real WebSocket connect would make the
//! Worker close the socket of a chatcli that is running (it keeps one connection per user).

use std::time::Duration;

use chatcli_core::doctor::{Check, Level};
use chatcli_core::{chain, config, log};

use crate::auth;
use crate::graph::Graph;
use crate::relay;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn check(name: &str, level: Level, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        name: name.to_string(),
        level,
        detail: detail.into(),
        fix: fix.map(str::to_string),
    }
}

/// Runs the Teams checks in order. `online` adds one `GET /subscriptions` through the stored
/// sign-in (it refreshes the token, which Microsoft rotates: harmless, but it is a sign-in).
pub async fn checks(online: bool) -> Vec<Check> {
    let mut out = Vec::new();
    let http = match auth::http_client() {
        Ok(h) => {
            out.push(check(
                "Teams: TLS roots",
                Level::Ok,
                "HTTPS uses the Windows root store",
                None,
            ));
            Some(h)
        }
        Err(e) => {
            out.push(check(
                "Teams: TLS roots",
                Level::Fail,
                e,
                Some("Check your network or proxy settings, then run chatcli doctor again."),
            ));
            None
        }
    };
    out.push(ssl_cert_file_check(
        std::env::var("SSL_CERT_FILE").ok().as_deref(),
    ));

    let app = auth::app();
    match &app {
        Ok(_) => out.push(check(
            "Teams: app registration",
            Level::Ok,
            "client and tenant ids are set",
            None,
        )),
        Err(e) => out.push(check(
            "Teams: app registration",
            Level::Warn,
            e.clone(),
            Some("chatcli teams setup <client-id> <tenant-id>"),
        )),
    }
    if let (Some(http), Ok(app)) = (&http, &app) {
        let url = signin_metadata_url(&app.tenant_id);
        out.push(status_check(
            "Teams: sign-in host",
            "login.microsoftonline.com",
            probe(http, &url, None).await,
            200,
            "Allow login.microsoftonline.com on this network (or fix TLS inspection), then try again.",
        ));
    }
    out.push(match auth::has_stored_sign_in() {
        Ok(true) => check("Teams: sign-in", Level::Ok, "a sign-in is stored", None),
        Ok(false) => check(
            "Teams: sign-in",
            Level::Warn,
            "not signed in",
            Some("Open chatcli and press Enter on the Teams tab, or run: chatcli teams login"),
        ),
        Err(e) => check("Teams: sign-in", Level::Fail, e, None),
    });

    let url = config::load().ok().and_then(|c| c.relay_url);
    let secrets = relay::load_secrets();
    out.push(relay_secrets_check(url.as_deref(), &secrets));
    if let (Some(http), Some(url), Ok(Some(secrets))) = (&http, &url, &secrets) {
        let host = url.trim_start_matches("https://");
        out.push(status_check(
            "Relay: /health",
            host,
            probe(http, &format!("{url}/health"), None).await,
            200,
            "Deploy the Worker again: cd relay, then bunx wrangler deploy.",
        ));
        out.push(ws_check(
            probe(http, &format!("{url}/ws"), Some(&secrets.token)).await,
        ));
    }
    if online {
        out.push(match http {
            Some(ref http) => subscriptions_check(http).await,
            None => check("Teams: subscriptions", Level::Fail, "no HTTP client", None),
        });
    }
    log::info(
        "doctor",
        &format!(
            "teams checks: {} ok, {} warn, {} fail",
            out.iter().filter(|c| c.level == Level::Ok).count(),
            out.iter().filter(|c| c.level == Level::Warn).count(),
            out.iter().filter(|c| c.level == Level::Fail).count()
        ),
    );
    out
}

/// Entra's public metadata for the tenant: no sign-in needed, so a 200 proves the host is
/// reachable and its certificate trusted.
fn signin_metadata_url(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/v2.0/.well-known/openid-configuration")
}

/// `SSL_CERT_FILE` replaces the Windows root store for rustls: a work network that re-signs
/// HTTPS then fails with `UnknownIssuer`.
fn ssl_cert_file_check(value: Option<&str>) -> Check {
    match value {
        Some(_) => check(
            "Teams: SSL_CERT_FILE",
            Level::Warn,
            "SSL_CERT_FILE is set and replaces the Windows root store for chatcli",
            Some("Remove the SSL_CERT_FILE environment variable (Windows: System Properties, Environment Variables)."),
        ),
        None => check("Teams: SSL_CERT_FILE", Level::Ok, "not set", None),
    }
}

/// The relay URL and both secrets must be there for live updates. The secret names are
/// reported, never their values.
fn relay_secrets_check(
    url: Option<&str>,
    secrets: &chatcli_core::Result<Option<relay::RelaySecrets>>,
) -> Check {
    match (url, secrets) {
        (None, _) => check(
            "Relay: setup",
            Level::Warn,
            "no relay URL: Teams works without live updates",
            Some("chatcli relay setup <url> (README, relay setup)"),
        ),
        (Some(_), Err(e)) => check("Relay: secrets", Level::Fail, e.clone(), None),
        (Some(_), Ok(None)) => check(
            "Relay: secrets",
            Level::Fail,
            format!(
                "{} and {} are missing from the credential store",
                relay::TOKEN_NAME,
                relay::CLIENT_STATE_NAME
            ),
            Some("chatcli relay setup <url> --rotate, then redo both wrangler secret put steps."),
        ),
        (Some(_), Ok(Some(_))) => check(
            "Relay: secrets",
            Level::Ok,
            format!(
                "{} and {} are stored",
                relay::TOKEN_NAME,
                relay::CLIENT_STATE_NAME
            ),
            None,
        ),
    }
}

/// One GET, bounded by a timeout. The HTTP status, or the failure text. A bearer token is
/// sent only when one is given, and never appears in the result.
async fn probe(http: &reqwest::Client, url: &str, bearer: Option<&str>) -> Result<u16, String> {
    let mut request = http.get(url).timeout(PROBE_TIMEOUT);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    request
        .send()
        .await
        .map(|r| r.status().as_u16())
        .map_err(|e| chain(&e))
}

fn status_check(name: &str, what: &str, got: Result<u16, String>, want: u16, fix: &str) -> Check {
    match got {
        Ok(s) if s == want => check(name, Level::Ok, format!("{what} answers HTTP {s}"), None),
        Ok(s) => check(
            name,
            Level::Fail,
            format!("{what} answered HTTP {s}, expected {want}"),
            Some(fix),
        ),
        Err(e) => check(
            name,
            Level::Fail,
            format!("{what} is not reachable: {e}"),
            Some(fix),
        ),
    }
}

/// 426 = the Worker wants a WebSocket upgrade, so it accepted the token. 401 = wrong token.
fn ws_check(got: Result<u16, String>) -> Check {
    let name = "Relay: token";
    match got {
        Ok(426) => check(name, Level::Ok, "the Worker accepts the stored token", None),
        Ok(401) => check(
            name,
            Level::Fail,
            "the Worker rejects the stored token (HTTP 401)",
            Some("chatcli relay setup <url> --rotate, then redo both wrangler secret put steps."),
        ),
        Ok(s) => check(
            name,
            Level::Fail,
            format!("unexpected answer HTTP {s} from /ws"),
            Some("Deploy the Worker again: cd relay, then bunx wrangler deploy."),
        ),
        Err(e) => check(
            name,
            Level::Fail,
            format!("the relay is not reachable: {e}"),
            Some("Check the relay URL: chatcli relay url <url>"),
        ),
    }
}

async fn subscriptions_check(http: &reqwest::Client) -> Check {
    let name = "Teams: subscriptions";
    let token = match auth::access_token(http).await {
        Ok(t) => t,
        Err(e) => {
            return check(
                name,
                Level::Fail,
                e,
                Some("Sign in again: chatcli teams login"),
            )
        }
    };
    match Graph::new(http.clone(), token).subscriptions().await {
        Ok(subs) => check(
            name,
            Level::Ok,
            format!(
                "Graph answers; {} subscription(s) on this account",
                subs.len()
            ),
            None,
        ),
        Err(e) => check(
            name,
            Level::Fail,
            e.message,
            Some("Sign in again (chatcli teams login) or check the Entra app permissions."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Mock, Reply};

    fn level_of(c: &Check) -> Level {
        c.level
    }

    #[test]
    fn ssl_cert_file_is_a_warning_only_when_set() {
        assert_eq!(level_of(&ssl_cert_file_check(None)), Level::Ok);
        let c = ssl_cert_file_check(Some("C:/certs/x.pem"));
        assert_eq!(c.level, Level::Warn);
        assert!(c.fix.is_some());
        assert!(!c.detail.contains("x.pem"), "the path is not echoed");
    }

    #[test]
    fn relay_secret_checks_name_the_secrets_and_never_print_them() {
        let both = Ok(Some(relay::RelaySecrets {
            token: "super-secret-token".into(),
            client_state: "super-secret-state".into(),
        }));
        let c = relay_secrets_check(Some("https://r.example"), &both);
        assert_eq!(c.level, Level::Ok);
        assert!(c.detail.contains("relay-token") && !c.detail.contains("super-secret"));
        assert_eq!(relay_secrets_check(None, &both).level, Level::Warn);
        let missing = relay_secrets_check(Some("https://r.example"), &Ok(None));
        assert_eq!(missing.level, Level::Fail);
        assert!(missing.fix.unwrap().contains("--rotate"));
        let broken = relay_secrets_check(Some("https://r.example"), &Err("store locked".into()));
        assert_eq!(
            (broken.level, broken.detail.as_str()),
            (Level::Fail, "store locked")
        );
    }

    #[test]
    fn status_checks_say_what_answered() {
        assert_eq!(
            status_check("n", "host", Ok(200), 200, "fix").level,
            Level::Ok
        );
        let c = status_check("n", "host", Ok(503), 200, "fix");
        assert_eq!((c.level, c.fix.as_deref()), (Level::Fail, Some("fix")));
        assert!(c.detail.contains("503"));
        let c = status_check("n", "host", Err("connection refused".into()), 200, "fix");
        assert!(c.detail.contains("connection refused"));
    }

    #[test]
    fn the_ws_probe_reads_426_as_a_good_token_and_401_as_a_bad_one() {
        assert_eq!(ws_check(Ok(426)).level, Level::Ok);
        let bad = ws_check(Ok(401));
        assert_eq!(bad.level, Level::Fail);
        assert!(bad.fix.unwrap().contains("--rotate"));
        assert_eq!(ws_check(Ok(500)).level, Level::Fail);
        assert_eq!(ws_check(Err("timed out".into())).level, Level::Fail);
    }

    #[test]
    fn the_sign_in_probe_asks_for_public_metadata() {
        assert_eq!(
            signin_metadata_url("contoso.onmicrosoft.com"),
            "https://login.microsoftonline.com/contoso.onmicrosoft.com/v2.0/.well-known/openid-configuration"
        );
    }

    /// The probe without a WebSocket upgrade: the Worker's 426 means the token was accepted, and
    /// nothing in it can evict a running chatcli.
    #[tokio::test]
    async fn the_relay_probes_run_against_a_worker_without_opening_a_socket() {
        let mock = Mock::start(|r| match (r.path(), r.header("authorization")) {
            ("/health", _) => Reply::bytes(200, "text/plain", b"ok".to_vec()),
            ("/ws", Some("Bearer good")) => {
                Reply::bytes(426, "text/plain", b"websocket upgrade required".to_vec())
            }
            ("/ws", _) => Reply::bytes(401, "text/plain", b"unauthorized".to_vec()),
            _ => Reply::empty(404),
        });
        let http = reqwest::Client::new();
        let url = mock.origin();
        assert_eq!(probe(&http, &format!("{url}/health"), None).await, Ok(200));
        assert_eq!(
            ws_check(probe(&http, &format!("{url}/ws"), Some("good")).await).level,
            Level::Ok
        );
        assert_eq!(
            ws_check(probe(&http, &format!("{url}/ws"), Some("bad")).await).level,
            Level::Fail
        );
        let reqs = mock.requests();
        assert!(
            reqs.iter().all(|r| r.header("upgrade").is_none()),
            "no WebSocket upgrade was ever asked for"
        );
        // A server that is down is a failure with a reason, not a hang.
        let gone = probe(&http, "http://127.0.0.1:1/health", None).await;
        assert!(gone.is_err());
    }
}
