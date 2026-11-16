//! The one Graph change-notification subscription per user: create, renew at half-life,
//! reauthorize, delete, and clean up orphans left by a crashed run (Graph allows 10 per user,
//! so a leak is a real failure).
//!
//! The subscription id is kept in store meta `teams.subscription_id`. The expiry is kept in
//! memory only: after a restart the renew call returns the real one.

use chrono::{DateTime, SecondsFormat};
use sha2::{Digest, Sha256};

use chatcli_core::log;
use chatcli_core::store::Store;

use crate::graph::{ApiError, ApiResult, Graph, Subscription};

pub const SUB_ID_KEY: &str = "teams.subscription_id";
/// What the stored subscription was created for (see `spec_fingerprint`).
const SPEC_KEY: &str = "teams.subscription_spec";
/// Teams chat messages allow up to 3 days; 24 h keeps a missed renewal cheap. Anything over
/// 1 hour needs a lifecycleNotificationUrl, which is always set.
pub const LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

/// What a subscription needs to know about this user and relay.
#[derive(Debug, Clone)]
pub struct Spec {
    pub user_id: String,
    /// `https://host`, no trailing slash (config normalizes it).
    pub relay_url: String,
    pub client_state: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubState {
    pub id: String,
    pub expires_ms: i64,
    /// Half-life: when the backend renews.
    pub renew_at_ms: i64,
}

impl SubState {
    /// Delivering notifications: not past its expiry.
    pub fn active(&self, now_ms: i64) -> bool {
        now_ms < self.expires_ms
    }
}

/// Identifies the relay and secrets a subscription was created with, without keeping the
/// secret itself. If it differs from the current Spec, the stored subscription still points at
/// the old relay (or carries an old clientState) and renewing it would look live but deliver
/// nothing.
pub fn spec_fingerprint(spec: &Spec) -> String {
    let mut h = Sha256::new();
    for part in [
        spec.relay_url.as_str(),
        spec.client_state.as_str(),
        spec.user_id.as_str(),
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn notification_url(relay_url: &str) -> String {
    format!("{relay_url}/graph/notify")
}

pub fn lifecycle_url(relay_url: &str) -> String {
    format!("{relay_url}/graph/lifecycle")
}

/// RFC 3339 UTC, whole seconds: `2026-11-17T01:02:03Z`.
pub fn expiration_iso(now_ms: i64, lifetime_ms: i64) -> String {
    DateTime::from_timestamp_millis(now_ms + lifetime_ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Uses the expiry Graph reports (it may shorten the request) and renews at the midpoint
/// between now and that expiry.
pub fn state_from(sub: &Subscription, requested_ms: i64, now_ms: i64) -> SubState {
    let expires_ms = sub
        .expiration_date_time
        .as_deref()
        .and_then(chatcli_core::model::parse_iso_ms)
        .unwrap_or(requested_ms);
    SubState {
        id: sub.id.clone(),
        expires_ms,
        renew_at_ms: now_ms + (expires_ms - now_ms).max(0) / 2,
    }
}

pub fn create_body(spec: &Spec, expiration: &str) -> serde_json::Value {
    serde_json::json!({
        "changeType": "created,updated,deleted",
        "notificationUrl": notification_url(&spec.relay_url),
        "lifecycleNotificationUrl": lifecycle_url(&spec.relay_url),
        "resource": format!("/users/{}/chats/getAllMessages", spec.user_id),
        "includeResourceData": false,
        "expirationDateTime": expiration,
        "clientState": spec.client_state,
    })
}

/// Ids of our own earlier subscriptions: same notification URL, not the one to keep.
pub fn orphans(all: &[Subscription], notification_url: &str, keep_id: Option<&str>) -> Vec<String> {
    all.iter()
        .filter(|s| {
            s.notification_url
                .as_deref()
                .is_some_and(|u| u.eq_ignore_ascii_case(notification_url))
        })
        .filter(|s| Some(s.id.as_str()) != keep_id)
        .map(|s| s.id.clone())
        .collect()
}

/// A renew that failed because the subscription is gone (expired while the laptop slept,
/// or removed by Graph): create a new one rather than give up.
fn is_gone(e: &ApiError) -> bool {
    e.status == 404 || e.status == 400
}

fn remember(store: &Store, id: &str) {
    if let Err(e) = store.set_meta(SUB_ID_KEY, Some(id)) {
        log::error(
            "teams-subscription",
            &format!("cannot remember subscription id: {e}"),
        );
    }
}

/// Renews the stored subscription, or deletes orphans and creates a new one. Returns the
/// state to track. Errors carry Graph's own message.
pub async fn ensure(g: &Graph, store: &Store, spec: &Spec, now_ms: i64) -> ApiResult<SubState> {
    let stored = store.meta(SUB_ID_KEY).ok().flatten();
    if let Some(id) = stored {
        let current = spec_fingerprint(spec);
        if store.meta(SPEC_KEY).ok().flatten().as_deref() != Some(current.as_str()) {
            log::info(
                "teams-subscription",
                "relay settings changed since the stored subscription, replacing it",
            );
            if let Err(e) = g.delete_subscription(&id).await {
                log::error(
                    "teams-subscription",
                    &format!("cannot delete the old subscription: {}", e.message),
                );
            }
        } else {
            match renew(g, &id, now_ms).await {
                Ok(state) => {
                    log::info("teams-subscription", "existing subscription renewed");
                    return Ok(state);
                }
                Err(e) if is_gone(&e) => {
                    log::info(
                        "teams-subscription",
                        &format!(
                            "stored subscription is gone ({}), creating a new one",
                            e.message
                        ),
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }
    create(g, store, spec, now_ms).await
}

pub async fn renew(g: &Graph, id: &str, now_ms: i64) -> ApiResult<SubState> {
    let requested = now_ms + LIFETIME_MS;
    let sub = g
        .renew_subscription(id, &expiration_iso(now_ms, LIFETIME_MS))
        .await?;
    Ok(state_from(&sub, requested, now_ms))
}

async fn create(g: &Graph, store: &Store, spec: &Spec, now_ms: i64) -> ApiResult<SubState> {
    let url = notification_url(&spec.relay_url);
    // Orphans first: a crash leaves the old subscription alive, and 10 is the limit.
    let all = g.subscriptions().await?;
    let stale = orphans(&all, &url, None);
    for id in &stale {
        match g.delete_subscription(id).await {
            Ok(()) => {}
            Err(e) => log::error(
                "teams-subscription",
                &format!("cannot delete orphan subscription: {}", e.message),
            ),
        }
    }
    if !stale.is_empty() {
        log::info(
            "teams-subscription",
            &format!("deleted {} orphan subscription(s)", stale.len()),
        );
    }
    let requested = now_ms + LIFETIME_MS;
    let sub = g
        .create_subscription(&create_body(spec, &expiration_iso(now_ms, LIFETIME_MS)))
        .await?;
    remember(store, &sub.id);
    if let Err(e) = store.set_meta(SPEC_KEY, Some(&spec_fingerprint(spec))) {
        log::error(
            "teams-subscription",
            &format!("cannot remember subscription settings: {e}"),
        );
    }
    log::info("teams-subscription", "subscription created");
    Ok(state_from(&sub, requested, now_ms))
}

/// Graph said `reauthorizationRequired`. One PATCH with a new expiry both reauthorizes and
/// renews. The `reauthorize` POST is deliberately not sent as well: Microsoft warns that the two
/// calls within ten minutes can leave the subscription in an inconsistent state.
pub async fn reauthorize(g: &Graph, id: &str, now_ms: i64) -> ApiResult<SubState> {
    let state = renew(g, id, now_ms).await?;
    log::info(
        "teams-subscription",
        "subscription reauthorized and renewed",
    );
    Ok(state)
}

/// Deletes the subscription on Graph and forgets its id.
pub async fn remove(g: &Graph, store: &Store, id: &str) -> ApiResult<()> {
    let result = g.delete_subscription(id).await;
    if let Err(e) = store.set_meta(SUB_ID_KEY, None) {
        log::error(
            "teams-subscription",
            &format!("cannot clear subscription id: {e}"),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, url: Option<&str>, exp: Option<&str>) -> Subscription {
        Subscription {
            id: id.into(),
            notification_url: url.map(str::to_string),
            expiration_date_time: exp.map(str::to_string),
        }
    }

    fn spec() -> Spec {
        Spec {
            user_id: "00000000-0000-0000-0000-000000000001".into(),
            relay_url: "https://chatcli-relay.example.workers.dev".into(),
            client_state: "state".into(),
        }
    }

    #[test]
    fn expiry_is_rfc3339_utc_24h_ahead() {
        let now = chatcli_core::model::parse_iso_ms("2026-11-16T01:02:03.789Z").unwrap();
        assert_eq!(expiration_iso(now, LIFETIME_MS), "2026-11-17T01:02:03Z");
        assert_eq!(expiration_iso(0, 3_600_000), "1970-01-01T01:00:00Z");
    }

    #[test]
    fn renew_is_due_at_half_life_of_the_expiry_graph_reports() {
        let now = 1_000_000;
        let granted = expiration_iso(now, LIFETIME_MS);
        let s = state_from(&sub("a", None, Some(&granted)), now + LIFETIME_MS, now);
        assert_eq!(s.id, "a");
        // Whole-second rounding in the ISO text: allow under a second of difference.
        assert!(
            (s.renew_at_ms - (now + LIFETIME_MS / 2)).abs() < 1000,
            "{s:?}"
        );
        assert!(s.active(now) && s.active(s.expires_ms - 1) && !s.active(s.expires_ms));
        // Graph shortened the lifetime to 1 h: renew at 30 min.
        let short = expiration_iso(now, 3_600_000);
        let s = state_from(&sub("a", None, Some(&short)), now + LIFETIME_MS, now);
        assert!((s.renew_at_ms - (now + 1_800_000)).abs() < 1000, "{s:?}");
        // No expiry in the answer: fall back to what was requested.
        let s = state_from(&sub("a", None, None), now + 1000, now);
        assert_eq!((s.expires_ms, s.renew_at_ms), (now + 1000, now + 500));
        // Already past expiry: renew immediately, never in the past-negative.
        let s = state_from(&sub("a", None, Some("1970-01-01T00:00:00Z")), 0, now);
        assert_eq!(s.renew_at_ms, now);
    }

    #[test]
    fn body_has_the_documented_fields_and_no_resource_data() {
        let body = create_body(&spec(), "2026-11-17T01:02:03Z");
        assert_eq!(
            body["resource"],
            "/users/00000000-0000-0000-0000-000000000001/chats/getAllMessages"
        );
        assert_eq!(body["changeType"], "created,updated,deleted");
        assert_eq!(body["includeResourceData"], false);
        assert_eq!(
            body["notificationUrl"],
            "https://chatcli-relay.example.workers.dev/graph/notify"
        );
        assert_eq!(
            body["lifecycleNotificationUrl"],
            "https://chatcli-relay.example.workers.dev/graph/lifecycle"
        );
        assert_eq!(body["clientState"], "state");
        assert_eq!(body["expirationDateTime"], "2026-11-17T01:02:03Z");
    }

    #[test]
    fn orphans_are_ours_with_another_id() {
        let ours = "https://chatcli-relay.example.workers.dev/graph/notify";
        let all = [
            sub("keep", Some(ours), None),
            sub("old1", Some(ours), None),
            sub(
                "old2",
                Some("HTTPS://CHATCLI-RELAY.EXAMPLE.WORKERS.DEV/graph/notify"),
                None,
            ),
            sub("other-app", Some("https://other.example/hook"), None),
            sub("no-url", None, None),
        ];
        assert_eq!(orphans(&all, ours, Some("keep")), ["old1", "old2"]);
        assert_eq!(orphans(&all, ours, None).len(), 3);
    }

    #[test]
    fn fingerprint_changes_with_relay_or_client_state() {
        let a = spec();
        assert_eq!(spec_fingerprint(&a), spec_fingerprint(&a.clone()));
        let mut b = a.clone();
        b.relay_url = "https://other.example.workers.dev".into();
        let mut c = a.clone();
        c.client_state = "other".into();
        assert_ne!(spec_fingerprint(&a), spec_fingerprint(&b));
        assert_ne!(spec_fingerprint(&a), spec_fingerprint(&c));
        assert!(!spec_fingerprint(&a).contains("state"));
    }

    /// CodeRabbit 6: reauthorize POST then renew PATCH on one subscription is the sequence
    /// Microsoft says not to send. A reauthorization event is one PATCH with a new expiry.
    #[tokio::test]
    async fn a_reauthorization_request_sends_one_patch_and_no_reauthorize_post() {
        use crate::testkit::{Mock, Reply};
        let mock = Mock::start(|_| {
            Reply::json(
                200,
                serde_json::json!({ "id": "sub1", "expirationDateTime": "2099-01-01T00:00:00Z" }),
            )
        });
        let g = Graph::new(reqwest::Client::new(), "tok".into()).with_base(&mock.graph_base());
        let state = reauthorize(&g, "sub1", 1_000_000).await.unwrap();
        assert_eq!(state.id, "sub1");
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1, "exactly one request");
        assert_eq!(reqs[0].method, "PATCH");
        assert_eq!(reqs[0].path(), "/v1.0/subscriptions/sub1");
        assert!(reqs[0].json()["expirationDateTime"].is_string());
        assert!(mock.hits("/reauthorize").is_empty());
    }

    #[tokio::test]
    async fn a_failed_reauthorization_is_an_error_not_a_second_request() {
        use crate::testkit::{Mock, Reply};
        let mock = Mock::start(|_| Reply::error(403, "Forbidden", "consent revoked"));
        let g = Graph::new(reqwest::Client::new(), "tok".into()).with_base(&mock.graph_base());
        let err = reauthorize(&g, "sub1", 0).await.unwrap_err();
        assert_eq!(err.status, 403);
        assert_eq!(mock.count(), 1);
    }

    #[test]
    fn gone_means_not_found_or_bad_request_only() {
        let e = |status| ApiError {
            status,
            message: String::new(),
        };
        assert!(is_gone(&e(404)) && is_gone(&e(400)));
        assert!(!is_gone(&e(401)) && !is_gone(&e(429)) && !is_gone(&e(0)) && !is_gone(&e(503)));
    }
}
