//! The Graph calls chatcli needs for Teams chats. No timers here: Microsoft allows polling
//! Teams APIs at most once a day, so every call is triggered by the user or by a change
//! notification (relay, later).

use serde::de::DeserializeOwned;
use serde::Deserialize;

use chatcli_core::{chain, Result};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Graph's maximum page size for chats and chat messages.
pub const MAX_PAGE: u32 = 50;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    pub topic: Option<String>,
    /// oneOnOne, group or meeting.
    pub chat_type: String,
    pub members: Option<Vec<Member>>,
    pub last_message_preview: Option<Preview>,
    pub viewpoint: Option<Viewpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub display_name: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub created_date_time: Option<String>,
    pub is_deleted: Option<bool>,
    pub message_type: Option<String>,
    pub body: Option<Body>,
    pub from: Option<From>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewpoint {
    pub is_hidden: Option<bool>,
    pub last_message_read_date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Body {
    pub content_type: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct From {
    pub user: Option<Identity>,
    pub application: Option<Identity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub created_date_time: Option<String>,
    pub message_type: Option<String>,
    pub deleted_date_time: Option<String>,
    pub from: Option<From>,
    pub body: Option<Body>,
}

#[derive(Deserialize)]
struct Page<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// One Graph change-notification subscription.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub notification_url: Option<String>,
    pub expiration_date_time: Option<String>,
}

/// All chats found, and whether the page cap stopped the listing early.
#[derive(Debug)]
pub struct ChatPage {
    pub chats: Vec<Chat>,
    pub capped: bool,
}

/// What to do after a page of results.
#[derive(Debug, PartialEq)]
pub enum NextStep {
    Done,
    Fetch(String),
    /// More pages exist but the cap was reached. Never silent: callers log and tell the user.
    Capped,
    /// The next link points somewhere other than Graph. The bearer token must not follow it.
    Foreign,
}

fn is_graph_url(link: &str) -> bool {
    link.starts_with("https://graph.microsoft.com/")
}

pub fn next_step(pages_fetched: u32, next_link: Option<&str>, max_pages: u32) -> NextStep {
    match next_link {
        None => NextStep::Done,
        Some(link) if !is_graph_url(link) => NextStep::Foreign,
        Some(_) if pages_fetched >= max_pages => NextStep::Capped,
        Some(link) => NextStep::Fetch(link.to_string()),
    }
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

/// A Graph failure with its HTTP status (0 = no HTTP answer), so callers can react to a
/// specific one, for example refresh the token on 401.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

impl std::convert::From<ApiError> for String {
    fn from(e: ApiError) -> String {
        e.message
    }
}

impl std::convert::From<String> for ApiError {
    fn from(message: String) -> ApiError {
        ApiError { status: 0, message }
    }
}

impl Chat {
    /// Topic for named group chats, otherwise the other members' names.
    pub fn title(&self, me_id: &str) -> String {
        if let Some(topic) = self
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return topic.to_string();
        }
        let others: Vec<&str> = self
            .members
            .iter()
            .flatten()
            .filter(|m| m.user_id.as_deref() != Some(me_id))
            .filter_map(|m| m.display_name.as_deref())
            .collect();
        if others.is_empty() {
            "(untitled chat)".to_string()
        } else {
            others.join(", ")
        }
    }

    /// The last message is newer than the user's read mark.
    // Limit: compares ISO timestamps to the second as text; a message in the same second
    // as the read mark counts as read.
    pub fn is_unread(&self) -> bool {
        let last = self
            .last_message_preview
            .as_ref()
            .and_then(|p| p.created_date_time.as_deref());
        let read = self
            .viewpoint
            .as_ref()
            .and_then(|v| v.last_message_read_date_time.as_deref());
        match (last, read) {
            (Some(last), Some(read)) => second(last) > second(read),
            (Some(_), None) => true,
            _ => false,
        }
    }
}

fn second(iso: &str) -> &str {
    iso.get(..19).unwrap_or(iso)
}

impl From {
    pub fn name(&self) -> &str {
        self.user
            .as_ref()
            .or(self.application.as_ref())
            .and_then(|i| i.display_name.as_deref())
            .unwrap_or("(unknown)")
    }
}

impl Body {
    pub fn text(&self) -> String {
        let content = self.content.as_deref().unwrap_or_default();
        if self.content_type.as_deref() == Some("html") {
            crate::html::to_text(content)
        } else {
            content.split_whitespace().collect::<Vec<_>>().join(" ")
        }
    }
}

impl Message {
    pub fn sender(&self) -> &str {
        self.from.as_ref().map_or("(system)", From::name)
    }

    pub fn text(&self) -> String {
        if self.deleted_date_time.is_some() {
            return "[deleted]".to_string();
        }
        if self.message_type.as_deref().is_some_and(|t| t != "message") {
            return "[system event]".to_string();
        }
        self.body.as_ref().map(Body::text).unwrap_or_default()
    }
}

#[derive(Clone)]
pub struct Graph {
    http: reqwest::Client,
    token: String,
}

fn url(segments: &[&str], query: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(GRAPH).map_err(|e| e.to_string())?;
    // push() percent-encodes '/', '?' and '#', so a chat id typed by the user can never
    // reach a different Graph path.
    url.path_segments_mut()
        .map_err(|_| "Graph base URL cannot take a path".to_string())?
        .extend(segments);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Ok(url)
}

impl Graph {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    /// Sends the request and returns the response body text. Any non-2xx becomes an ApiError
    /// carrying Graph's own code and message.
    async fn raw(&self, request: reqwest::RequestBuilder) -> ApiResult<String> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ApiError {
                status: 0,
                message: format!("Graph: {}", chain(&e)),
            })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.text().await.map_err(|e| ApiError {
            status: 0,
            message: format!("Graph: {}", chain(&e)),
        })?;
        if !status.is_success() {
            let detail = serde_json::from_str::<ErrorEnvelope>(&body)
                .map(|e| format!("{}: {}", e.error.code, e.error.message))
                .unwrap_or_else(|_| body.chars().take(300).collect());
            let message = match status.as_u16() {
                429 => format!(
                    "Teams is throttling requests. Try again in {} s. ({detail})",
                    retry_after.as_deref().unwrap_or("a few")
                ),
                401 => format!("Teams rejected the sign-in. Sign in again. ({detail})"),
                403 => format!("Teams refused access (missing permission or consent). ({detail})"),
                _ => format!("Graph HTTP {status}: {detail}"),
            };
            return Err(ApiError {
                status: status.as_u16(),
                message,
            });
        }
        Ok(body)
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> ApiResult<T> {
        let body = self.raw(request).await?;
        serde_json::from_str(&body).map_err(|e| ApiError {
            status: 200,
            message: format!("Graph: unexpected response: {e}"),
        })
    }

    async fn get<T: DeserializeOwned>(&self, url: reqwest::Url) -> ApiResult<T> {
        self.send(self.http.get(url)).await
    }

    pub async fn me(&self) -> ApiResult<Me> {
        self.get(url(&["me"], &[])?).await
    }

    /// The signed-in user's chats, most recent message first (first page of up to 50).
    pub async fn chats(&self) -> ApiResult<Vec<Chat>> {
        Ok(self.first_chat_page().await?.value)
    }

    async fn first_chat_page(&self) -> ApiResult<Page<Chat>> {
        let top = MAX_PAGE.to_string();
        let query = |expand| {
            [
                ("$expand", expand),
                ("$orderby", "lastMessagePreview/createdDateTime desc"),
                ("$top", top.as_str()),
            ]
        };
        let both = url(&["me", "chats"], &query("members,lastMessagePreview"))?;
        match self.get::<Page<Chat>>(both).await {
            Ok(page) => Ok(page),
            // Graph documents members and lastMessagePreview as expandable but not whether
            // together. If refused, 1:1 chats go without member names rather than failing.
            Err(e) if e.status == 400 => {
                let preview_only = url(&["me", "chats"], &query("lastMessagePreview"))?;
                self.get::<Page<Chat>>(preview_only).await
            }
            Err(e) => Err(e),
        }
    }

    /// Every chat, following `@odata.nextLink` for at most `max_pages` pages. Hitting the cap
    /// is reported in `capped`, never swallowed.
    pub async fn chats_all(&self, max_pages: u32) -> ApiResult<ChatPage> {
        let mut page = self.first_chat_page().await?;
        let mut chats = Vec::new();
        let mut fetched = 1;
        loop {
            chats.append(&mut page.value);
            match next_step(fetched, page.next_link.as_deref(), max_pages) {
                NextStep::Done => {
                    return Ok(ChatPage {
                        chats,
                        capped: false,
                    })
                }
                NextStep::Capped => {
                    return Ok(ChatPage {
                        chats,
                        capped: true,
                    })
                }
                NextStep::Foreign => {
                    return Err(
                        "Graph sent a next-page link to another host, so the chat list stops here."
                            .to_string()
                            .into(),
                    )
                }
                NextStep::Fetch(link) => {
                    let link = reqwest::Url::parse(&link).map_err(|e| e.to_string())?;
                    page = self.get(link).await?;
                    fetched += 1;
                }
            }
        }
    }

    pub async fn chat(&self, chat_id: &str) -> ApiResult<Chat> {
        self.get(url(&["chats", chat_id], &[("$expand", "members")])?)
            .await
            .map_err(|mut e| {
                if e.status == 404 {
                    e.message = format!(
                        "No Teams chat with id '{chat_id}'. Copy the id from: chatcli teams chats"
                    );
                }
                e
            })
    }

    /// The newest `count` messages, returned oldest first.
    pub async fn messages(&self, chat_id: &str, count: u32) -> ApiResult<Vec<Message>> {
        let top = count.clamp(1, MAX_PAGE).to_string();
        let page: Page<Message> = self
            .get(url(
                &["chats", chat_id, "messages"],
                &[("$top", top.as_str()), ("$orderby", "createdDateTime desc")],
            )?)
            .await?;
        let mut messages = page.value;
        messages.reverse();
        Ok(messages)
    }

    /// One message, for a change notification that names its chat and message id.
    pub async fn message(&self, chat_id: &str, message_id: &str) -> ApiResult<Message> {
        self.get(url(&["chats", chat_id, "messages", message_id], &[])?)
            .await
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> ApiResult<Message> {
        let body = serde_json::json!({ "body": { "contentType": "text", "content": text } });
        self.send(
            self.http
                .post(url(&["chats", chat_id, "messages"], &[])?)
                .json(&body),
        )
        .await
    }

    /// Marks the chat read for the signed-in user (Graph answers 204 with no body).
    pub async fn mark_read(&self, chat_id: &str, user_id: &str, tenant_id: &str) -> ApiResult<()> {
        let body = serde_json::json!({ "user": { "id": user_id, "tenantId": tenant_id } });
        self.raw(
            self.http
                .post(url(&["chats", chat_id, "markChatReadForUser"], &[])?)
                .json(&body),
        )
        .await
        .map(|_| ())
    }

    pub async fn subscriptions(&self) -> ApiResult<Vec<Subscription>> {
        let page: Page<Subscription> = self.get(url(&["subscriptions"], &[])?).await?;
        Ok(page.value)
    }

    pub async fn create_subscription(&self, body: &serde_json::Value) -> ApiResult<Subscription> {
        self.send(self.http.post(url(&["subscriptions"], &[])?).json(body))
            .await
    }

    pub async fn renew_subscription(&self, id: &str, expiration: &str) -> ApiResult<Subscription> {
        let body = serde_json::json!({ "expirationDateTime": expiration });
        self.send(
            self.http
                .patch(url(&["subscriptions", id], &[])?)
                .json(&body),
        )
        .await
    }

    /// Graph's answer to a `reauthorizationRequired` lifecycle event.
    pub async fn reauthorize_subscription(&self, id: &str) -> ApiResult<()> {
        self.raw(
            self.http
                .post(url(&["subscriptions", id, "reauthorize"], &[])?),
        )
        .await
        .map(|_| ())
    }

    /// A subscription already gone (404) counts as deleted.
    pub async fn delete_subscription(&self, id: &str) -> ApiResult<()> {
        match self
            .raw(self.http.delete(url(&["subscriptions", id], &[])?))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.status == 404 => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(json: serde_json::Value) -> Chat {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn title_prefers_topic_then_other_members() {
        let named =
            chat(serde_json::json!({ "id": "1", "topic": " Floor team ", "chatType": "group" }));
        assert_eq!(named.title("me"), "Floor team");
        let one_on_one = chat(serde_json::json!({
            "id": "2", "topic": null, "chatType": "oneOnOne",
            "members": [
                { "displayName": "Alex Example", "userId": "me" },
                { "displayName": "Sam Sample", "userId": "jane" }
            ]
        }));
        assert_eq!(one_on_one.title("me"), "Sam Sample");
        let bare = chat(serde_json::json!({ "id": "3", "chatType": "oneOnOne" }));
        assert_eq!(bare.title("me"), "(untitled chat)");
    }

    #[test]
    fn unread_compares_last_message_with_read_mark() {
        let unread = chat(serde_json::json!({
            "id": "1", "chatType": "oneOnOne",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:05.123Z" },
            "viewpoint": { "lastMessageReadDateTime": "2026-11-16T01:00:00Z" }
        }));
        assert!(unread.is_unread());
        let read = chat(serde_json::json!({
            "id": "2", "chatType": "oneOnOne",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:00.500Z" },
            "viewpoint": { "lastMessageReadDateTime": "2026-11-16T01:00:01Z" }
        }));
        assert!(!read.is_unread());
        let never_read = chat(serde_json::json!({
            "id": "3", "chatType": "group",
            "lastMessagePreview": { "createdDateTime": "2026-11-16T01:00:00Z" },
            "viewpoint": { "lastMessageReadDateTime": "0001-01-01T00:00:00Z" }
        }));
        assert!(never_read.is_unread());
    }

    #[test]
    fn paging_follows_graph_links_and_reports_the_cap() {
        let link = "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=abc";
        assert_eq!(next_step(1, None, 20), NextStep::Done);
        assert_eq!(
            next_step(1, Some(link), 20),
            NextStep::Fetch(link.to_string())
        );
        assert_eq!(
            next_step(19, Some(link), 20),
            NextStep::Fetch(link.to_string())
        );
        assert_eq!(
            next_step(20, Some(link), 20),
            NextStep::Capped,
            "20 pages fetched, more exist"
        );
        assert_eq!(
            next_step(20, None, 20),
            NextStep::Done,
            "exactly 20 pages is not capped"
        );
    }

    #[test]
    fn paging_never_sends_the_token_to_another_host() {
        assert_eq!(
            next_step(1, Some("https://evil.example/v1.0/me/chats"), 20),
            NextStep::Foreign
        );
        assert_eq!(
            next_step(1, Some("https://graph.microsoft.com.evil.example/x"), 20),
            NextStep::Foreign
        );
        assert_eq!(
            next_step(1, Some("http://graph.microsoft.com/v1.0/x"), 20),
            NextStep::Foreign
        );
    }

    #[test]
    fn page_and_subscription_json_shapes() {
        let page: Page<Subscription> = serde_json::from_value(serde_json::json!({
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/subscriptions?$skip=1",
            "value": [{ "id": "s1", "notificationUrl": "https://r.example/graph/notify",
                        "expirationDateTime": "2026-11-17T00:00:00Z", "resource": "x" }]
        }))
        .unwrap();
        assert_eq!(page.value[0].id, "s1");
        assert!(page.next_link.is_some());
    }

    #[test]
    fn chat_id_cannot_escape_its_path_segment() {
        let u = url(&["chats", "19:abc@thread.v2/../../me", "messages"], &[]).unwrap();
        assert_eq!(
            u.path(),
            "/v1.0/chats/19:abc@thread.v2%2F..%2F..%2Fme/messages"
        );
    }

    #[test]
    fn message_text_handles_deleted_and_system() {
        let m: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "messageType": "message", "deletedDateTime": "2026-11-16T01:00:00Z",
            "body": { "contentType": "text", "content": "secret" }
        }))
        .unwrap();
        assert_eq!(m.text(), "[deleted]");
        let s: Message = serde_json::from_value(serde_json::json!({
            "id": "2", "messageType": "systemEventMessage", "from": null,
            "body": { "contentType": "html", "content": "<systemEventMessage/>" }
        }))
        .unwrap();
        assert_eq!(s.text(), "[system event]");
        assert_eq!(s.sender(), "(system)");
    }
}
