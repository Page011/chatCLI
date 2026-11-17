//! The shared vocabulary between the network backends (teams, whatsapp) and the TUI.
//!
//! Flow: each backend runs as one tokio task. It receives `Command`s from the TUI on its own
//! channel and sends `Event`s to the TUI on a shared channel. Backends never touch the
//! terminal; the TUI never touches the network.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Network {
    Teams,
    WhatsApp,
}

impl Network {
    pub const ALL: [Network; 2] = [Network::Teams, Network::WhatsApp];

    /// Stable key for the store and logs. Never change: stored rows are keyed by it.
    pub fn key(self) -> &'static str {
        match self {
            Network::Teams => "teams",
            Network::WhatsApp => "whatsapp",
        }
    }

    pub fn from_key(key: &str) -> Option<Network> {
        match key {
            "teams" => Some(Network::Teams),
            "whatsapp" => Some(Network::WhatsApp),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Network::Teams => "Teams",
            Network::WhatsApp => "WhatsApp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Direct,
    Group,
}

impl ChatKind {
    pub fn key(self) -> &'static str {
        match self {
            ChatKind::Direct => "direct",
            ChatKind::Group => "group",
        }
    }

    pub fn from_key(key: &str) -> ChatKind {
        if key == "group" {
            ChatKind::Group
        } else {
            ChatKind::Direct
        }
    }
}

// ---------- people, quotes, reactions, mentions ----------

/// A person by network id plus display name. Used for mentions and chat members.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PersonRef {
    /// Teams: AAD user id. WhatsApp: JID string (phone or LID form).
    pub id: String,
    /// Display name, one line, plain text.
    pub name: String,
}

/// The message a reply quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedRef {
    /// Network message id of the quoted message.
    pub id: String,
    /// Display label of the quoted sender ("You" for own).
    pub sender: String,
    /// Network id of the quoted sender; "" when unknown.
    pub sender_id: String,
    /// One line, plain text, at most 120 chars; "" = not available (TUI shows a placeholder).
    pub preview: String,
}

/// Aggregated reactions on one message: one entry per emoji.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    /// True when one of the `count` is the signed-in user.
    pub mine: bool,
}

// ---------- media ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
    Other,
}

impl MediaKind {
    /// Stable store key. Never change: stored rows are keyed by it.
    pub fn key(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Document => "document",
            MediaKind::Other => "other",
        }
    }

    /// Unknown keys map to Other.
    pub fn from_key(key: &str) -> MediaKind {
        match key {
            "image" => MediaKind::Image,
            "video" => MediaKind::Video,
            "audio" => MediaKind::Audio,
            "document" => MediaKind::Document,
            _ => MediaKind::Other,
        }
    }
}

/// Opaque, network-specific recipe to fetch one attachment (JSON chosen by the backend).
/// WhatsApp's holds the media key: treat it as a secret. Debug never prints it.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct FetchRef(String);

impl FetchRef {
    pub fn new(raw: impl Into<String>) -> FetchRef {
        FetchRef(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for FetchRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetchRef(<redacted>)")
    }
}

/// Metadata of one attachment. Never bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub kind: MediaKind,
    /// Sender's file name, display only. NEVER used as a path component.
    pub name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
    /// Empty for a local pending attachment (not yet sent).
    pub fetch_ref: FetchRef,
    /// Teams `reference` files only: the https contentUrl for the browser fallback.
    pub open_url: Option<String>,
}

// ---------- chat flags ----------

pub const MUTE_FOREVER: i64 = i64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatFlags {
    pub pinned: bool,
    /// Unix ms; Some(MUTE_FOREVER) = muted with no end. None = not muted.
    pub muted_until: Option<i64>,
    pub hidden: bool,
}

impl ChatFlags {
    /// Muted now: an end time still ahead of `now_ms` (MUTE_FOREVER never ends).
    pub fn is_muted(&self, now_ms: i64) -> bool {
        self.muted_until.is_some_and(|until| until > now_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatFlag {
    Pinned,
    Muted,
    Hidden,
}

// ---------- capabilities ----------

/// What a backend can do right now (scopes, settings, network). The TUI dims actions that are
/// false. It may still send the command; the backend then answers with its failure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub reply: bool,
    pub react: bool,
    pub edit: bool,
    pub delete: bool,
    pub mentions: bool,
    pub send_image: bool,
    pub send_file: bool,
    pub fetch_media: bool,
    /// Teams `reference` files downloadable (Files.Read.All granted).
    pub fetch_files: bool,
    pub load_older: bool,
    pub typing: bool,
    pub pin: bool,
    pub mute: bool,
    pub hide: bool,
}

impl Capabilities {
    /// Every capability on (the demo backend).
    pub fn all() -> Capabilities {
        Capabilities {
            reply: true,
            react: true,
            edit: true,
            delete: true,
            mentions: true,
            send_image: true,
            send_file: true,
            fetch_media: true,
            fetch_files: true,
            load_older: true,
            typing: true,
            pin: true,
            mute: true,
            hide: true,
        }
    }
}

/// One row in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSummary {
    pub network: Network,
    /// The network's own id (Teams `19:...@thread.v2`, WhatsApp JID).
    pub id: String,
    pub title: String,
    pub kind: ChatKind,
    /// Last message time, Unix milliseconds UTC. None = no messages yet.
    pub last_ts: Option<i64>,
    /// "Sender: text" of the last message, one line, already plain text.
    pub last_preview: String,
    pub unread: u32,
    /// Unread messages that mention the signed-in user (subset of `unread`).
    pub mentions: u32,
    pub flags: ChatFlags,
}

impl ChatSummary {
    /// `network` set; every other field empty, zero or default. Base for
    /// `..ChatSummary::blank(n)`.
    pub fn blank(network: Network) -> ChatSummary {
        ChatSummary {
            network,
            id: String::new(),
            title: String::new(),
            kind: ChatKind::Direct,
            last_ts: None,
            last_preview: String::new(),
            unread: 0,
            mentions: 0,
            flags: ChatFlags::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageState {
    /// Confirmed by the network.
    Sent,
    /// Typed in chatcli, not yet confirmed.
    Pending,
    /// The network refused it; the reason is shown to the user.
    Failed(String),
    Deleted,
}

impl MessageState {
    pub fn key(&self) -> &'static str {
        match self {
            MessageState::Sent => "sent",
            MessageState::Pending => "pending",
            MessageState::Failed(_) => "failed",
            MessageState::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub network: Network,
    pub chat_id: String,
    /// The network's message id. For a Pending message: the client id from Command::Send,
    /// replaced by the real id once confirmed (Event::MessageConfirmed).
    pub id: String,
    /// Unix milliseconds UTC.
    pub ts: i64,
    pub sender: String,
    pub from_me: bool,
    /// Plain text, one logical message. Media shows as markers like "[image]".
    pub text: String,
    pub state: MessageState,
    /// Network id of the sender (Teams AAD id, WhatsApp JID); "" when unknown (rows from v1).
    pub sender_id: String,
    pub reply_to: Option<QuotedRef>,
    /// Ordered by count desc, then emoji asc (deterministic for snapshots).
    pub reactions: Vec<Reaction>,
    pub edited: bool,
    /// In network order; the index is the `index` in FetchMedia / MediaReady.
    pub attachments: Vec<Attachment>,
    /// Each mention appears in `text` as "@" + name.
    pub mentions: Vec<PersonRef>,
    pub mentions_me: bool,
}

impl ChatMessage {
    /// `network` set; strings empty, ts 0, from_me false, state Sent, every new field empty.
    pub fn blank(network: Network) -> ChatMessage {
        ChatMessage {
            network,
            chat_id: String::new(),
            id: String::new(),
            ts: 0,
            sender: String::new(),
            from_me: false,
            text: String::new(),
            state: MessageState::Sent,
            sender_id: String::new(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
            attachments: Vec::new(),
            mentions: Vec::new(),
            mentions_me: false,
        }
    }
}

/// Connection state of one network, shown as the tab's status chip.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Backend starting, reading the store.
    Starting,
    /// Never signed in / linked, or the sign-in was lost. The TUI shows the login panel.
    NeedsLogin {
        reason: Option<String>,
    },
    /// Teams: browser sign-in open. WhatsApp: waiting for a QR or pair code.
    LoggingIn,
    /// WhatsApp only: show this QR payload for the phone to scan.
    Qr {
        code: String,
    },
    Connecting,
    /// Signed in and loaded. `live` = new messages arrive by themselves (Teams: relay socket
    /// and Graph subscription both up; WhatsApp: socket connected). `detail` says why not
    /// live, for example "relay not set up".
    Online {
        live: bool,
        detail: Option<String>,
    },
    /// Broken in a way the user must act on. The text says what to do.
    Error {
        message: String,
    },
}

/// Backend -> TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Status(Network, Status),
    /// The full chat list for a network, newest first. Replaces the sidebar.
    Chats(Network, Vec<ChatSummary>),
    /// One chat changed (new message, rename, read). Insert or replace by (network, id).
    ChatUpdated(ChatSummary),
    /// History for one chat, oldest first. Replaces what the TUI holds for that chat.
    Messages {
        network: Network,
        chat_id: String,
        messages: Vec<ChatMessage>,
    },
    /// One new or changed message. Insert or replace by (network, chat_id, id).
    Message(ChatMessage),
    /// A message sent with Command::Send was confirmed: replace `client_id` with `message`.
    MessageConfirmed {
        client_id: String,
        message: ChatMessage,
    },
    /// A message sent with Command::Send failed.
    MessageFailed {
        network: Network,
        chat_id: String,
        client_id: String,
        error: String,
    },
    /// One short line for the status bar (info or error). Also written to the log.
    Notice {
        network: Network,
        text: String,
        is_error: bool,
    },
    /// An existing message changed (edit, delete, reaction, attachment update). Insert-or-replace
    /// by (network, chat_id, id) exactly like Message, but NEVER raises a notification.
    MessageChanged(ChatMessage),
    /// Answer to LoadOlder: older messages, oldest first, possibly overlapping what the TUI
    /// holds (dedupe by id). `at_start` = nothing older exists; stop requesting for this chat.
    OlderMessages {
        network: Network,
        chat_id: String,
        messages: Vec<ChatMessage>,
        at_start: bool,
    },
    OlderFailed {
        network: Network,
        chat_id: String,
        error: String,
    },
    /// Answer to FetchMedia. `path` is inside the media cache.
    MediaReady {
        network: Network,
        chat_id: String,
        message_id: String,
        index: u32,
        path: PathBuf,
    },
    /// `fallback_url`: https URL the TUI may open in the browser instead (Teams files).
    MediaFailed {
        network: Network,
        chat_id: String,
        message_id: String,
        index: u32,
        reason: String,
        fallback_url: Option<String>,
    },
    /// Upload progress of a SendMedia. At most 4 per second per transfer.
    SendProgress {
        network: Network,
        chat_id: String,
        client_id: String,
        done: u64,
        total: u64,
    },
    /// Someone is typing (WhatsApp only). `sender` = display label.
    Typing {
        network: Network,
        chat_id: String,
        sender: String,
        active: bool,
    },
    /// Answer to LoadMembers. `error` Some = failed (members then empty).
    Members {
        network: Network,
        chat_id: String,
        members: Vec<PersonRef>,
        error: Option<String>,
    },
    /// Answer to FetchAvatars. `path` is a square PNG in the media cache; None = no photo, not
    /// allowed, or failed. The TUI does not ask again this session.
    Avatar {
        network: Network,
        person_id: String,
        path: Option<PathBuf>,
    },
    /// Sent after reaching Online and whenever a capability changes.
    Capabilities {
        network: Network,
        caps: Capabilities,
    },
}

/// TUI -> one backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Start sign-in (Teams: browser; WhatsApp: QR pairing).
    Login,
    /// Forget the sign-in / unlink this device.
    Logout,
    /// Reload the chat list from the network. User-triggered only: Teams forbids polling.
    Refresh,
    /// The user opened this chat: send its history (store first, then the network).
    OpenChat {
        chat_id: String,
    },
    /// Send text. `client_id` is a TUI-made id for the Pending message until confirmed.
    Send {
        chat_id: String,
        text: String,
        client_id: String,
        /// Reply with quote.
        reply_to: Option<QuotedRef>,
        /// Group chats only; ignored in 1:1. Each appears in `text` as "@" + name.
        mentions: Vec<PersonRef>,
    },
    MarkRead {
        chat_id: String,
    },
    /// Send a local file. The backend reads it (never the TUI), checks kind and size before
    /// any network call, and answers like Send: MessageConfirmed or MessageFailed, with
    /// SendProgress between. `kind` is the TUI's guess (Image or Document); the backend
    /// re-checks by content.
    SendMedia {
        chat_id: String,
        client_id: String,
        path: PathBuf,
        kind: MediaKind,
        caption: String,
    },
    /// Set (remove=false) or remove the signed-in user's reaction. One reaction per user per
    /// message: setting a different emoji replaces the old one.
    React {
        chat_id: String,
        message_id: String,
        emoji: String,
        remove: bool,
    },
    /// Edit an own message. Success: MessageChanged (edited=true). Failure: Notice; the
    /// stored text is unchanged.
    Edit {
        chat_id: String,
        message_id: String,
        text: String,
    },
    /// Delete an own message for everyone. Success: MessageChanged (state Deleted).
    Delete {
        chat_id: String,
        message_id: String,
    },
    /// Fetch attachment `index` of a message into the media cache. User action only.
    FetchMedia {
        chat_id: String,
        message_id: String,
        index: u32,
    },
    /// Older history before the oldest message the TUI holds. User action only.
    LoadOlder {
        chat_id: String,
        before_ts: i64,
        before_id: String,
    },
    /// Members of a group chat for @-completion.
    LoadMembers {
        chat_id: String,
    },
    /// Own typing state (WhatsApp). The TUI sends active=true at most once per 5 s per chat,
    /// and active=false on send.
    Typing {
        chat_id: String,
        active: bool,
    },
    /// Profile photos for the avatars beside messages: people of this network (Teams AAD user
    /// id, WhatsApp JID). Answered with one `Event::Avatar` per person, in any order, never
    /// silent. The TUI asks once per person per session, only for people whose avatar is on
    /// screen.
    FetchAvatars {
        people: Vec<String>,
    },
    /// Pin, mute (forever) or hide a chat, or undo it.
    SetChatFlag {
        chat_id: String,
        flag: ChatFlag,
        on: bool,
    },
    /// The user changed settings in the TUI: replace the backend's copy. Never answered.
    Settings(Arc<crate::settings::Settings>),
    /// Clean up (Teams: delete the Graph subscription) and end the task.
    Shutdown,
}

/// The contract's default answer (spec section 3) for a command a backend does not support (yet):
/// never silent. Returns the outcome events for the commands that can fail, nothing for the
/// rest. `SetChatFlag` also needs a `ChatUpdated` with the stored, unchanged flags: the backend
/// emits that itself, because only it can read its store. `Typing` is dropped silently: the
/// TUI never sends it to a network that cannot use it. Commands handled by every backend
/// (Login, Send, ...) give nothing.
pub fn unsupported(network: Network, cmd: &Command) -> Vec<Event> {
    let no = |feature: &str| format!("{feature} is not available on {}.", network.label());
    let notice = |feature: &str| Event::Notice {
        network,
        text: no(feature),
        is_error: true,
    };
    match cmd {
        Command::SendMedia {
            chat_id, client_id, ..
        } => vec![Event::MessageFailed {
            network,
            chat_id: chat_id.clone(),
            client_id: client_id.clone(),
            error: no("Sending files"),
        }],
        Command::React { .. } => vec![notice("Reactions")],
        Command::Edit { .. } => vec![notice("Editing")],
        Command::Delete { .. } => vec![notice("Deleting")],
        Command::FetchMedia {
            chat_id,
            message_id,
            index,
        } => vec![Event::MediaFailed {
            network,
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
            index: *index,
            reason: no("Media download"),
            fallback_url: None,
        }],
        Command::LoadOlder { chat_id, .. } => vec![Event::OlderFailed {
            network,
            chat_id: chat_id.clone(),
            error: no("Older history"),
        }],
        Command::LoadMembers { chat_id } => vec![Event::Members {
            network,
            chat_id: chat_id.clone(),
            members: Vec::new(),
            error: Some(no("Mentions")),
        }],
        Command::FetchAvatars { people } => people
            .iter()
            .map(|p| Event::Avatar {
                network,
                person_id: p.clone(),
                path: None,
            })
            .collect(),
        Command::SetChatFlag { flag, .. } => vec![notice(match flag {
            ChatFlag::Pinned => "Pinning",
            ChatFlag::Muted => "Muting",
            ChatFlag::Hidden => "Hiding",
        })],
        Command::Login
        | Command::Logout
        | Command::Refresh
        | Command::OpenChat { .. }
        | Command::Send { .. }
        | Command::MarkRead { .. }
        | Command::Typing { .. }
        | Command::Settings(_)
        | Command::Shutdown => Vec::new(),
    }
}

pub type EventTx = mpsc::UnboundedSender<Event>;
pub type EventRx = mpsc::UnboundedReceiver<Event>;
pub type CommandTx = mpsc::UnboundedSender<Command>;
pub type CommandRx = mpsc::UnboundedReceiver<Command>;

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Parses an RFC 3339 / ISO 8601 timestamp (Graph's `createdDateTime`) to Unix ms.
pub fn parse_iso_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_keys_round_trip() {
        for n in Network::ALL {
            assert_eq!(Network::from_key(n.key()), Some(n));
        }
        assert_eq!(Network::from_key("signal"), None);
    }

    #[test]
    fn media_kind_keys_round_trip_and_unknown_is_other() {
        for k in [
            MediaKind::Image,
            MediaKind::Video,
            MediaKind::Audio,
            MediaKind::Document,
            MediaKind::Other,
        ] {
            assert_eq!(MediaKind::from_key(k.key()), k);
        }
        assert_eq!(MediaKind::from_key("sticker"), MediaKind::Other);
    }

    #[test]
    fn fetch_ref_debug_never_prints_the_secret() {
        let r = FetchRef::new(r#"{"media_key":"s3cret"}"#);
        assert_eq!(format!("{r:?}"), "FetchRef(<redacted>)");
        let a = Attachment {
            kind: MediaKind::Image,
            name: None,
            mime: None,
            size: None,
            fetch_ref: r,
            open_url: None,
        };
        assert!(!format!("{a:?}").contains("s3cret"));
        assert!(FetchRef::default().is_empty());
    }

    #[test]
    fn mute_ends_and_forever_does_not() {
        let mut f = ChatFlags::default();
        assert!(!f.is_muted(100));
        f.muted_until = Some(200);
        assert!(f.is_muted(100));
        assert!(!f.is_muted(200), "ends at the stored time");
        f.muted_until = Some(MUTE_FOREVER);
        assert!(f.is_muted(i64::MAX - 1));
    }

    #[test]
    fn unsupported_commands_always_get_an_answer_except_typing() {
        let n = Network::Teams;
        let chat_id = || "c".to_string();
        let answered = [
            Command::SendMedia {
                chat_id: chat_id(),
                client_id: "k".into(),
                path: PathBuf::from("x"),
                kind: MediaKind::Image,
                caption: String::new(),
            },
            Command::React {
                chat_id: chat_id(),
                message_id: "m".into(),
                emoji: "+1".into(),
                remove: false,
            },
            Command::Edit {
                chat_id: chat_id(),
                message_id: "m".into(),
                text: "t".into(),
            },
            Command::Delete {
                chat_id: chat_id(),
                message_id: "m".into(),
            },
            Command::FetchMedia {
                chat_id: chat_id(),
                message_id: "m".into(),
                index: 0,
            },
            Command::LoadOlder {
                chat_id: chat_id(),
                before_ts: 0,
                before_id: String::new(),
            },
            Command::LoadMembers { chat_id: chat_id() },
            Command::SetChatFlag {
                chat_id: chat_id(),
                flag: ChatFlag::Pinned,
                on: true,
            },
            Command::FetchAvatars {
                people: vec!["p".into()],
            },
        ];
        for cmd in &answered {
            let events = unsupported(n, cmd);
            assert_eq!(events.len(), 1, "{cmd:?}");
        }
        let two = Command::FetchAvatars {
            people: vec!["a".into(), "b".into()],
        };
        assert_eq!(unsupported(n, &two).len(), 2);
        assert_eq!(
            unsupported(n, &answered[1]),
            vec![Event::Notice {
                network: n,
                text: "Reactions is not available on Teams.".into(),
                is_error: true
            }]
        );
        let typing = Command::Typing {
            chat_id: chat_id(),
            active: true,
        };
        assert!(
            unsupported(n, &typing).is_empty(),
            "typing is the one silent drop"
        );
        let settings = Command::Settings(Arc::new(crate::settings::Settings::default()));
        assert!(
            unsupported(n, &settings).is_empty(),
            "settings are never answered"
        );
    }

    #[test]
    fn blanks_are_empty() {
        let m = ChatMessage::blank(Network::Teams);
        assert_eq!(m.state, MessageState::Sent);
        assert!(m.attachments.is_empty() && m.reactions.is_empty() && m.reply_to.is_none());
        let c = ChatSummary::blank(Network::WhatsApp);
        assert_eq!(
            (c.unread, c.mentions, c.flags),
            (0, 0, ChatFlags::default())
        );
    }

    #[test]
    fn iso_parsing() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(
            parse_iso_ms("2026-11-16T01:02:03.456Z"),
            Some(1_794_790_923_456)
        );
        assert_eq!(parse_iso_ms("not a date"), None);
    }
}
