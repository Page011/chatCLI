//! The Graph calls chatcli needs for Teams chats. No timers here: Microsoft allows polling
//! Teams APIs at most once a day, so every call is triggered by the user or by a change
//! notification (relay, later).

use std::time::Duration;

use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use chatcli_core::{chain, Result};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Graph's maximum page size for chats and chat messages.
pub const MAX_PAGE: u32 = 50;
/// A file or picture moving to or from Graph may take a while; the client's 30 s default is
/// for ordinary calls.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);
/// One profile photo request: a dead network must not hold the batch for ten minutes.
const PHOTO_TIMEOUT: Duration = Duration::from_secs(15);
/// Where Teams chat files are uploaded in the user's OneDrive (the folder Teams itself uses).
const CHAT_FILES_FOLDER: &str = "Microsoft Teams Chat Files";

/// Missing or `null` lists come back empty: Graph sends both.
fn null_as_empty<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

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

/// A chat's last message as the chat list shows it (`chatMessageInfo`). It has an id and a
/// deleted flag but no edit time, so an edit shows only in the body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub id: Option<String>,
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
    pub last_edited_date_time: Option<String>,
    pub message_type: Option<String>,
    pub deleted_date_time: Option<String>,
    pub from: Option<From>,
    pub body: Option<Body>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub attachments: Vec<MsgAttachment>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub mentions: Vec<MsgMention>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub reactions: Vec<MsgReaction>,
}

/// `chatMessageAttachment`. `content` is a JSON string for a quoted-message reference.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MsgAttachment {
    pub id: Option<String>,
    pub content_type: Option<String>,
    pub content_url: Option<String>,
    pub content: Option<Value>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MsgMention {
    /// A number in Graph's answers; kept loose so a string cannot fail the whole page.
    pub id: Option<Value>,
    pub mention_text: Option<String>,
    pub mentioned: Option<Mentioned>,
}

#[derive(Debug, Deserialize)]
pub struct Mentioned {
    pub user: Option<Identity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MsgReaction {
    pub reaction_type: Option<String>,
    pub user: Option<ReactionUser>,
}

#[derive(Debug, Deserialize)]
pub struct ReactionUser {
    pub user: Option<Identity>,
}

#[derive(Deserialize)]
struct Page<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// One page of chat messages, newest first as Graph sends them.
#[derive(Debug)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    /// Graph's own link to the next (older) page. Follow it as given; never build one.
    pub next_link: Option<String>,
}

/// One Graph change-notification subscription.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub notification_url: Option<String>,
    pub expiration_date_time: Option<String>,
}

/// The parts of a OneDrive item chatcli needs after an upload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: Option<String>,
    pub name: Option<String>,
    pub e_tag: Option<String>,
    pub web_url: Option<String>,
    pub web_dav_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    upload_url: String,
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

/// `scheme://host/` of a base URL, with the trailing slash.
fn origin_of(base: &str) -> String {
    let after = base.find("://").map_or(0, |i| i + 3);
    let end = base[after..].find('/').map_or(base.len(), |i| after + i);
    format!("{}/", &base[..end])
}

pub fn next_step(pages_fetched: u32, next_link: Option<&str>, max_pages: u32) -> NextStep {
    next_step_in(&origin_of(GRAPH), pages_fetched, next_link, max_pages)
}

fn next_step_in(
    origin: &str,
    pages_fetched: u32,
    next_link: Option<&str>,
    max_pages: u32,
) -> NextStep {
    match next_link {
        None => NextStep::Done,
        Some(link) if !link.starts_with(origin) => NextStep::Foreign,
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

/// A profile photo over the size cap: status 413, so callers can tell it from a network failure.
fn too_big() -> ApiError {
    ApiError {
        status: 413,
        message: "profile photo too big".to_string(),
    }
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

/// The user-facing error for a non-2xx answer. Graph's own code and message are kept; 429, 401
/// and 403 say what to do.
fn api_error(status: u16, retry_after: Option<&str>, body: &str) -> ApiError {
    let detail = serde_json::from_str::<ErrorEnvelope>(body)
        .map(|e| format!("{}: {}", e.error.code, e.error.message))
        .unwrap_or_else(|_| body.chars().take(300).collect());
    let message = match status {
        429 => format!(
            "Teams is throttling requests. Try again in {} s. ({detail})",
            retry_after.unwrap_or("a few")
        ),
        401 => format!("Teams rejected the sign-in. Sign in again. ({detail})"),
        403 => format!("Teams refused access (missing permission or consent). ({detail})"),
        _ => format!("Graph HTTP {status}: {detail}"),
    };
    ApiError { status, message }
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

/// An answer whose body is still to be read, with the two headers callers need up front.
pub struct Download {
    response: reqwest::Response,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
}

impl Download {
    /// Copies the body to `out` chunk by chunk (constant memory). A write error (for example
    /// the cache's size limit) stops the copy and its text is the error.
    pub async fn copy_to(mut self, out: &mut impl std::io::Write) -> Result<u64> {
        let mut total = 0u64;
        loop {
            match self.response.chunk().await {
                Ok(Some(bytes)) => {
                    out.write_all(&bytes).map_err(|e| e.to_string())?;
                    total += bytes.len() as u64;
                }
                Ok(None) => return Ok(total),
                Err(e) => return Err(format!("Graph: {}", chain(&e))),
            }
        }
    }
}

/// A `Write` that errors once more than `max` bytes arrive.
struct CappedVec {
    bytes: Vec<u8>,
    max: u64,
}

impl std::io::Write for CappedVec {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() as u64 + buf.len() as u64 > self.max {
            return Err(std::io::Error::other("profile photo too big"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct Graph {
    http: reqwest::Client,
    token: String,
    /// Tests point this at a local mock server. It does not exist in a release build, and no
    /// environment variable or setting can change where chatcli sends the bearer token.
    #[cfg(test)]
    base: String,
}

fn url_in(base: &str, segments: &[&str], query: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).map_err(|e| e.to_string())?;
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

#[cfg(test)]
fn url(segments: &[&str], query: &[(&str, &str)]) -> Result<reqwest::Url> {
    url_in(GRAPH, segments, query)
}

impl Graph {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self {
            http,
            token,
            #[cfg(test)]
            base: GRAPH.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base(mut self, base: &str) -> Self {
        self.base = base.to_string();
        self
    }

    fn base(&self) -> &str {
        #[cfg(test)]
        {
            &self.base
        }
        #[cfg(not(test))]
        {
            GRAPH
        }
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<reqwest::Url> {
        url_in(self.base(), segments, query)
    }

    /// A link Graph gave us is followed only when it stays on Graph's own host.
    fn origin(&self) -> String {
        origin_of(self.base())
    }

    /// Sends the request (with the bearer token unless `auth` is false) and returns the
    /// response when it is a success. Any other status becomes an ApiError carrying Graph's
    /// own code and message.
    async fn checked(
        &self,
        request: reqwest::RequestBuilder,
        auth: bool,
    ) -> ApiResult<reqwest::Response> {
        let request = if auth {
            request.bearer_auth(&self.token)
        } else {
            request
        };
        let response = request.send().await.map_err(|e| ApiError {
            status: 0,
            message: format!("Graph: {}", chain(&e)),
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.text().await.unwrap_or_default();
        Err(api_error(status.as_u16(), retry_after.as_deref(), &body))
    }

    /// The response body text of a successful call.
    async fn raw(&self, request: reqwest::RequestBuilder) -> ApiResult<String> {
        self.checked(request, true)
            .await?
            .text()
            .await
            .map_err(|e| ApiError {
                status: 0,
                message: format!("Graph: {}", chain(&e)),
            })
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

    /// POST with a JSON body and no answer body (Graph says 204).
    async fn post_empty(&self, url: reqwest::Url, body: &Value) -> ApiResult<()> {
        self.raw(self.http.post(url).json(body)).await.map(|_| ())
    }

    pub async fn me(&self) -> ApiResult<Me> {
        self.get(self.url(&["me"], &[])?).await
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
        let both = self.url(&["me", "chats"], &query("members,lastMessagePreview"))?;
        match self.get::<Page<Chat>>(both).await {
            Ok(page) => Ok(page),
            // Graph documents members and lastMessagePreview as expandable but not whether
            // together. If refused, 1:1 chats go without member names rather than failing.
            Err(e) if e.status == 400 => {
                let preview_only = self.url(&["me", "chats"], &query("lastMessagePreview"))?;
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
            match next_step_in(
                &self.origin(),
                fetched,
                page.next_link.as_deref(),
                max_pages,
            ) {
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
        self.get(self.url(&["chats", chat_id], &[("$expand", "members")])?)
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
            .get(self.url(
                &["chats", chat_id, "messages"],
                &[("$top", top.as_str()), ("$orderby", "createdDateTime desc")],
            )?)
            .await?;
        let mut messages = page.value;
        messages.reverse();
        Ok(messages)
    }

    /// One page of messages older than `before_ms`, newest first, or the page behind `next`
    /// (a link Graph gave earlier, followed as given). The filter needs the `$orderby` on the
    /// same property. A link to any other host is refused: the token must not follow it.
    pub async fn messages_before(
        &self,
        chat_id: &str,
        before_ms: i64,
        next: Option<&str>,
    ) -> ApiResult<MessagePage> {
        let url = match next {
            Some(link) => {
                if !link.starts_with(&self.origin()) {
                    return Err(
                        "Graph sent a next-page link to another host, so older messages stop here."
                            .to_string()
                            .into(),
                    );
                }
                reqwest::Url::parse(link).map_err(|e| e.to_string())?
            }
            None => {
                let iso = chrono::DateTime::from_timestamp_millis(before_ms)
                    .ok_or_else(|| "That message time is out of range.".to_string())?
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let filter = format!("createdDateTime lt {iso}");
                let top = MAX_PAGE.to_string();
                self.url(
                    &["chats", chat_id, "messages"],
                    &[
                        ("$top", top.as_str()),
                        ("$orderby", "createdDateTime desc"),
                        ("$filter", filter.as_str()),
                    ],
                )?
            }
        };
        let page: Page<Message> = self.get(url).await?;
        Ok(MessagePage {
            messages: page.value,
            next_link: page.next_link,
        })
    }

    /// One message, for a change notification that names its chat and message id.
    pub async fn message(&self, chat_id: &str, message_id: &str) -> ApiResult<Message> {
        self.get(self.url(&["chats", chat_id, "messages", message_id], &[])?)
            .await
    }

    /// Posts a plain text message (the `teams send` command). Same HTML body as the TUI.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> ApiResult<Message> {
        let body = crate::body::message_json(&crate::body::text_body(text, &[]));
        self.post_message(chat_id, &body).await
    }

    /// `POST /chats/{id}/messages` with a ready chatMessage JSON.
    pub async fn post_message(&self, chat_id: &str, message: &Value) -> ApiResult<Message> {
        self.send(
            self.http
                .post(self.url(&["chats", chat_id, "messages"], &[])?)
                .timeout(TRANSFER_TIMEOUT)
                .json(message),
        )
        .await
    }

    /// `POST /chats/{id}/messages/replyWithQuote` (needs `ChatMessage.Send`).
    pub async fn reply_with_quote(&self, chat_id: &str, request: &Value) -> ApiResult<Message> {
        self.send(
            self.http
                .post(self.url(&["chats", chat_id, "messages", "replyWithQuote"], &[])?)
                .json(request),
        )
        .await
    }

    /// `setReaction` (`set`) or `unsetReaction`; both answer 204.
    pub async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
        set: bool,
    ) -> ApiResult<()> {
        let action = if set { "setReaction" } else { "unsetReaction" };
        let url = self.url(&["chats", chat_id, "messages", message_id, action], &[])?;
        self.post_empty(url, &serde_json::json!({ "reactionType": emoji }))
            .await
    }

    /// `PATCH /chats/{id}/messages/{id}`; answers 204.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        request: &Value,
    ) -> ApiResult<()> {
        let url = self.url(&["chats", chat_id, "messages", message_id], &[])?;
        self.raw(self.http.patch(url).json(request))
            .await
            .map(|_| ())
    }

    /// `POST /users/{user-id}/chats/{chat-id}/messages/{id}/softDelete`; answers 204. The path
    /// carries the user id, unlike every other chat call.
    pub async fn soft_delete(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
    ) -> ApiResult<()> {
        let url = self.url(
            &[
                "users",
                user_id,
                "chats",
                chat_id,
                "messages",
                message_id,
                "softDelete",
            ],
            &[],
        )?;
        self.raw(self.http.post(url)).await.map(|_| ())
    }

    /// Members of a chat (up to `max_pages` pages, links followed as given). This call takes no
    /// OData query parameters, so none are sent.
    pub async fn members(&self, chat_id: &str, max_pages: u32) -> ApiResult<Vec<Member>> {
        let mut page: Page<Member> = self
            .get(self.url(&["chats", chat_id, "members"], &[])?)
            .await?;
        let mut members = Vec::new();
        let mut fetched = 1;
        loop {
            members.append(&mut page.value);
            match next_step_in(
                &self.origin(),
                fetched,
                page.next_link.as_deref(),
                max_pages,
            ) {
                NextStep::Fetch(link) => {
                    let link = reqwest::Url::parse(&link).map_err(|e| e.to_string())?;
                    page = self.get(link).await?;
                    fetched += 1;
                }
                // Done, capped or foreign: what we have is still a usable list.
                _ => return Ok(members),
            }
        }
    }

    /// Marks the chat read for the signed-in user (Graph answers 204 with no body).
    pub async fn mark_read(&self, chat_id: &str, user_id: &str, tenant_id: &str) -> ApiResult<()> {
        self.chat_user_action(chat_id, "markChatReadForUser", user_id, tenant_id)
            .await
    }

    /// `hideForUser` / `unhideForUser`.
    pub async fn set_hidden(
        &self,
        chat_id: &str,
        hidden: bool,
        user_id: &str,
        tenant_id: &str,
    ) -> ApiResult<()> {
        let action = if hidden {
            "hideForUser"
        } else {
            "unhideForUser"
        };
        self.chat_user_action(chat_id, action, user_id, tenant_id)
            .await
    }

    async fn chat_user_action(
        &self,
        chat_id: &str,
        action: &str,
        user_id: &str,
        tenant_id: &str,
    ) -> ApiResult<()> {
        let body = serde_json::json!({ "user": { "id": user_id, "tenantId": tenant_id } });
        self.post_empty(self.url(&["chats", chat_id, action], &[])?, &body)
            .await
    }

    // --- media -----------------------------------------------------------------------------

    async fn download(&self, url: reqwest::Url) -> ApiResult<Download> {
        self.download_within(url, TRANSFER_TIMEOUT).await
    }

    async fn download_within(&self, url: reqwest::Url, limit: Duration) -> ApiResult<Download> {
        let response = self
            .checked(self.http.get(url).timeout(limit), true)
            .await?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(Download {
            content_length: response.content_length(),
            content_type,
            response,
        })
    }

    /// An inline picture: `GET .../hostedContents/{id}/$value`.
    pub async fn hosted_content(
        &self,
        chat_id: &str,
        message_id: &str,
        hosted_id: &str,
    ) -> ApiResult<Download> {
        self.download(self.url(
            &[
                "chats",
                chat_id,
                "messages",
                message_id,
                "hostedContents",
                hosted_id,
                "$value",
            ],
            &[],
        )?)
        .await
    }

    /// A file linked from a message (OneDrive or SharePoint): `GET /shares/{share-id}/driveItem/
    /// content` with `u!` plus the base64url of the link (needs `Files.Read.All`). Graph answers
    /// with a redirect to a pre-signed address; reqwest drops the token when that is another
    /// host.
    pub async fn shared_file(&self, content_url: &str) -> ApiResult<Download> {
        let share = share_id(content_url);
        self.download(self.url(&["shares", &share, "driveItem", "content"], &[])?)
            .await
    }

    /// A person's profile photo: `GET /users/{id}/photos/48x48/$value`, and on 404
    /// `/users/{id}/photo/$value` (a photo kept only in Entra has no fixed sizes). Ok(None) =
    /// 404 on both (no photo) or 403 (no consent). The body is read with a 1 MB cap: over it is
    /// an error.
    pub async fn user_photo(&self, user_id: &str) -> ApiResult<Option<Vec<u8>>> {
        let sized = self.url(&["users", user_id, "photos", "48x48", "$value"], &[])?;
        let plain = self.url(&["users", user_id, "photo", "$value"], &[])?;
        let mut dl = None;
        for url in [sized, plain] {
            match self.download_within(url, PHOTO_TIMEOUT).await {
                Ok(d) => {
                    dl = Some(d);
                    break;
                }
                Err(e) if e.status == 404 => {}
                Err(e) if e.status == 403 => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        let Some(dl) = dl else { return Ok(None) };
        let max = chatcli_core::media::AVATAR_MAX_BYTES;
        if dl.content_length.is_some_and(|n| n > max) {
            return Err(too_big());
        }
        let mut out = CappedVec {
            bytes: Vec::new(),
            max,
        };
        dl.copy_to(&mut out).await.map_err(|e| {
            if e.contains("profile photo too big") {
                too_big()
            } else {
                e.into()
            }
        })?;
        Ok(Some(out.bytes))
    }

    // --- sending files ---------------------------------------------------------------------

    fn chat_files_segments<'a>(name_with_colon: &'a str, last: &'a str) -> [&'a str; 6] {
        [
            "me",
            "drive",
            "root:",
            CHAT_FILES_FOLDER,
            name_with_colon,
            last,
        ]
    }

    /// `PUT /me/drive/root:/Microsoft Teams Chat Files/{name}:/content` for a small file. An
    /// existing file of that name is never replaced: OneDrive renames the new one.
    pub async fn upload_small(&self, name: &str, bytes: Vec<u8>) -> ApiResult<DriveItem> {
        let tail = format!("{name}:");
        let url = self.url(
            &Self::chat_files_segments(&tail, "content"),
            &[("@microsoft.graph.conflictBehavior", "rename")],
        )?;
        self.send(
            self.http
                .put(url)
                .timeout(TRANSFER_TIMEOUT)
                .header("Content-Type", "application/octet-stream")
                .body(bytes),
        )
        .await
    }

    /// Starts an upload session for a larger file; the returned address takes the chunks.
    pub async fn create_upload_session(&self, name: &str) -> ApiResult<String> {
        let tail = format!("{name}:");
        let url = self.url(
            &Self::chat_files_segments(&tail, "createUploadSession"),
            &[],
        )?;
        let body = serde_json::json!({ "item": { "@microsoft.graph.conflictBehavior": "rename" } });
        let session: UploadSession = self.send(self.http.post(url).json(&body)).await?;
        // Tests alone may use the local mock server's plain http address.
        let local_mock = cfg!(test) && session.upload_url.starts_with("http://127.0.0.1");
        if !session.upload_url.starts_with("https://") && !local_mock {
            return Err(
                "OneDrive gave an upload address that is not https, so nothing was sent."
                    .to_string()
                    .into(),
            );
        }
        Ok(session.upload_url)
    }

    /// One chunk of an upload session. The address is pre-authorized: the bearer token is NOT
    /// sent to it. `Some(item)` when the upload is complete, `None` while it expects more.
    pub async fn upload_chunk(
        &self,
        upload_url: &str,
        start: u64,
        total: u64,
        bytes: Vec<u8>,
    ) -> ApiResult<Option<DriveItem>> {
        let end = start + bytes.len() as u64 - 1;
        let response = self
            .checked(
                self.http
                    .put(upload_url)
                    .timeout(TRANSFER_TIMEOUT)
                    .header("Content-Range", format!("bytes {start}-{end}/{total}"))
                    .body(bytes),
                false,
            )
            .await?;
        let finished = response.status().as_u16() != 202;
        if !finished {
            return Ok(None);
        }
        let item = response.json::<DriveItem>().await.map_err(|e| ApiError {
            status: 200,
            message: format!(
                "OneDrive: unexpected answer to the last chunk: {}",
                chain(&e)
            ),
        })?;
        Ok(Some(item))
    }

    /// A OneDrive item with the fields a message attachment needs (`webDavUrl` is only returned
    /// when selected).
    pub async fn drive_item(&self, item_id: &str) -> ApiResult<DriveItem> {
        self.get(self.url(
            &["me", "drive", "items", item_id],
            &[("$select", "id,name,eTag,webUrl,webDavUrl")],
        )?)
        .await
    }

    // --- subscriptions ---------------------------------------------------------------------

    pub async fn subscriptions(&self) -> ApiResult<Vec<Subscription>> {
        let page: Page<Subscription> = self.get(self.url(&["subscriptions"], &[])?).await?;
        Ok(page.value)
    }

    pub async fn create_subscription(&self, body: &Value) -> ApiResult<Subscription> {
        self.send(
            self.http
                .post(self.url(&["subscriptions"], &[])?)
                .json(body),
        )
        .await
    }

    pub async fn renew_subscription(&self, id: &str, expiration: &str) -> ApiResult<Subscription> {
        let body = serde_json::json!({ "expirationDateTime": expiration });
        self.send(
            self.http
                .patch(self.url(&["subscriptions", id], &[])?)
                .json(&body),
        )
        .await
    }

    /// A subscription already gone (404) counts as deleted.
    pub async fn delete_subscription(&self, id: &str) -> ApiResult<()> {
        match self
            .raw(self.http.delete(self.url(&["subscriptions", id], &[])?))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.status == 404 => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// `u!` plus the base64url (no padding) of the link: Graph's share id for a URL.
pub fn share_id(content_url: &str) -> String {
    format!(
        "u!{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(content_url.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Mock, Reply};
    use serde_json::json;

    fn chat(json: serde_json::Value) -> Chat {
        serde_json::from_value(json).unwrap()
    }

    fn graph_for(mock: &Mock) -> Graph {
        Graph::new(reqwest::Client::new(), "tok".into()).with_base(&mock.graph_base())
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
    fn origin_is_scheme_and_host() {
        assert_eq!(origin_of(GRAPH), "https://graph.microsoft.com/");
        assert_eq!(origin_of("http://127.0.0.1:9/v1.0"), "http://127.0.0.1:9/");
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

    #[test]
    fn message_json_with_null_lists_still_parses() {
        let m: Message = serde_json::from_value(json!({
            "id": "1", "attachments": null, "mentions": null, "reactions": null
        }))
        .unwrap();
        assert!(m.attachments.is_empty() && m.mentions.is_empty() && m.reactions.is_empty());
        let m: Message = serde_json::from_value(json!({ "id": "1" })).unwrap();
        assert!(m.reactions.is_empty());
    }

    /// A release build cannot be told to send the bearer token anywhere but Graph: the only
    /// override is a `#[cfg(test)]` field, and no environment variable picks an address.
    #[test]
    fn a_release_build_has_no_way_to_redirect_graph() {
        let env_read = ["env", "var"].join("::");
        for (name, src) in [
            ("graph", include_str!("graph.rs")),
            ("auth", include_str!("auth.rs")),
            ("transfer", include_str!("transfer.rs")),
            ("backend", include_str!("backend.rs")),
            ("relay", include_str!("relay.rs")),
            ("subscription", include_str!("subscription.rs")),
        ] {
            assert!(
                !src.contains(&env_read),
                "{name} reads an environment variable"
            );
        }
        let src = include_str!("graph.rs");
        for needle in [
            "base: String,",
            "pub fn with_base",
            "base: GRAPH.to_string(),",
        ] {
            let line = src
                .lines()
                .position(|l| l.contains(needle))
                .expect("the override is where this test expects it");
            let before = src.lines().nth(line - 1).unwrap().trim();
            assert_eq!(
                before, "#[cfg(test)]",
                "`{needle}` must exist only in test builds"
            );
        }
        // The address a release build uses is a constant.
        assert_eq!(GRAPH, "https://graph.microsoft.com/v1.0");
    }

    #[test]
    fn share_ids_are_u_bang_plus_base64url_without_padding() {
        // The example from the shares-get documentation.
        assert_eq!(
            share_id("https://onedrive.live.com/redir?resid=1231244193912!12&authKey=1201919!12921!1"),
            "u!aHR0cHM6Ly9vbmVkcml2ZS5saXZlLmNvbS9yZWRpcj9yZXNpZD0xMjMxMjQ0MTkzOTEyITEyJmF1dGhLZXk9MTIwMTkxOSExMjkyMSEx"
        );
    }

    // --- request shapes against the mock server ---------------------------------------------

    #[tokio::test]
    async fn reply_with_quote_posts_to_the_documented_path() {
        let mock = Mock::start(|_| {
            Reply::json(
                201,
                json!({ "id": "1729091934196", "createdDateTime": "2024-10-16T15:18:54.196Z" }),
            )
        });
        let g = graph_for(&mock);
        let req = crate::body::reply_json(
            "1728088338580",
            crate::body::message_json(&crate::body::text_body("Hello World", &[])),
        );
        let sent = g.reply_with_quote("19:abc@thread.v2", &req).await.unwrap();
        assert_eq!(sent.id, "1729091934196");
        let r = &mock.requests()[0];
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.path(),
            "/v1.0/chats/19:abc@thread.v2/messages/replyWithQuote"
        );
        assert_eq!(r.header("authorization"), Some("Bearer tok"));
        assert_eq!(r.json()["messageIds"][0], "1728088338580");
        assert_eq!(r.json()["replyMessage"]["body"]["contentType"], "html");
    }

    #[tokio::test]
    async fn reactions_post_set_and_unset_with_a_reaction_type() {
        let mock = Mock::start(|_| Reply::empty(204));
        let g = graph_for(&mock);
        g.react("19:a", "m1", "thumb", true).await.unwrap();
        g.react("19:a", "m1", "thumb", false).await.unwrap();
        let reqs = mock.requests();
        assert_eq!(reqs[0].path(), "/v1.0/chats/19:a/messages/m1/setReaction");
        assert_eq!(reqs[1].path(), "/v1.0/chats/19:a/messages/m1/unsetReaction");
        assert_eq!(reqs[0].json(), json!({ "reactionType": "thumb" }));
    }

    #[tokio::test]
    async fn edit_patches_and_delete_uses_the_user_path() {
        let mock = Mock::start(|_| Reply::empty(204));
        let g = graph_for(&mock);
        g.edit_message(
            "19:a",
            "m1",
            &crate::body::edit_json(&crate::body::text_body("new", &[])),
        )
        .await
        .unwrap();
        g.soft_delete("u1", "19:a", "m1").await.unwrap();
        let reqs = mock.requests();
        assert_eq!(
            (reqs[0].method.as_str(), reqs[0].path()),
            ("PATCH", "/v1.0/chats/19:a/messages/m1")
        );
        assert_eq!(reqs[0].json()["body"]["content"], "new");
        assert_eq!(
            (reqs[1].method.as_str(), reqs[1].path()),
            ("POST", "/v1.0/users/u1/chats/19:a/messages/m1/softDelete")
        );
    }

    #[tokio::test]
    async fn older_messages_filter_on_created_time_with_a_matching_orderby() {
        let mock = Mock::start(|_| Reply::json(200, json!({ "value": [] })));
        let g = graph_for(&mock);
        let ms = chatcli_core::model::parse_iso_ms("2026-11-16T01:00:05.250Z").unwrap();
        let page = g.messages_before("19:a", ms, None).await.unwrap();
        assert!(page.messages.is_empty() && page.next_link.is_none());
        let r = &mock.requests()[0];
        assert_eq!(r.param("$top").as_deref(), Some("50"));
        assert_eq!(r.param("$orderby").as_deref(), Some("createdDateTime desc"));
        assert_eq!(
            r.param("$filter").as_deref(),
            Some("createdDateTime lt 2026-11-16T01:00:05.250Z")
        );
    }

    #[tokio::test]
    async fn a_next_link_is_followed_as_given_and_a_foreign_one_never() {
        let mock = Mock::start(|r| {
            if r.target.contains("skiptoken=abc") {
                Reply::json(200, json!({ "value": [{ "id": "old" }] }))
            } else {
                Reply::json(500, json!({}))
            }
        });
        let g = graph_for(&mock);
        let link = format!("{}/v1.0/chats/19:a/messages?$skiptoken=abc", mock.origin());
        let page = g.messages_before("19:a", 0, Some(&link)).await.unwrap();
        assert_eq!(page.messages[0].id, "old");
        let before = mock.count();
        let err = g
            .messages_before("19:a", 0, Some("https://evil.example/x"))
            .await
            .unwrap_err();
        assert!(err.message.contains("another host"), "{}", err.message);
        assert_eq!(mock.count(), before, "no request left the machine");
    }

    #[tokio::test]
    async fn members_follow_the_next_link() {
        let mock = Mock::start(|r| {
            if r.target.contains("page=2") {
                Reply::json(
                    200,
                    json!({ "value": [{ "userId": "u2", "displayName": "Sam Sample" }] }),
                )
            } else {
                // The link is absolute and points back at this server.
                Reply::json(
                    200,
                    json!({
                        "value": [{ "userId": "u1", "displayName": "Alex Example" }],
                        "@odata.nextLink": format!("http://{}/v1.0/chats/19:a/members?page=2",
                            r.header("host").unwrap_or("")),
                    }),
                )
            }
        });
        let g = graph_for(&mock);
        let members = g.members("19:a", 3).await.unwrap();
        let ids: Vec<_> = members
            .iter()
            .filter_map(|m| m.user_id.as_deref())
            .collect();
        assert_eq!(ids, ["u1", "u2"]);
        // The members call takes no OData query parameters (Microsoft Learn: chat-list-members).
        assert_eq!(mock.requests()[0].query(), "");
    }

    #[tokio::test]
    async fn hidden_and_read_actions_name_the_user_and_tenant() {
        let mock = Mock::start(|_| Reply::empty(204));
        let g = graph_for(&mock);
        g.set_hidden("19:a", true, "u1", "t1").await.unwrap();
        g.set_hidden("19:a", false, "u1", "t1").await.unwrap();
        let reqs = mock.requests();
        assert_eq!(reqs[0].path(), "/v1.0/chats/19:a/hideForUser");
        assert_eq!(reqs[1].path(), "/v1.0/chats/19:a/unhideForUser");
        assert_eq!(
            reqs[0].json(),
            json!({ "user": { "id": "u1", "tenantId": "t1" } })
        );
    }

    #[tokio::test]
    async fn graph_errors_carry_status_code_and_message() {
        let mock = Mock::start(|_| Reply::error(403, "Forbidden", "Missing scope permissions"));
        let g = graph_for(&mock);
        let e = g.react("19:a", "m", "x", true).await.unwrap_err();
        assert_eq!(e.status, 403);
        assert!(
            e.message.contains("Forbidden: Missing scope permissions"),
            "{}",
            e.message
        );
        let throttled = Mock::start(|_| {
            let mut r = Reply::error(429, "TooManyRequests", "slow down");
            r.headers.push(("Retry-After".into(), "17".into()));
            r
        });
        let e = graph_for(&throttled).me().await.unwrap_err();
        assert!(e.message.contains("Try again in 17 s"), "{}", e.message);
    }

    #[tokio::test]
    async fn shared_file_download_drops_the_token_on_the_redirect_host() {
        // Two servers on different ports are two origins, like Graph and SharePoint.
        let cdn = Mock::start(|_| Reply::bytes(200, "application/pdf", b"%PDF-1".to_vec()));
        let target = format!("{}/download/file.pdf?sig=abc", cdn.origin());
        let graph = Mock::start(move |_| Reply::redirect(&target));
        let g = graph_for(&graph);
        let dl = g
            .shared_file("https://contoso.example.invalid/personal/alex/Documents/a.pdf")
            .await
            .unwrap();
        assert_eq!(dl.content_type.as_deref(), Some("application/pdf"));
        let mut sink = Vec::new();
        assert_eq!(dl.copy_to(&mut sink).await.unwrap(), 6);
        assert_eq!(sink, b"%PDF-1");
        let first = &graph.requests()[0];
        assert!(first.path().starts_with("/v1.0/shares/u!"));
        assert!(first.path().ends_with("/driveItem/content"));
        assert_eq!(first.header("authorization"), Some("Bearer tok"));
        assert_eq!(
            cdn.requests()[0].header("authorization"),
            None,
            "the bearer token must not follow the redirect to another host"
        );
    }

    #[tokio::test]
    async fn user_photo_reads_the_sized_photo_then_the_plain_one() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/jpeg", vec![1, 2, 3]));
        let g = graph_for(&mock);
        assert_eq!(g.user_photo("u1").await.unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(
            mock.requests()[0].path(),
            "/v1.0/users/u1/photos/48x48/$value"
        );
        let mock = Mock::start(|r| {
            if r.path().contains("48x48") {
                Reply::error(404, "ImageNotFound", "none")
            } else {
                Reply::bytes(200, "image/jpeg", vec![9])
            }
        });
        let g = graph_for(&mock);
        assert_eq!(g.user_photo("u1").await.unwrap(), Some(vec![9]));
        assert_eq!(mock.requests()[1].path(), "/v1.0/users/u1/photo/$value");
    }

    #[tokio::test]
    async fn user_photo_answers_none_for_404_twice_and_for_403() {
        let mock = Mock::start(|_| Reply::error(404, "ImageNotFound", "none"));
        assert_eq!(graph_for(&mock).user_photo("u1").await.unwrap(), None);
        assert_eq!(mock.requests().len(), 2);
        let mock = Mock::start(|_| Reply::error(403, "Forbidden", "no scope"));
        assert_eq!(graph_for(&mock).user_photo("u1").await.unwrap(), None);
        assert_eq!(mock.requests().len(), 1);
        let mock = Mock::start(|_| Reply::error(500, "Oops", "x"));
        assert!(graph_for(&mock).user_photo("u1").await.is_err());
    }

    #[tokio::test]
    async fn user_photo_over_one_megabyte_is_an_error() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/jpeg", vec![0; 1024 * 1024 + 1]));
        assert!(graph_for(&mock).user_photo("u1").await.is_err());
    }

    #[tokio::test]
    async fn hosted_content_is_fetched_from_the_documented_path() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/png", vec![1, 2, 3]));
        let g = graph_for(&mock);
        let dl = g.hosted_content("19:a", "m1", "aWQ9Mg").await.unwrap();
        assert_eq!(
            (dl.content_type.as_deref(), dl.content_length),
            (Some("image/png"), Some(3))
        );
        assert_eq!(
            mock.requests()[0].path(),
            "/v1.0/chats/19:a/messages/m1/hostedContents/aWQ9Mg/$value"
        );
    }

    #[tokio::test]
    async fn small_upload_puts_to_the_chat_files_folder_and_never_overwrites() {
        let mock = Mock::start(|_| Reply::json(201, json!({ "id": "i1", "name": "a b.txt" })));
        let g = graph_for(&mock);
        let item = g.upload_small("a b.txt", b"hello".to_vec()).await.unwrap();
        assert_eq!(item.id.as_deref(), Some("i1"));
        let r = &mock.requests()[0];
        assert_eq!(r.method, "PUT");
        assert_eq!(
            r.path(),
            "/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/a%20b.txt:/content"
        );
        assert_eq!(
            r.param("@microsoft.graph.conflictBehavior").as_deref(),
            Some("rename")
        );
        assert_eq!(r.body, b"hello");
    }

    #[tokio::test]
    async fn upload_session_chunks_carry_content_range_and_no_token() {
        let mock = Mock::start(|r| {
            let upload = if r.path().contains("plain.zip") {
                "http://upload.example.invalid/x"
            } else {
                "https://upload.example.invalid/x"
            };
            Reply::json(200, json!({ "uploadUrl": upload }))
        });
        let g = graph_for(&mock);
        // The session address must be https: a plain http address is refused.
        let err = g.create_upload_session("plain.zip").await.unwrap_err();
        assert!(err.message.contains("not https"), "{}", err.message);
        assert_eq!(
            g.create_upload_session("big.zip").await.unwrap(),
            "https://upload.example.invalid/x"
        );
        let reqs = mock.requests()[1..].to_vec();
        assert!(reqs[0]
            .path()
            .ends_with("/root:/Microsoft%20Teams%20Chat%20Files/big.zip:/createUploadSession"));
        assert_eq!(
            reqs[0].json(),
            json!({ "item": { "@microsoft.graph.conflictBehavior": "rename" } })
        );
        // A chunk goes to the given address with its range and without the bearer token.
        let chunk_server = Mock::start(|r| match r.header("content-range") {
            Some("bytes 0-2/5") => Reply::json(202, json!({ "nextExpectedRanges": ["3-4"] })),
            _ => Reply::json(200, json!({ "id": "done" })),
        });
        let url = format!("{}/session", chunk_server.origin());
        assert_eq!(
            g.upload_chunk(&url, 0, 5, vec![1, 2, 3]).await.unwrap(),
            None
        );
        let last = g
            .upload_chunk(&url, 3, 5, vec![4, 5])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.id.as_deref(), Some("done"));
        let sent = chunk_server.requests();
        assert_eq!(sent[1].header("content-range"), Some("bytes 3-4/5"));
        assert!(sent.iter().all(|r| r.header("authorization").is_none()));
    }
}
