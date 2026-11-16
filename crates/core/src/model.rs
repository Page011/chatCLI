//! The shared vocabulary between the network backends (teams, whatsapp) and the TUI.
//!
//! Flow: each backend runs as one tokio task. It receives `Command`s from the TUI on its own
//! channel and sends `Event`s to the TUI on a shared channel. Backends never touch the
//! terminal; the TUI never touches the network.

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
    },
    MarkRead {
        chat_id: String,
    },
    /// Clean up (Teams: delete the Graph subscription) and end the task.
    Shutdown,
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
    fn iso_parsing() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(
            parse_iso_ms("2026-11-16T01:02:03.456Z"),
            Some(1_794_790_923_456)
        );
        assert_eq!(parse_iso_ms("not a date"), None);
    }
}
