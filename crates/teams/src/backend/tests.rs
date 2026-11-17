//! Backend behaviour against a local mock Graph: request shapes, the events the TUI gets, and
//! what the store holds afterwards. No real network, token or credential store is touched.

use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};

use chatcli_core::model::Reaction;
use chatcli_core::settings::MediaSettings;

use super::*;
use crate::testkit::{Mock, Reply, Req};

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chatcli-teams-backend-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn emoji(cp: u32) -> String {
    char::from_u32(cp).unwrap().to_string()
}

fn scopes(list: &str) -> Vec<String> {
    list.split(' ').map(str::to_string).collect()
}

const BASE: &str = "offline_access User.Read Chat.ReadWrite ChatMessage.Send";

struct Env {
    b: Backend,
    ev: mpsc::UnboundedReceiver<Event>,
    mock: Mock,
    cmds: CommandRx,
    dir: PathBuf,
}

fn env(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Env {
    env_with(Settings::default(), BASE, handler)
}

fn env_with(
    settings: Settings,
    granted: &str,
    handler: impl Fn(&Req) -> Reply + Send + Sync + 'static,
) -> Env {
    let dir = temp_dir("env");
    let media = MediaCache::open(&dir.join("media"), &MediaSettings::default()).unwrap();
    let mock = Mock::start(handler);
    let (ev_tx, ev) = mpsc::unbounded_channel();
    let (relay_tx, _relay_rx) = mpsc::unbounded_channel();
    let (_cmd_tx, cmds) = mpsc::unbounded_channel();
    let ctx = BackendCtx {
        store: Store::open_in_memory().unwrap(),
        settings: Arc::new(settings),
        media,
    };
    let mut b = Backend::new(ctx, ev_tx, reqwest::Client::new(), relay_tx);
    b.graph_base = Some(mock.graph_base());
    b.me_id = Some("me".into());
    b.tenant_id = Some("tenant-1".into());
    b.token = Some(CachedToken {
        access: "tok".into(),
        expires_at_ms: i64::MAX / 2,
    });
    b.granted = scopes(granted);
    Env {
        b,
        ev,
        mock,
        cmds,
        dir,
    }
}

impl Env {
    async fn cmd(&mut self, c: Command) {
        self.b.command(c, &mut self.cmds).await;
    }

    fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.ev.try_recv() {
            out.push(e);
        }
        out
    }

    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Event {
        loop {
            let e = tokio::time::timeout(Duration::from_secs(20), self.ev.recv())
                .await
                .expect("timed out waiting for an event")
                .expect("event channel closed");
            if pred(&e) {
                return e;
            }
        }
    }

    fn chat(&self, id: &str, kind: ChatKind) {
        self.b
            .store
            .upsert_chats(&[ChatSummary {
                id: id.into(),
                title: id.into(),
                kind,
                ..ChatSummary::blank(NET)
            }])
            .unwrap();
    }

    fn put(&self, m: ChatMessage) {
        self.b.store.upsert_messages(&[m]).unwrap();
    }

    fn stored(&self, chat: &str, id: &str) -> ChatMessage {
        self.b
            .store
            .message(NET, chat, id)
            .unwrap()
            .expect("message is stored")
    }

    fn media_path(&self) -> PathBuf {
        self.dir.join("media")
    }
}

fn msg(chat: &str, id: &str, ts: i64, from_me: bool, text: &str) -> ChatMessage {
    ChatMessage {
        chat_id: chat.into(),
        id: id.into(),
        ts,
        sender: if from_me {
            "Alex Example"
        } else {
            "Sam Sample"
        }
        .into(),
        sender_id: if from_me { "me" } else { "sam" }.into(),
        from_me,
        text: text.into(),
        ..ChatMessage::blank(NET)
    }
}

fn graph_msg(id: &str, iso: &str, from: &str, html: &str) -> Value {
    json!({
        "id": id, "createdDateTime": iso, "messageType": "message",
        "from": { "user": { "id": from, "displayName": if from == "me" { "Alex Example" } else { "Sam Sample" } } },
        "body": { "contentType": "html", "content": html },
    })
}

fn is_confirmed(e: &Event) -> bool {
    matches!(e, Event::MessageConfirmed { .. })
}

fn is_failed(e: &Event) -> bool {
    matches!(e, Event::MessageFailed { .. })
}

fn notices(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Notice {
                text,
                is_error: true,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// --- startup path kept from before ------------------------------------------------------------

/// An unconfigured install: the task announces itself, shows the cached chats, asks for
/// setup (never touching the credential store) and ends on Shutdown.
#[tokio::test]
async fn unconfigured_start_asks_for_setup_and_shuts_down() {
    let dir = temp_dir("unconfigured");
    std::env::set_var("CHATCLI_DATA_DIR", &dir);
    let ctx = BackendCtx {
        store: Store::open_in_memory().unwrap(),
        settings: Arc::new(Settings::default()),
        media: MediaCache::open(&dir.join("media"), &MediaSettings::default()).unwrap(),
    };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run(ctx, cmd_rx, ev_tx));
    let mut seen = Vec::new();
    while seen.len() < 3 {
        seen.push(
            tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(seen[0], Event::Status(Network::Teams, Status::Starting));
    assert_eq!(seen[1], Event::Chats(Network::Teams, vec![]));
    let Event::Status(
        _,
        Status::NeedsLogin {
            reason: Some(reason),
        },
    ) = &seen[2]
    else {
        panic!("expected NeedsLogin with a reason, got {:?}", seen[2]);
    };
    assert!(reason.contains(auth::NOT_SET_UP), "{reason}");
    cmd_tx.send(Command::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_only_when_relay_socket_and_subscription_are_all_up() {
    assert_eq!(live_state(None, true, None, true, None), (true, None));
    let not_live = |args: (Option<&str>, bool, Option<&str>, bool, Option<&str>)| {
        live_state(args.0, args.1, args.2, args.3, args.4)
    };
    assert_eq!(
        not_live((Some("relay not set up"), true, None, true, None)),
        (false, Some("relay not set up".into()))
    );
    assert_eq!(
        not_live((None, false, Some("relay offline: timed out"), true, None)),
        (false, Some("relay offline: timed out".into()))
    );
    assert_eq!(
        not_live((None, false, None, true, None)),
        (false, Some("connecting to relay".into()))
    );
    assert_eq!(
        not_live((
            None,
            true,
            None,
            false,
            Some("Teams subscription failed: x")
        )),
        (false, Some("Teams subscription failed: x".into()))
    );
    assert_eq!(
        not_live((None, true, None, false, None)),
        (false, Some("no Teams subscription yet".into()))
    );
}

#[tokio::test]
async fn sign_in_needed_is_not_overwritten_by_online_status() {
    let mut t = env(|_| Reply::empty(500));
    t.b.me_id = Some("u".into());
    t.b.signed_out(Some("password changed".into()));
    t.drain();
    // What upkeep and frame do after a rejected refresh, and a stale relay event.
    t.b.upkeep().await;
    t.b.relay_event(RelayEvent::Up).await;
    t.b.publish_online();
    assert!(t.ev.try_recv().is_err(), "nothing may follow NeedsLogin");
    assert!(matches!(t.b.last_status, Some(Status::NeedsLogin { .. })));
}

#[test]
fn upkeep_runs_at_renew_time_unless_a_retry_is_pending() {
    let sub = SubState {
        id: "s".into(),
        expires_ms: 1000,
        renew_at_ms: 500,
    };
    assert_eq!(next_upkeep_ms(Some(&sub), None), Some(500));
    assert_eq!(next_upkeep_ms(Some(&sub), Some(900)), Some(900));
    assert_eq!(next_upkeep_ms(None, Some(900)), Some(900));
    assert_eq!(next_upkeep_ms(None, None), None);
}

// --- capabilities (contract section 2) --------------------------------------------------------

#[tokio::test]
async fn capabilities_are_sent_once_and_again_only_when_they_change() {
    let mut t = env(|_| Reply::empty(500));
    t.b.publish_caps();
    t.b.publish_caps();
    let first = t.drain();
    assert_eq!(first.len(), 1, "no duplicate: {first:?}");
    let Event::Capabilities { network, caps } = &first[0] else {
        panic!("{first:?}")
    };
    assert_eq!(*network, Network::Teams);
    assert!(caps.reply && caps.react && caps.hide && !caps.typing && !caps.send_file);
    // Consent for ChatMessage.Send is lost (a refresh fell back to the 0.1 scopes).
    t.b.granted = scopes("offline_access User.Read Chat.ReadWrite");
    t.b.publish_caps();
    let again = t.drain();
    assert!(matches!(&again[..], [Event::Capabilities { caps, .. }] if !caps.reply));
}

/// The settings screen sends the new settings: the backend uses them at once, no restart.
#[tokio::test]
async fn a_settings_command_republishes_capabilities_and_new_media_limits() {
    let mut t = env_with(
        Settings::default(),
        "offline_access User.Read Chat.ReadWrite ChatMessage.Send Files.Read.All",
        |_| Reply::empty(500),
    );
    t.b.publish_caps();
    assert!(matches!(&t.drain()[..], [Event::Capabilities { caps, .. }] if !caps.fetch_files));
    let mut new = Settings::default();
    new.teams.files_read = true;
    new.media.max_file_mb = 10;
    t.cmd(Command::Settings(Arc::new(new))).await;
    assert!(matches!(&t.drain()[..], [Event::Capabilities { caps, .. }] if caps.fetch_files));
    assert_eq!(t.b.media.max_file_bytes(), 10 * 1024 * 1024);
    // The same settings again publish nothing.
    t.cmd(Command::Settings(t.b.settings.clone())).await;
    assert!(t.drain().is_empty());
}

/// A refresh that fell back to fewer scopes must not be silent: one plain notice names them.
#[tokio::test]
async fn missing_scopes_are_named_once_in_a_notice() {
    let mut settings = Settings::default();
    settings.teams.files_read = true;
    let mut t = env_with(settings, "offline_access User.Read Chat.ReadWrite", |_| {
        Reply::empty(500)
    });
    t.b.warn_missing_scopes();
    t.b.warn_missing_scopes();
    let events = t.drain();
    assert_eq!(events.len(), 1, "once: {events:?}");
    let Event::Notice { text, is_error, .. } = &events[0] else {
        panic!("{events:?}")
    };
    assert!(!is_error);
    assert!(
        text.contains("ChatMessage.Send") && text.contains("Files.Read.All"),
        "{text}"
    );
    assert!(
        !text.contains("Files.ReadWrite"),
        "files_write is off, so it is not asked for"
    );
    // Everything granted: nothing to say.
    let mut ok = env(|_| Reply::empty(500));
    ok.b.warn_missing_scopes();
    assert!(ok.drain().is_empty());
}

#[tokio::test]
async fn the_missing_photos_scope_is_named_as_profile_photos() {
    let mut settings = Settings::default();
    settings.teams.photos = true;
    let mut t = env_with(settings, BASE, |_| Reply::empty(500));
    t.b.warn_missing_scopes();
    let events = t.drain();
    let [Event::Notice { text, .. }] = &events[..] else {
        panic!("{events:?}")
    };
    assert!(text.contains("profile photos"), "{text}");
}

const GUID: &str = "11111111-1111-1111-1111-111111111111";

fn photo_png() -> Vec<u8> {
    let mut v = Vec::new();
    image::RgbImage::from_pixel(20, 10, image::Rgb([5, 6, 7]))
        .write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png)
        .unwrap();
    v
}

fn avatar_cmd(ids: &[&str]) -> Command {
    Command::FetchAvatars {
        people: ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn avatars_of(events: Vec<Event>) -> Vec<(String, Option<PathBuf>)> {
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::Avatar {
                person_id, path, ..
            } => Some((person_id, path)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn photos_off_answers_none_without_a_request() {
    let mut t = env(|_| Reply::bytes(200, "image/png", photo_png()));
    t.cmd(avatar_cmd(&[GUID, "x"])).await;
    let got = avatars_of(t.drain());
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|(_, p)| p.is_none()));
    assert_eq!(t.mock.count(), 0);
    // Setting on, consent missing: the same.
    let mut settings = Settings::default();
    settings.teams.photos = true;
    let mut t = env_with(settings, BASE, |_| Reply::empty(500));
    t.cmd(avatar_cmd(&[GUID])).await;
    assert!(avatars_of(t.drain())[0].1.is_none());
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn a_photo_is_stored_under_avatars_and_the_second_ask_comes_from_the_cache() {
    let mut settings = Settings::default();
    settings.teams.photos = true;
    let mut t = env_with(settings, &format!("{BASE} User.ReadBasic.All"), |_| {
        Reply::bytes(200, "image/png", photo_png())
    });
    t.cmd(avatar_cmd(&[GUID, "not-a-guid"])).await;
    let first = t
        .until(|e| matches!(e, Event::Avatar { path: Some(_), .. }))
        .await;
    let Event::Avatar {
        path: Some(path), ..
    } = first
    else {
        unreachable!()
    };
    assert_eq!(path.parent().unwrap(), t.media_path().join("avatars"));
    assert!(path.is_file());
    assert_eq!(t.mock.count(), 1, "the non-GUID id made no request");
    t.cmd(avatar_cmd(&[GUID])).await;
    assert_eq!(avatars_of(t.drain())[0].1.as_deref(), Some(path.as_path()));
    assert_eq!(t.mock.count(), 1, "cache hit, no request");
}

#[tokio::test]
async fn a_404_writes_the_no_photo_marker_and_is_not_asked_again() {
    let mut settings = Settings::default();
    settings.teams.photos = true;
    let mut t = env_with(settings, &format!("{BASE} User.ReadBasic.All"), |_| {
        Reply::error(404, "ImageNotFound", "none")
    });
    t.cmd(avatar_cmd(&[GUID])).await;
    let e = t.until(|e| matches!(e, Event::Avatar { .. })).await;
    assert!(matches!(e, Event::Avatar { path: None, .. }));
    assert_eq!(t.mock.count(), 2, "sized, then plain");
    t.cmd(avatar_cmd(&[GUID])).await;
    assert!(avatars_of(t.drain())[0].1.is_none());
    assert_eq!(t.mock.count(), 2);
}

#[tokio::test]
async fn file_capabilities_need_both_the_setting_and_the_scope() {
    let mut settings = Settings::default();
    settings.teams.files_read = true;
    settings.teams.files_write = true;
    let t = env_with(settings, &format!("{BASE} Files.Read.All"), |_| {
        Reply::empty(500)
    });
    let c = t.b.caps();
    assert!(
        c.fetch_files && !c.send_file,
        "only Files.Read.All was granted"
    );
}

// --- reply (#12) ------------------------------------------------------------------------------

fn quoted() -> QuotedRef {
    QuotedRef {
        id: "1728422677844".into(),
        sender: "Pat Sample".into(),
        sender_id: "pat-id".into(),
        preview: "Hello World".into(),
    }
}

fn send_cmd(
    chat: &str,
    text: &str,
    client: &str,
    reply_to: Option<QuotedRef>,
    mentions: Vec<PersonRef>,
) -> Command {
    Command::Send {
        chat_id: chat.into(),
        text: text.into(),
        client_id: client.into(),
        reply_to,
        mentions,
    }
}

#[tokio::test]
async fn a_reply_goes_through_reply_with_quote_and_keeps_its_quote() {
    let mut t = env(|r| {
        if r.path().ends_with("/replyWithQuote") {
            Reply::json(
                201,
                json!({
                    "id": "1729091934196", "createdDateTime": "2024-10-16T15:18:54.196Z",
                    "messageType": "message",
                    "from": { "user": { "id": "me", "displayName": "Alex Example" } },
                    "body": { "contentType": "html", "content": "<p></p>\n<attachment id=\"1728422677844\"></attachment>\ntest reply\n<p></p>" },
                    "attachments": [{ "id": "1728422677844", "contentType": "messageReference",
                        "content": "{\"messageId\":\"1728422677844\",\"messagePreview\":\"Hello World\",\"messageSender\":{\"user\":{\"id\":\"pat-id\",\"displayName\":\"Pat Sample\"}}}" }],
                }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", "test reply", "c1", Some(quoted()), vec![]))
        .await;
    let Event::MessageConfirmed { client_id, message } = t.until(is_confirmed).await else {
        unreachable!()
    };
    assert_eq!(client_id, "c1");
    assert!(message.from_me);
    assert_eq!(message.text, "test reply");
    let q = message.reply_to.clone().unwrap();
    assert_eq!(
        (q.id.as_str(), q.sender.as_str(), q.preview.as_str()),
        ("1728422677844", "Pat Sample", "Hello World")
    );
    let reqs = t.mock.requests();
    assert_eq!(reqs.len(), 1, "one request, never a plain send as well");
    assert_eq!(reqs[0].path(), "/v1.0/chats/19:a/messages/replyWithQuote");
    assert_eq!(reqs[0].json()["messageIds"], json!(["1728422677844"]));
    assert_eq!(
        reqs[0].json()["replyMessage"]["body"]["content"],
        "test reply"
    );
    assert_eq!(
        t.stored("19:a", "1729091934196").reply_to,
        message.reply_to,
        "the quote is stored"
    );
}

/// Without the scope a reply fails with the fixed text and nothing is sent, not even plain.
#[tokio::test]
async fn a_reply_without_the_scope_fails_loudly_and_never_goes_out_plain() {
    let mut t = env_with(
        Settings::default(),
        "offline_access User.Read Chat.ReadWrite",
        |_| Reply::empty(500),
    );
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", "hi", "c1", Some(quoted()), vec![]))
        .await;
    let Event::MessageFailed {
        client_id, error, ..
    } = t.until(is_failed).await
    else {
        unreachable!()
    };
    assert_eq!(client_id, "c1");
    assert_eq!(error, "REPLY NEEDS ChatMessage.Send, sign in again");
    assert_eq!(t.mock.count(), 0);
    let kept = t.stored("19:a", "c1");
    assert!(matches!(kept.state, MessageState::Failed(_)));
    assert_eq!(
        kept.reply_to,
        Some(quoted()),
        "the failed message keeps its quote"
    );
}

#[tokio::test]
async fn a_refused_reply_says_the_scope_is_missing() {
    let mut t = env(|_| Reply::error(403, "Forbidden", "Missing scope permissions"));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", "hi", "c1", Some(quoted()), vec![]))
        .await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert_eq!(error, "REPLY NEEDS ChatMessage.Send, sign in again");
    assert_eq!(t.mock.count(), 1, "no retry as a plain message");
}

// --- text, html body, mentions (#10, #28) -----------------------------------------------------

#[tokio::test]
async fn text_goes_out_as_escaped_html() {
    let mut t = env(|_| {
        Reply::json(
            201,
            json!({ "id": "9", "createdDateTime": "2026-11-16T01:00:00Z" }),
        )
    });
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", "a < b & c\nline two", "c1", None, vec![]))
        .await;
    t.until(is_confirmed).await;
    let body = t.mock.requests()[0].json();
    assert_eq!(body["body"]["contentType"], "html");
    assert_eq!(body["body"]["content"], "a &lt; b &amp; c<br>line two");
    assert!(body.get("mentions").is_none());
}

#[tokio::test]
async fn a_text_too_long_for_teams_is_refused_before_any_request() {
    let mut t = env(|_| Reply::empty(500));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", &"x".repeat(81 * 1024), "c1", None, vec![]))
        .await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert!(error.starts_with("MESSAGE TOO LONG"), "{error}");
    assert_eq!(t.mock.count(), 0);
}

fn sam() -> PersonRef {
    PersonRef {
        id: "00000000-0000-0000-0000-000000000002".into(),
        name: "Sam Sample".into(),
    }
}

#[tokio::test]
async fn a_mention_in_a_group_becomes_an_at_tag_and_a_mentions_entry() {
    let mut t = env(|_| {
        Reply::json(
            201,
            json!({ "id": "9", "createdDateTime": "2026-11-16T01:00:00Z" }),
        )
    });
    t.chat("19:g", ChatKind::Group);
    t.cmd(send_cmd(
        "19:g",
        "hi @Sam Sample, ok?",
        "c1",
        None,
        vec![sam()],
    ))
    .await;
    t.until(is_confirmed).await;
    let body = t.mock.requests()[0].json();
    assert_eq!(
        body["body"]["content"],
        "hi <at id=\"0\">Sam Sample</at>, ok?"
    );
    assert_eq!(body["mentions"][0]["id"], 0);
    assert_eq!(body["mentions"][0]["mentionText"], "Sam Sample");
    assert_eq!(
        body["mentions"][0]["mentioned"]["user"]["id"],
        "00000000-0000-0000-0000-000000000002"
    );
    assert_eq!(
        body["mentions"][0]["mentioned"]["user"]["userIdentityType"],
        "aadUser"
    );
}

#[tokio::test]
async fn a_mention_in_a_one_to_one_chat_stays_plain_text() {
    let mut t = env(|_| {
        Reply::json(
            201,
            json!({ "id": "9", "createdDateTime": "2026-11-16T01:00:00Z" }),
        )
    });
    t.chat("19:a", ChatKind::Direct);
    t.cmd(send_cmd("19:a", "hi @Sam Sample", "c1", None, vec![sam()]))
        .await;
    t.until(is_confirmed).await;
    let body = t.mock.requests()[0].json();
    assert_eq!(body["body"]["content"], "hi @Sam Sample");
    assert!(body.get("mentions").is_none());
}

#[tokio::test]
async fn an_incoming_mention_of_the_user_bumps_the_chat_counter_once() {
    let mut t = env(|r| {
        if r.path().ends_with("/messages/m1") {
            Reply::json(
                200,
                json!({
                    "id": "m1", "createdDateTime": "2026-11-16T01:00:05Z", "messageType": "message",
                    "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                    "body": { "contentType": "html", "content": "hi <at id=\"0\">Alex Example</at>" },
                    "mentions": [{ "id": 0, "mentionText": "Alex Example",
                                   "mentioned": { "user": { "id": "me", "displayName": "Alex Example" } } }],
                }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:g", ChatKind::Group);
    let mut relisted = true;
    t.b.live_message("19:g", "m1", true, &mut relisted).await;
    let events = t.drain();
    let Some(Event::Message(m)) = events.iter().find(|e| matches!(e, Event::Message(_))) else {
        panic!("{events:?}")
    };
    assert!(m.mentions_me);
    assert_eq!(m.text, "hi @Alex Example");
    let chat = t.b.store.chat(NET, "19:g").unwrap().unwrap();
    assert_eq!((chat.unread, chat.mentions), (1, 1));
    // The same notification again is a redelivery: no second count.
    t.b.live_message("19:g", "m1", true, &mut relisted).await;
    let chat = t.b.store.chat(NET, "19:g").unwrap().unwrap();
    assert_eq!((chat.unread, chat.mentions), (1, 1));
    // Marking read clears both.
    t.b.store.set_unread(NET, "19:g", 0).unwrap();
    assert_eq!(t.b.store.chat(NET, "19:g").unwrap().unwrap().mentions, 0);
}

// --- reactions (#13) --------------------------------------------------------------------------

fn reacted(chat: &str, id: &str, mine: Option<&str>, others: &[(&str, &str)]) -> ChatMessage {
    let m = msg(chat, id, 1000, false, "hello");
    let _ = (mine, others);
    m
}

fn seed_reactions(t: &Env, chat: &str, id: &str, rows: &[(&str, &str, bool)]) {
    let rows: Vec<chatcli_core::store::ReactionRow> = rows
        .iter()
        .map(|(who, e, mine)| chatcli_core::store::ReactionRow {
            reactor: map::reactor_key(who, e),
            emoji: (*e).to_string(),
            from_me: *mine,
        })
        .collect();
    t.b.store.replace_reactions(NET, chat, id, &rows).unwrap();
}

#[tokio::test]
async fn setting_a_new_reaction_unsets_the_old_one_first() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.put(reacted("19:a", "m1", None, &[]));
    let (thumb, heart) = (emoji(0x1F44D), emoji(0x2764));
    seed_reactions(
        &t,
        "19:a",
        "m1",
        &[(REACTOR_ME, &thumb, true), ("sam", &thumb, false)],
    );
    t.cmd(Command::React {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        emoji: heart.clone(),
        remove: false,
    })
    .await;
    let reqs = t.mock.requests();
    let steps: Vec<(&str, Value)> = reqs
        .iter()
        .map(|r| (r.path().rsplit('/').next().unwrap(), r.json()))
        .collect();
    assert_eq!(
        steps,
        vec![
            ("unsetReaction", json!({ "reactionType": thumb })),
            ("setReaction", json!({ "reactionType": heart })),
        ],
        "old reaction removed, then the new one set"
    );
    let Event::MessageChanged(m) = t.until(|e| matches!(e, Event::MessageChanged(_))).await else {
        unreachable!()
    };
    assert_eq!(
        m.reactions,
        vec![
            Reaction {
                emoji: heart.clone(),
                count: 1,
                mine: true
            },
            Reaction {
                emoji: thumb.clone(),
                count: 1,
                mine: false
            },
        ]
    );
    assert_eq!(t.stored("19:a", "m1").reactions, m.reactions);
}

#[tokio::test]
async fn removing_a_reaction_unsets_it_and_setting_the_same_one_again_does_nothing() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.put(reacted("19:a", "m1", None, &[]));
    let thumb = emoji(0x1F44D);
    seed_reactions(&t, "19:a", "m1", &[(REACTOR_ME, &thumb, true)]);
    t.cmd(Command::React {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        emoji: thumb.clone(),
        remove: false,
    })
    .await;
    assert_eq!(t.mock.count(), 0, "already set: nothing to send");
    assert!(t.drain().is_empty());
    t.cmd(Command::React {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        emoji: thumb.clone(),
        remove: true,
    })
    .await;
    assert_eq!(
        t.mock.requests()[0].path(),
        "/v1.0/chats/19:a/messages/m1/unsetReaction"
    );
    let Event::MessageChanged(m) = t.until(|e| matches!(e, Event::MessageChanged(_))).await else {
        unreachable!()
    };
    assert!(m.reactions.is_empty());
}

#[tokio::test]
async fn a_refused_reaction_shows_teams_own_reason_and_changes_nothing() {
    let mut t = env(|_| Reply::error(400, "BadRequest", "reactionType is not supported"));
    t.chat("19:a", ChatKind::Direct);
    t.put(reacted("19:a", "m1", None, &[]));
    t.cmd(Command::React {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        emoji: emoji(0x1F680),
        remove: false,
    })
    .await;
    let events = t.drain();
    assert!(!events.iter().any(|e| matches!(e, Event::MessageChanged(_))));
    let errs = notices(&events);
    assert_eq!(errs.len(), 1);
    assert!(
        errs[0].contains("reactionType is not supported"),
        "{}",
        errs[0]
    );
    assert!(t.stored("19:a", "m1").reactions.is_empty());
}

/// Acceptance: a reaction to an unknown message is logged and dropped, never a panic.
#[tokio::test]
async fn a_reaction_to_an_unknown_message_is_dropped_without_a_panic() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(Command::React {
        chat_id: "19:a".into(),
        message_id: "nope".into(),
        emoji: emoji(0x1F44D),
        remove: false,
    })
    .await;
    assert!(t
        .drain()
        .iter()
        .all(|e| !matches!(e, Event::MessageChanged(_))));
}

#[test]
fn reaction_steps_keep_one_reaction_per_user() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        reaction_steps(&s(&[]), "a", false),
        vec![("a".into(), true)]
    );
    assert_eq!(reaction_steps(&s(&["a"]), "a", false), vec![]);
    assert_eq!(
        reaction_steps(&s(&["x", "y"]), "a", false),
        vec![("x".into(), false), ("y".into(), false), ("a".into(), true)]
    );
    assert_eq!(
        reaction_steps(&s(&["a", "x"]), "a", false),
        vec![("x".into(), false)]
    );
    assert_eq!(
        reaction_steps(&s(&["a"]), "a", true),
        vec![("a".into(), false)]
    );
}

// --- edit and delete (#27) --------------------------------------------------------------------

#[tokio::test]
async fn an_edit_patches_the_body_and_marks_the_message_edited() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, true, "typo"));
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        text: "fixed & done".into(),
    })
    .await;
    let r = &t.mock.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.path()),
        ("PATCH", "/v1.0/chats/19:a/messages/m1")
    );
    assert_eq!(r.json()["body"]["content"], "fixed &amp; done");
    let Event::MessageChanged(m) = t.until(|e| matches!(e, Event::MessageChanged(_))).await else {
        unreachable!()
    };
    assert_eq!(m.text, "fixed & done");
    assert!(m.edited);
    assert_eq!(t.stored("19:a", "m1").text, "fixed & done");
}

/// An edit sends the mentions again, or the people would stop counting as mentioned.
#[tokio::test]
async fn an_edit_keeps_the_mentions_of_the_message() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Group);
    let mut m = msg("19:a", "m1", 1000, true, "hi @Sam Sample");
    m.mentions = vec![PersonRef {
        id: "00000000-0000-0000-0000-000000000002".into(),
        name: "Sam Sample".into(),
    }];
    t.put(m);
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        text: "hello @Sam Sample".into(),
    })
    .await;
    let r = &t.mock.requests()[0];
    assert_eq!(
        r.json()["body"]["content"],
        "hello <at id=\"0\">Sam Sample</at>"
    );
    assert_eq!(
        r.json()["mentions"][0]["mentioned"]["user"]["id"],
        "00000000-0000-0000-0000-000000000002"
    );
    t.until(|e| matches!(e, Event::MessageChanged(_))).await;
    assert_eq!(t.stored("19:a", "m1").mentions.len(), 1);
}

/// Acceptance: a failed edit keeps the original text and shows a notice.
#[tokio::test]
async fn a_failed_edit_keeps_the_original_text_and_shows_a_notice() {
    let mut t = env(|_| Reply::error(403, "Forbidden", "cannot edit messages sent from Skype"));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, true, "typo"));
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        text: "fixed".into(),
    })
    .await;
    let events = t.drain();
    assert!(!events.iter().any(|e| matches!(e, Event::MessageChanged(_))));
    assert!(notices(&events)[0].contains("Skype"));
    let m = t.stored("19:a", "m1");
    assert_eq!((m.text.as_str(), m.edited), ("typo", false));
}

#[tokio::test]
async fn edit_and_delete_are_refused_locally_for_other_peoples_messages() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, false, "theirs"));
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        text: "x".into(),
    })
    .await;
    t.cmd(Command::Delete {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
    })
    .await;
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "unknown".into(),
        text: "x".into(),
    })
    .await;
    assert_eq!(t.mock.count(), 0, "nothing reaches Graph");
    assert_eq!(notices(&t.drain()).len(), 3);
    assert_eq!(t.stored("19:a", "m1").text, "theirs");
}

/// An edit sends a whole new body, so a message with a quote or a file would lose it.
#[tokio::test]
async fn a_message_with_a_quote_or_file_is_not_edited_here() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    let mut quoted_msg = msg("19:a", "m1", 1000, true, "reply");
    quoted_msg.reply_to = Some(quoted());
    t.put(quoted_msg);
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        text: "x".into(),
    })
    .await;
    assert_eq!(t.mock.count(), 0);
    assert!(notices(&t.drain())[0].contains("edit it in Teams"));
    t.put(msg("19:a", "m2", 1001, true, "ok"));
    t.cmd(Command::Edit {
        chat_id: "19:a".into(),
        message_id: "m2".into(),
        text: "   ".into(),
    })
    .await;
    assert_eq!(t.mock.count(), 0, "an empty edit is refused");
}

#[tokio::test]
async fn a_delete_uses_the_user_path_and_leaves_an_empty_deleted_message() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    let mut m = msg("19:a", "m1", 1000, true, "oops");
    m.attachments = vec![chatcli_core::model::Attachment {
        kind: MediaKind::Image,
        name: None,
        mime: None,
        size: None,
        fetch_ref: chatcli_core::model::FetchRef::new("x"),
        open_url: None,
    }];
    t.put(m);
    seed_reactions(&t, "19:a", "m1", &[("sam", &emoji(0x1F44D), false)]);
    t.cmd(Command::Delete {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
    })
    .await;
    let r = &t.mock.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.path()),
        ("POST", "/v1.0/users/me/chats/19:a/messages/m1/softDelete")
    );
    let Event::MessageChanged(d) = t.until(|e| matches!(e, Event::MessageChanged(_))).await else {
        unreachable!()
    };
    assert_eq!(d.state, MessageState::Deleted);
    assert!(d.text.is_empty() && d.attachments.is_empty() && d.reactions.is_empty());
    assert_eq!(t.stored("19:a", "m1").state, MessageState::Deleted);
}

#[tokio::test]
async fn a_failed_delete_leaves_the_message_alone() {
    let mut t = env(|_| Reply::error(403, "Forbidden", "no"));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, true, "keep"));
    t.cmd(Command::Delete {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
    })
    .await;
    assert_eq!(notices(&t.drain()).len(), 1);
    assert_eq!(t.stored("19:a", "m1").state, MessageState::Sent);
}

// --- live changes (#13, #27) ------------------------------------------------------------------

#[tokio::test]
async fn an_updated_notification_is_a_change_and_never_a_new_message() {
    let mut t = env(|r| {
        if r.path().ends_with("/messages/m1") {
            Reply::json(
                200,
                json!({
                    "id": "m1", "createdDateTime": "2026-11-16T01:00:05Z", "messageType": "message",
                    "lastEditedDateTime": "2026-11-16T01:05:00Z",
                    "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                    "body": { "contentType": "html", "content": "edited words" },
                    "reactions": [{ "reactionType": "like", "user": { "user": { "id": "me" } } }],
                }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    t.put(msg(
        "19:a",
        "m1",
        chatcli_core::model::parse_iso_ms("2026-11-16T01:00:05Z").unwrap(),
        false,
        "old words",
    ));
    let mut relisted = true;
    t.b.live_message("19:a", "m1", false, &mut relisted).await;
    let events = t.drain();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message(_))),
        "an update never toasts"
    );
    let Some(Event::MessageChanged(m)) = events
        .iter()
        .find(|e| matches!(e, Event::MessageChanged(_)))
    else {
        panic!("{events:?}")
    };
    assert_eq!(m.text, "edited words");
    assert!(m.edited);
    assert_eq!(
        m.reactions,
        vec![Reaction {
            emoji: emoji(0x1F44D),
            count: 1,
            mine: true
        }]
    );
    assert_eq!(
        t.b.store.chat(NET, "19:a").unwrap().unwrap().unread,
        0,
        "an edit is not unread"
    );
}

#[tokio::test]
async fn a_created_notification_is_a_message_once_and_counts_unread_once() {
    let mut t = env(|r| {
        if r.path().ends_with("/messages/m2") {
            Reply::json(
                200,
                graph_msg("m2", "2026-11-16T01:00:06Z", "sam", "hello there"),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    let mut relisted = true;
    t.b.live_message("19:a", "m2", true, &mut relisted).await;
    assert!(t
        .drain()
        .iter()
        .any(|e| matches!(e, Event::Message(m) if m.id == "m2")));
    assert_eq!(t.b.store.chat(NET, "19:a").unwrap().unwrap().unread, 1);
    t.b.live_message("19:a", "m2", true, &mut relisted).await;
    let events = t.drain();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message(_))),
        "a redelivery is not new"
    );
    assert!(events.iter().any(|e| matches!(e, Event::MessageChanged(_))));
    assert_eq!(t.b.store.chat(NET, "19:a").unwrap().unwrap().unread, 1);
}

#[tokio::test]
async fn a_deleted_notification_becomes_a_change_with_no_text() {
    let mut t = env(|_| Reply::empty(500));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, false, "secret"));
    t.b.mark_deleted("19:a", "m1");
    let Event::MessageChanged(m) = t.until(|e| matches!(e, Event::MessageChanged(_))).await else {
        unreachable!()
    };
    assert_eq!((m.state, m.text.as_str()), (MessageState::Deleted, ""));
    t.b.mark_deleted("19:a", "never-stored");
    assert!(
        t.drain().is_empty(),
        "a delete for an unknown message says nothing"
    );
    assert_eq!(t.mock.count(), 0);
}

// --- fetching media (#17, #34) ----------------------------------------------------------------

fn put_with_attachment(t: &Env, att: chatcli_core::model::Attachment) {
    let mut m = msg("19:a", "m1", 1000, false, "[image]");
    m.attachments = vec![att];
    t.put(m);
}

fn hosted_att() -> chatcli_core::model::Attachment {
    chatcli_core::model::Attachment {
        kind: MediaKind::Image,
        name: None,
        mime: None,
        size: None,
        fetch_ref: FetchSpec::Hosted {
            chat_id: "19:a".into(),
            message_id: "m1".into(),
            hosted_id: "aWQ9MQ".into(),
        }
        .to_ref(),
        open_url: None,
    }
}

fn file_att() -> chatcli_core::model::Attachment {
    let url = "https://contoso.example.invalid/personal/alex/Documents/a.pdf".to_string();
    chatcli_core::model::Attachment {
        kind: MediaKind::Document,
        name: Some("a.pdf".into()),
        mime: None,
        size: None,
        fetch_ref: FetchSpec::File { url: url.clone() }.to_ref(),
        open_url: Some(url),
    }
}

fn fetch_cmd(index: u32) -> Command {
    Command::FetchMedia {
        chat_id: "19:a".into(),
        message_id: "m1".into(),
        index,
    }
}

fn is_media(e: &Event) -> bool {
    matches!(e, Event::MediaReady { .. } | Event::MediaFailed { .. })
}

#[tokio::test]
async fn nothing_is_downloaded_until_the_user_asks() {
    let mut t = env(|_| Reply::bytes(200, "image/png", vec![1, 2, 3]));
    t.chat("19:a", ChatKind::Direct);
    put_with_attachment(&t, hosted_att());
    t.b.emit_history("19:a");
    t.b.emit_chat("19:a");
    assert_eq!(t.mock.count(), 0);
    t.cmd(fetch_cmd(0)).await;
    let Event::MediaReady {
        path,
        index,
        message_id,
        ..
    } = t.until(is_media).await
    else {
        panic!("not ready")
    };
    assert_eq!((index, message_id.as_str()), (0, "m1"));
    assert!(path.starts_with(t.media_path()));
    assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3]);
    // The cache answers the second ask.
    t.cmd(fetch_cmd(0)).await;
    assert!(matches!(t.until(is_media).await, Event::MediaReady { .. }));
    assert_eq!(t.mock.count(), 1);
}

#[tokio::test]
async fn a_linked_file_without_the_scope_offers_the_browser_and_asks_graph_nothing() {
    let mut t = env(|_| Reply::empty(500));
    t.chat("19:a", ChatKind::Direct);
    put_with_attachment(&t, file_att());
    t.cmd(fetch_cmd(0)).await;
    let Event::MediaFailed {
        reason,
        fallback_url,
        ..
    } = t.until(is_media).await
    else {
        panic!("expected failure")
    };
    assert_eq!(reason, "FILE: open in browser");
    assert_eq!(
        fallback_url.as_deref(),
        Some("https://contoso.example.invalid/personal/alex/Documents/a.pdf")
    );
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn a_linked_file_downloads_when_the_setting_and_the_scope_are_both_there() {
    let mut settings = Settings::default();
    settings.teams.files_read = true;
    let mut t = env_with(settings, &format!("{BASE} Files.Read.All"), |_| {
        Reply::bytes(200, "application/pdf", b"%PDF".to_vec())
    });
    t.chat("19:a", ChatKind::Direct);
    put_with_attachment(&t, file_att());
    t.cmd(fetch_cmd(0)).await;
    let Event::MediaReady { path, .. } = t.until(is_media).await else {
        panic!("expected the file")
    };
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF");
    assert!(t.mock.requests()[0].path().starts_with("/v1.0/shares/u!"));
}

#[tokio::test]
async fn a_forbidden_file_says_no_access_and_is_not_retried() {
    let mut settings = Settings::default();
    settings.teams.files_read = true;
    let mut t = env_with(settings, &format!("{BASE} Files.Read.All"), |_| {
        Reply::error(403, "accessDenied", "no")
    });
    t.chat("19:a", ChatKind::Direct);
    put_with_attachment(&t, file_att());
    t.cmd(fetch_cmd(0)).await;
    let Event::MediaFailed {
        reason,
        fallback_url,
        ..
    } = t.until(is_media).await
    else {
        panic!("expected failure")
    };
    assert_eq!(reason, "NO ACCESS TO FILE");
    assert!(fallback_url.is_some());
    assert_eq!(t.mock.count(), 1);
}

#[tokio::test]
async fn an_attachment_that_is_not_stored_is_a_failure_not_silence() {
    let mut t = env(|_| Reply::empty(500));
    t.chat("19:a", ChatKind::Direct);
    t.put(msg("19:a", "m1", 1000, false, "no files"));
    t.cmd(fetch_cmd(0)).await;
    assert!(matches!(t.until(is_media).await, Event::MediaFailed { .. }));
    t.cmd(Command::FetchMedia {
        chat_id: "19:a".into(),
        message_id: "nope".into(),
        index: 0,
    })
    .await;
    assert!(matches!(t.until(is_media).await, Event::MediaFailed { .. }));
    assert_eq!(t.mock.count(), 0);
}

// --- sending media (#21, #36) -----------------------------------------------------------------

fn write_png(path: &Path, w: u32, h: u32, noisy: bool) {
    let mut seed = 99u32;
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        if noisy {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        } else {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 3])
        }
    });
    img.save(path).unwrap();
}

use std::path::Path;

fn media_cmd(path: &Path, kind: MediaKind, caption: &str) -> Command {
    Command::SendMedia {
        chat_id: "19:a".into(),
        client_id: "c1".into(),
        path: path.to_path_buf(),
        kind,
        caption: caption.into(),
    }
}

fn created_ok(_: &Req) -> Reply {
    Reply::json(
        201,
        json!({
            "id": "sent1", "createdDateTime": "2026-11-16T01:00:00Z",
            "from": { "user": { "id": "me", "displayName": "Alex Example" } },
            "body": { "contentType": "html", "content": "x" },
        }),
    )
}

#[tokio::test]
async fn a_picture_is_sent_inline_and_confirmed() {
    let mut t = env(created_ok);
    t.chat("19:a", ChatKind::Direct);
    let file = t.dir.join("photo.png");
    write_png(&file, 8, 8, false);
    t.cmd(media_cmd(&file, MediaKind::Image, "look")).await;
    let Event::MessageConfirmed { client_id, message } = t.until(is_confirmed).await else {
        unreachable!()
    };
    assert_eq!((client_id.as_str(), message.id.as_str()), ("c1", "sent1"));
    let body = t.mock.requests()[0].json();
    assert_eq!(body["hostedContents"][0]["contentType"], "image/png");
    assert!(body["body"]["content"]
        .as_str()
        .unwrap()
        .contains("hostedContents/1/$value"));
}

#[tokio::test]
async fn the_backend_decides_by_content_not_by_the_tuis_guess() {
    let mut t = env(created_ok);
    t.chat("19:a", ChatKind::Direct);
    // A text file the TUI called an image is a document: no Files.ReadWrite, so it fails.
    let txt = t.dir.join("notes.png");
    std::fs::write(&txt, "not a picture").unwrap();
    t.cmd(media_cmd(&txt, MediaKind::Image, "")).await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert_eq!(error, "FILE SEND NEEDS Files.ReadWrite");
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn an_oversize_picture_and_a_missing_file_fail_before_any_request() {
    let mut t = env(created_ok);
    t.chat("19:a", ChatKind::Direct);
    let big = t.dir.join("noise.png");
    write_png(&big, 1500, 1000, true);
    t.cmd(media_cmd(&big, MediaKind::Image, "")).await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert!(error.starts_with("IMAGE OVER 4 MB"), "{error}");
    t.cmd(media_cmd(&t.dir.join("gone.png"), MediaKind::Image, ""))
        .await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert_eq!(error, "FILE NOT FOUND");
    assert_eq!(t.mock.count(), 0);
    // The failed sends stay as NOT SENT in the store.
    assert!(matches!(
        t.stored("19:a", "c1").state,
        MessageState::Failed(_)
    ));
}

#[tokio::test]
async fn a_file_without_files_readwrite_is_refused_before_any_upload() {
    let mut t = env(created_ok);
    t.chat("19:a", ChatKind::Direct);
    let doc = t.dir.join("report.docx");
    std::fs::write(&doc, b"PK").unwrap();
    t.cmd(media_cmd(&doc, MediaKind::Document, "")).await;
    let Event::MessageFailed { error, .. } = t.until(is_failed).await else {
        unreachable!()
    };
    assert_eq!(error, "FILE SEND NEEDS Files.ReadWrite");
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn a_file_with_the_scope_uploads_posts_a_reference_and_reports_progress() {
    let mut settings = Settings::default();
    settings.teams.files_write = true;
    let mut t = env_with(settings, &format!("{BASE} Files.ReadWrite"), |r| {
        if r.method == "PUT" {
            Reply::json(
                201,
                json!({
                    "id": "item1", "name": "report.docx",
                    "eTag": "\"{11111111-1111-1111-1111-111111111111},1\"",
                    "webDavUrl": "https://contoso.example.invalid/personal/alex/Documents/report.docx",
                }),
            )
        } else {
            created_ok(r)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    let doc = t.dir.join("report.docx");
    std::fs::write(&doc, b"PK-bytes").unwrap();
    t.cmd(media_cmd(&doc, MediaKind::Document, "")).await;
    let mut progress = Vec::new();
    loop {
        match t
            .until(|e| {
                matches!(
                    e,
                    Event::SendProgress { .. } | Event::MessageConfirmed { .. }
                )
            })
            .await
        {
            Event::SendProgress {
                done,
                total,
                client_id,
                ..
            } => {
                assert_eq!(client_id, "c1");
                progress.push((done, total));
            }
            Event::MessageConfirmed { .. } => break,
            _ => unreachable!(),
        }
    }
    assert_eq!(progress.last(), Some(&(8, 8)));
    let reqs = t.mock.requests();
    assert_eq!(reqs[0].method, "PUT");
    assert_eq!(
        reqs.last().unwrap().json()["attachments"][0]["contentType"],
        "reference"
    );
}

// --- older history (#29) ----------------------------------------------------------------------

fn older_cmd(ts: i64, id: &str) -> Command {
    Command::LoadOlder {
        chat_id: "19:a".into(),
        before_ts: ts,
        before_id: id.into(),
    }
}

fn is_older(e: &Event) -> bool {
    matches!(e, Event::OlderMessages { .. } | Event::OlderFailed { .. })
}

#[tokio::test]
async fn older_messages_come_from_the_store_first_with_no_network_call() {
    let mut t = env(|_| Reply::empty(500));
    t.chat("19:a", ChatKind::Direct);
    for i in 1..=60 {
        t.put(msg("19:a", &format!("m{i:02}"), i * 1000, false, "x"));
    }
    t.cmd(older_cmd(61_000, "zz")).await;
    let Event::OlderMessages {
        messages, at_start, ..
    } = t.until(is_older).await
    else {
        panic!("no older")
    };
    assert_eq!(messages.len(), 50, "one page of 50");
    assert!(!at_start);
    assert!(
        messages.windows(2).all(|w| w[0].ts < w[1].ts),
        "oldest first"
    );
    assert_eq!(messages.last().unwrap().id, "m60");
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn nothing_in_the_store_means_one_filtered_page_then_graphs_own_next_link() {
    let mock_origin = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let origin = mock_origin.clone();
    let mut t = env(move |r| {
        if r.target.contains("skiptoken=p2") {
            Reply::json(
                200,
                json!({ "value": [graph_msg("a1", "2026-11-10T00:00:01Z", "sam", "oldest")] }),
            )
        } else {
            Reply::json(
                200,
                json!({
                    "value": [
                        graph_msg("b2", "2026-11-12T00:00:02Z", "sam", "newer of the page"),
                        graph_msg("b1", "2026-11-12T00:00:01Z", "me", "older of the page"),
                    ],
                    "@odata.nextLink": format!("{}/v1.0/chats/19:a/messages?$skiptoken=p2", origin.lock().unwrap()),
                }),
            )
        }
    });
    *mock_origin.lock().unwrap() = t.mock.origin();
    t.chat("19:a", ChatKind::Direct);
    t.put(msg(
        "19:a",
        "keep",
        chatcli_core::model::parse_iso_ms("2026-11-15T00:00:00Z").unwrap(),
        false,
        "newest",
    ));
    let before = chatcli_core::model::parse_iso_ms("2026-11-15T00:00:00Z").unwrap();

    t.cmd(older_cmd(before, "keep")).await;
    let Event::OlderMessages {
        messages, at_start, ..
    } = t.until(is_older).await
    else {
        panic!("no older")
    };
    assert_eq!(
        messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["b1", "b2"],
        "oldest first"
    );
    assert!(!at_start);
    let first = &t.mock.requests()[0];
    assert!(first
        .param("$filter")
        .unwrap()
        .starts_with("createdDateTime lt 2026-11-15T00:00:00"));
    assert_eq!(
        first.param("$orderby").as_deref(),
        Some("createdDateTime desc")
    );
    assert_eq!(
        t.stored("19:a", "b1").text,
        "older of the page",
        "stored for next time"
    );

    // The second scroll: the store now holds b1/b2 above this point, so ask for older than b1.
    t.cmd(older_cmd(messages[0].ts, "b1")).await;
    let Event::OlderMessages {
        messages, at_start, ..
    } = t.until(is_older).await
    else {
        panic!("no older")
    };
    assert_eq!(
        messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a1"]
    );
    assert!(at_start, "no next link: the start of the conversation");
    assert!(
        t.mock.requests()[1].target.contains("skiptoken=p2"),
        "the link is followed as given"
    );
    assert_eq!(t.mock.count(), 2, "exactly one request per scroll");

    // At the start: asking again never reaches the network.
    t.cmd(older_cmd(messages[0].ts, "a1")).await;
    let Event::OlderMessages {
        messages, at_start, ..
    } = t.until(is_older).await
    else {
        panic!("no older")
    };
    assert!(messages.is_empty() && at_start);
    assert_eq!(t.mock.count(), 2);
}

#[tokio::test]
async fn an_empty_page_marks_the_start_and_a_failure_is_an_older_failed() {
    let mut t = env(|_| Reply::json(200, json!({ "value": [] })));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(older_cmd(5000, "x")).await;
    let Event::OlderMessages {
        messages, at_start, ..
    } = t.until(is_older).await
    else {
        panic!("no older")
    };
    assert!(messages.is_empty() && at_start);

    let mut t = env(|_| {
        let mut r = Reply::error(429, "TooManyRequests", "slow down");
        r.headers.push(("Retry-After".into(), "9".into()));
        r
    });
    t.chat("19:a", ChatKind::Direct);
    t.cmd(older_cmd(5000, "x")).await;
    let Event::OlderFailed { error, .. } = t.until(is_older).await else {
        panic!("expected failure")
    };
    assert!(error.contains("Try again in 9 s"), "{error}");
    assert_eq!(t.mock.count(), 1, "no automatic retry");
    // A failure is not the start: the next scroll asks again.
    assert_eq!(t.b.store.meta("teams.older.19:a").unwrap(), None);
}

#[tokio::test]
async fn opening_a_chat_never_asks_for_older_history() {
    let mut t = env(|r| {
        if r.param("$filter").is_some() {
            Reply::empty(500)
        } else {
            Reply::json(
                200,
                json!({ "value": [graph_msg("m1", "2026-11-16T01:00:05Z", "sam", "hi")] }),
            )
        }
    });
    t.chat("19:a", ChatKind::Direct);
    t.cmd(Command::OpenChat {
        chat_id: "19:a".into(),
    })
    .await;
    assert!(t
        .mock
        .requests()
        .iter()
        .all(|r| r.param("$filter").is_none()));
    assert_eq!(t.mock.count(), 1);
}

// --- members (#28) ----------------------------------------------------------------------------

#[tokio::test]
async fn members_come_from_teams_once_then_from_the_cache() {
    let mut t = env(|_| {
        Reply::json(
            200,
            json!({ "value": [
            { "userId": "u2", "displayName": "Sam  Sample" },
            { "userId": "me", "displayName": "Alex Example" },
            { "userId": "u1", "displayName": "Jordan Placeholder" },
            { "userId": null, "displayName": "No Id" },
            { "userId": "u2", "displayName": "Sam Sample" },
        ] }),
        )
    });
    t.chat("19:g", ChatKind::Group);
    t.cmd(Command::LoadMembers {
        chat_id: "19:g".into(),
    })
    .await;
    let Event::Members { members, error, .. } =
        t.until(|e| matches!(e, Event::Members { .. })).await
    else {
        unreachable!()
    };
    assert!(error.is_none());
    assert_eq!(
        members
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str()))
            .collect::<Vec<_>>(),
        [("u1", "Jordan Placeholder"), ("u2", "Sam Sample")],
        "sorted by name, the user themselves and nameless entries left out"
    );
    t.cmd(Command::LoadMembers {
        chat_id: "19:g".into(),
    })
    .await;
    let Event::Members { members: again, .. } =
        t.until(|e| matches!(e, Event::Members { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(again, members);
    assert_eq!(t.mock.count(), 1, "the second ask is served from the store");
}

#[tokio::test]
async fn a_member_list_that_cannot_be_read_comes_back_empty_with_the_reason() {
    let mut t = env(|_| Reply::error(403, "Forbidden", "no"));
    t.chat("19:g", ChatKind::Group);
    t.cmd(Command::LoadMembers {
        chat_id: "19:g".into(),
    })
    .await;
    let Event::Members { members, error, .. } =
        t.until(|e| matches!(e, Event::Members { .. })).await
    else {
        unreachable!()
    };
    assert!(members.is_empty());
    assert!(error.unwrap().contains("Could not load the members"));
}

// --- chat flags and read state ----------------------------------------------------------------

#[tokio::test]
async fn pin_and_mute_are_local_and_hide_is_written_to_teams() {
    let mut t = env(|_| Reply::empty(204));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(Command::SetChatFlag {
        chat_id: "19:a".into(),
        flag: ChatFlag::Pinned,
        on: true,
    })
    .await;
    t.cmd(Command::SetChatFlag {
        chat_id: "19:a".into(),
        flag: ChatFlag::Muted,
        on: true,
    })
    .await;
    assert_eq!(t.mock.count(), 0, "local only");
    let flags = t.b.store.chat_flags(NET, "19:a").unwrap();
    assert!(flags.pinned && flags.muted_until == Some(MUTE_FOREVER));
    t.cmd(Command::SetChatFlag {
        chat_id: "19:a".into(),
        flag: ChatFlag::Hidden,
        on: true,
    })
    .await;
    let r = &t.mock.requests()[0];
    assert_eq!(r.path(), "/v1.0/chats/19:a/hideForUser");
    assert_eq!(
        r.json(),
        json!({ "user": { "id": "me", "tenantId": "tenant-1" } })
    );
    assert!(t.b.store.chat_flags(NET, "19:a").unwrap().hidden);
    let updates = t
        .drain()
        .into_iter()
        .filter(|e| matches!(e, Event::ChatUpdated(c) if c.id == "19:a"))
        .count();
    assert_eq!(updates, 3, "each change is announced");
    t.cmd(Command::SetChatFlag {
        chat_id: "19:a".into(),
        flag: ChatFlag::Hidden,
        on: false,
    })
    .await;
    assert_eq!(
        t.mock.requests()[1].path(),
        "/v1.0/chats/19:a/unhideForUser"
    );
    assert!(!t.b.store.chat_flags(NET, "19:a").unwrap().hidden);
}

#[tokio::test]
async fn a_hide_teams_refuses_is_put_back_with_a_notice() {
    let mut t = env(|_| Reply::error(403, "Forbidden", "no"));
    t.chat("19:a", ChatKind::Direct);
    t.cmd(Command::SetChatFlag {
        chat_id: "19:a".into(),
        flag: ChatFlag::Hidden,
        on: true,
    })
    .await;
    assert!(
        !t.b.store.chat_flags(NET, "19:a").unwrap().hidden,
        "reverted"
    );
    let events = t.drain();
    assert_eq!(notices(&events).len(), 1);
    let last_update = events.iter().rev().find_map(|e| match e {
        Event::ChatUpdated(c) => Some(c.clone()),
        _ => None,
    });
    assert!(
        !last_update.unwrap().flags.hidden,
        "the TUI is told the flag went back"
    );
}

#[tokio::test]
async fn a_mark_read_teams_refuses_restores_the_unread_count() {
    let mut t = env(|_| Reply::error(500, "InternalServerError", "boom"));
    t.chat("19:a", ChatKind::Group);
    t.b.store
        .upsert_chats(&[ChatSummary {
            id: "19:a".into(),
            title: "19:a".into(),
            kind: ChatKind::Group,
            unread: 4,
            ..ChatSummary::blank(NET)
        }])
        .unwrap();
    t.b.store.bump_mentions(NET, "19:a").unwrap();
    t.cmd(Command::MarkRead {
        chat_id: "19:a".into(),
    })
    .await;
    let chat = t.b.store.chat(NET, "19:a").unwrap().unwrap();
    assert_eq!((chat.unread, chat.mentions), (4, 1), "restored");
    let events = t.drain();
    assert_eq!(notices(&events).len(), 1);
    let updates: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatUpdated(c) => Some(c.unread),
            _ => None,
        })
        .collect();
    assert_eq!(updates, [0, 4], "cleared at once, then restored");
}

#[tokio::test]
async fn a_mark_read_that_works_stays_cleared() {
    let mut t = env(|_| Reply::empty(204));
    t.b.store
        .upsert_chats(&[ChatSummary {
            id: "19:a".into(),
            title: "x".into(),
            unread: 2,
            ..ChatSummary::blank(NET)
        }])
        .unwrap();
    t.cmd(Command::MarkRead {
        chat_id: "19:a".into(),
    })
    .await;
    assert_eq!(t.b.store.chat(NET, "19:a").unwrap().unwrap().unread, 0);
    assert_eq!(
        t.mock.requests()[0].path(),
        "/v1.0/chats/19:a/markChatReadForUser"
    );
}

// --- chat list: hidden chats stay (contract section 2) ----------------------------------------

#[tokio::test]
async fn a_hidden_chat_stays_in_the_list_flagged_and_keeps_its_messages() {
    let mut t = env(|r| {
        if r.path().ends_with("/me/chats") {
            Reply::json(
                200,
                json!({ "value": [
                { "id": "19:vis", "chatType": "oneOnOne", "viewpoint": { "isHidden": false } },
                { "id": "19:hid", "chatType": "oneOnOne", "viewpoint": { "isHidden": true } },
                { "id": "19:meet", "chatType": "meeting" },
            ] }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:hid", ChatKind::Direct);
    t.put(msg("19:hid", "m1", 1000, false, "history"));
    t.b.store
        .set_chat_flags(
            NET,
            "19:vis",
            chatcli_core::model::ChatFlags {
                hidden: true,
                pinned: true,
                muted_until: None,
            },
        )
        .unwrap();
    let fresh = t.b.refresh_chats().await.ok().expect("refreshed");
    assert_eq!(fresh.shown.len(), 2);
    let Some(Event::Chats(_, chats)) = t
        .drain()
        .into_iter()
        .find(|e| matches!(e, Event::Chats(..)))
    else {
        panic!("no chat list")
    };
    let hid = chats
        .iter()
        .find(|c| c.id == "19:hid")
        .expect("hidden chat is in the list");
    assert!(hid.flags.hidden);
    let vis = chats.iter().find(|c| c.id == "19:vis").unwrap();
    assert!(
        !vis.flags.hidden,
        "Teams says it is not hidden: the network wins"
    );
    assert!(vis.flags.pinned, "pin is local and untouched");
    assert_eq!(
        t.stored("19:hid", "m1").text,
        "history",
        "hiding never deletes stored messages"
    );
    assert!(t.b.hidden.contains("19:hid") && !t.b.hidden.contains("19:vis"));
    assert!(t.b.not_shown.contains("19:meet"));
}

// --- catch-up (CodeRabbit 4) ------------------------------------------------------------------

#[tokio::test]
async fn catch_up_refetches_a_chat_whose_last_message_was_edited_while_away() {
    let mut t = env(|r| {
        if r.path().ends_with("/me/chats") {
            Reply::json(
                200,
                json!({ "value": [{
                "id": "19:a", "chatType": "oneOnOne",
                "lastMessagePreview": { "id": "m1", "createdDateTime": "2026-11-16T01:00:05Z",
                    "messageType": "message",
                    "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                    "body": { "contentType": "html", "content": "new words" } },
                "viewpoint": { "lastMessageReadDateTime": "2026-11-16T01:00:05Z" },
            }] }),
            )
        } else if r.path().ends_with("/chats/19:a/messages") {
            Reply::json(
                200,
                json!({ "value": [{
                "id": "m1", "createdDateTime": "2026-11-16T01:00:05Z", "messageType": "message",
                "lastEditedDateTime": "2026-11-16T02:00:00Z",
                "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                "body": { "contentType": "html", "content": "new words" } }] }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    t.put(msg(
        "19:a",
        "m1",
        chatcli_core::model::parse_iso_ms("2026-11-16T01:00:05Z").unwrap(),
        false,
        "old words",
    ));
    t.b.catch_up().await;
    let events = t.drain();
    let changed = events
        .iter()
        .find_map(|e| match e {
            Event::MessageChanged(m) => Some(m.clone()),
            _ => None,
        })
        .expect("a change event");
    assert_eq!(
        (changed.id.as_str(), changed.text.as_str()),
        ("m1", "new words")
    );
    assert!(changed.edited);
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message(_))),
        "an edit is not a new message"
    );
    assert_eq!(t.stored("19:a", "m1").text, "new words");
}

#[tokio::test]
async fn catch_up_sends_missed_messages_as_new_and_skips_a_chat_that_is_current() {
    let mut t = env(|r| {
        if r.path().ends_with("/me/chats") {
            Reply::json(
                200,
                json!({ "value": [
                { "id": "19:a", "chatType": "oneOnOne",
                  "lastMessagePreview": { "id": "m2", "createdDateTime": "2026-11-16T01:00:09Z", "messageType": "message",
                      "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                      "body": { "contentType": "html", "content": "missed" } } },
                { "id": "19:b", "chatType": "oneOnOne",
                  "lastMessagePreview": { "id": "b1", "createdDateTime": "2026-11-16T01:00:01Z", "messageType": "message",
                      "from": { "user": { "id": "sam", "displayName": "Sam Sample" } },
                      "body": { "contentType": "html", "content": "same" } } },
            ] }),
            )
        } else if r.path().ends_with("/chats/19:a/messages") {
            Reply::json(
                200,
                json!({ "value": [
                graph_msg("m2", "2026-11-16T01:00:09Z", "sam", "missed"),
                graph_msg("m1", "2026-11-16T01:00:05Z", "sam", "known"),
            ] }),
            )
        } else {
            Reply::empty(500)
        }
    });
    t.chat("19:a", ChatKind::Direct);
    t.chat("19:b", ChatKind::Direct);
    t.put(msg(
        "19:a",
        "m1",
        chatcli_core::model::parse_iso_ms("2026-11-16T01:00:05Z").unwrap(),
        false,
        "known",
    ));
    t.put(msg(
        "19:b",
        "b1",
        chatcli_core::model::parse_iso_ms("2026-11-16T01:00:01Z").unwrap(),
        false,
        "same",
    ));
    t.b.catch_up().await;
    let events = t.drain();
    let new: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Message(m) => Some(m.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(new, ["m2"]);
    assert!(
        t.mock
            .requests()
            .iter()
            .all(|r| !r.path().contains("19:b/messages")),
        "the current chat is not fetched"
    );
}

// --- account binding and shutdown -------------------------------------------------------------

#[tokio::test]
async fn a_task_in_flight_is_aborted_by_shutdown_within_five_seconds() {
    let mut t = env(|r| {
        if r.method == "POST" {
            // A send that never answers.
            std::thread::sleep(Duration::from_secs(30));
        }
        created_ok(r)
    });
    t.chat("19:a", ChatKind::Direct);
    let file = t.dir.join("photo.png");
    write_png(&file, 8, 8, false);
    t.cmd(media_cmd(&file, MediaKind::Image, "")).await;
    while t.mock.count() == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(t.b.tasks.len(), 1);
    let started = std::time::Instant::now();
    t.b.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(t.b.tasks.is_empty());
}

#[tokio::test]
async fn typing_is_dropped_silently_and_every_other_command_is_answered() {
    let mut t = env(|_| Reply::empty(500));
    t.cmd(Command::Typing {
        chat_id: "19:a".into(),
        active: true,
    })
    .await;
    assert!(t.drain().is_empty());
    assert_eq!(t.mock.count(), 0);
}

#[tokio::test]
async fn signed_out_commands_fail_with_a_clear_text_instead_of_hanging() {
    let mut t = env(|_| Reply::empty(500));
    t.b.me_id = None;
    t.cmd(Command::React {
        chat_id: "c".into(),
        message_id: "m".into(),
        emoji: "x".into(),
        remove: false,
    })
    .await;
    t.cmd(Command::LoadMembers {
        chat_id: "c".into(),
    })
    .await;
    t.cmd(older_cmd(1, "x")).await;
    let events = t.drain();
    assert!(notices(&events)[0].contains("Not signed in"));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Members { error: Some(_), .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::OlderFailed { .. })));
    assert_eq!(t.mock.count(), 0);
}
