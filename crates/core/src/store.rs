//! Local copy of chats and messages in SQLite (`store.db` in the data folder).
//!
//! Why: the sidebar shows instantly on start, chats stay readable offline, and WhatsApp has
//! no "list my chats" call at all (chats arrive by history sync and new messages only).
//! Retention: text is kept forever. 1,000 messages a day is ~365k rows a year, indexed by
//! (network, chat_id, ts); reads are always bounded by a LIMIT. Media is never stored here.
//!
//! One connection behind a mutex. Every call is a few small statements, so holding the lock
//! briefly from async code is fine.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{ChatKind, ChatMessage, ChatSummary, MessageState, Network};
use crate::Result;

const SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

fn db_err(context: &str) -> impl Fn(rusqlite::Error) -> String + '_ {
    move |e| format!("local store ({context}): {e}")
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).map_err(db_err("open"))?;
        Store::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory().map_err(db_err("open"))?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err("busy timeout"))?;
        // WAL keeps a crash from corrupting the file mid-write. In-memory DBs ignore it.
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(db_err("journal mode"))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err("schema version"))?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "local store was written by a newer chatcli (schema {version}, this one knows {SCHEMA_VERSION}). Update chatcli."
            ));
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chats (
                network TEXT NOT NULL,
                id TEXT NOT NULL,
                title TEXT NOT NULL,
                kind TEXT NOT NULL,
                last_ts INTEGER,
                last_preview TEXT NOT NULL DEFAULT '',
                unread INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (network, id)
            );
            CREATE TABLE IF NOT EXISTS messages (
                network TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                id TEXT NOT NULL,
                ts INTEGER NOT NULL,
                sender TEXT NOT NULL,
                from_me INTEGER NOT NULL,
                text TEXT NOT NULL,
                state TEXT NOT NULL,
                error TEXT,
                PRIMARY KEY (network, chat_id, id)
            );
            CREATE INDEX IF NOT EXISTS messages_by_time ON messages (network, chat_id, ts);
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            PRAGMA user_version = 1;",
        )
        .map_err(db_err("schema"))?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| "local store lock poisoned".to_string())
    }

    pub fn upsert_chats(&self, chats: &[ChatSummary]) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        for c in chats {
            upsert_chat(&tx, c)?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// The network's full chat list: chats not in `chats` are removed with their messages
    /// (left, deleted or hidden on the network).
    pub fn replace_chats(&self, network: Network, chats: &[ChatSummary]) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        let existing: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT id FROM chats WHERE network = ?1")
                .map_err(db_err("list chat ids"))?;
            let rows = stmt
                .query_map(params![network.key()], |r| r.get(0))
                .map_err(db_err("list chat ids"))?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(db_err("list chat ids"))?
        };
        for id in existing
            .iter()
            .filter(|id| !chats.iter().any(|c| &c.id == *id))
        {
            tx.execute(
                "DELETE FROM messages WHERE network = ?1 AND chat_id = ?2",
                params![network.key(), id],
            )
            .map_err(db_err("delete messages"))?;
            tx.execute(
                "DELETE FROM chats WHERE network = ?1 AND id = ?2",
                params![network.key(), id],
            )
            .map_err(db_err("delete chat"))?;
        }
        for c in chats {
            upsert_chat(&tx, c)?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// Newest first; chats with no messages last.
    pub fn chats(&self, network: Network) -> Result<Vec<ChatSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, kind, last_ts, last_preview, unread FROM chats
                 WHERE network = ?1 ORDER BY last_ts IS NULL, last_ts DESC, title",
            )
            .map_err(db_err("read chats"))?;
        let rows = stmt
            .query_map(params![network.key()], |r| {
                Ok(ChatSummary {
                    network,
                    id: r.get(0)?,
                    title: r.get(1)?,
                    kind: ChatKind::from_key(&r.get::<_, String>(2)?),
                    last_ts: r.get(3)?,
                    last_preview: r.get(4)?,
                    unread: r.get(5)?,
                })
            })
            .map_err(db_err("read chats"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read chats"))
    }

    pub fn chat(&self, network: Network, chat_id: &str) -> Result<Option<ChatSummary>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, title, kind, last_ts, last_preview, unread FROM chats
             WHERE network = ?1 AND id = ?2",
            params![network.key(), chat_id],
            |r| {
                Ok(ChatSummary {
                    network,
                    id: r.get(0)?,
                    title: r.get(1)?,
                    kind: ChatKind::from_key(&r.get::<_, String>(2)?),
                    last_ts: r.get(3)?,
                    last_preview: r.get(4)?,
                    unread: r.get(5)?,
                })
            },
        )
        .optional()
        .map_err(db_err("read chat"))
    }

    pub fn set_unread(&self, network: Network, chat_id: &str, unread: u32) -> Result<()> {
        self.lock()?
            .execute(
                "UPDATE chats SET unread = ?3 WHERE network = ?1 AND id = ?2",
                params![network.key(), chat_id, unread],
            )
            .map_err(db_err("set unread"))?;
        Ok(())
    }

    pub fn upsert_messages(&self, messages: &[ChatMessage]) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        for m in messages {
            upsert_message(&tx, m)?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// Swaps a Pending message (keyed by its client id) for the confirmed one.
    pub fn confirm_message(&self, client_id: &str, message: &ChatMessage) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        tx.execute(
            "DELETE FROM messages WHERE network = ?1 AND chat_id = ?2 AND id = ?3",
            params![message.network.key(), message.chat_id, client_id],
        )
        .map_err(db_err("drop pending"))?;
        upsert_message(&tx, message)?;
        tx.commit().map_err(db_err("commit"))
    }

    /// Time of the newest message the network itself has told us about. Failed and pending
    /// sends carry the local clock and must not count, or they hide newer remote messages.
    pub fn newest_synced_ts(&self, network: Network, chat_id: &str) -> Result<Option<i64>> {
        self.lock()?
            .query_row(
                "SELECT MAX(ts) FROM messages
                 WHERE network = ?1 AND chat_id = ?2 AND state IN ('sent', 'deleted')",
                params![network.key(), chat_id],
                |r| r.get(0),
            )
            .map_err(db_err("read newest message"))
    }

    /// The newest `limit` messages of a chat, returned oldest first.
    pub fn messages(
        &self,
        network: Network,
        chat_id: &str,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, ts, sender, from_me, text, state, error FROM messages
                 WHERE network = ?1 AND chat_id = ?2 ORDER BY ts DESC, id DESC LIMIT ?3",
            )
            .map_err(db_err("read messages"))?;
        let rows = stmt
            .query_map(params![network.key(), chat_id, limit], |r| {
                let state: String = r.get(5)?;
                let error: Option<String> = r.get(6)?;
                Ok(ChatMessage {
                    network,
                    chat_id: chat_id.to_string(),
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    sender: r.get(2)?,
                    from_me: r.get(3)?,
                    text: r.get(4)?,
                    state: match state.as_str() {
                        "pending" => MessageState::Pending,
                        "failed" => MessageState::Failed(error.unwrap_or_default()),
                        "deleted" => MessageState::Deleted,
                        _ => MessageState::Sent,
                    },
                })
            })
            .map_err(db_err("read messages"))?;
        let mut out: Vec<ChatMessage> = rows
            .collect::<std::result::Result<_, _>>()
            .map_err(db_err("read messages"))?;
        out.reverse();
        Ok(out)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.lock()?
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err("read meta"))
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<()> {
        let conn = self.lock()?;
        match value {
            Some(v) => conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, v],
            ),
            None => conn.execute("DELETE FROM meta WHERE key = ?1", params![key]),
        }
        .map_err(db_err("write meta"))?;
        Ok(())
    }
}

fn upsert_chat(conn: &Connection, c: &ChatSummary) -> Result<()> {
    conn.execute(
        "INSERT INTO chats (network, id, title, kind, last_ts, last_preview, unread)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (network, id) DO UPDATE SET
            title = excluded.title, kind = excluded.kind, last_ts = excluded.last_ts,
            last_preview = excluded.last_preview, unread = excluded.unread",
        params![
            c.network.key(),
            c.id,
            c.title,
            c.kind.key(),
            c.last_ts,
            c.last_preview,
            c.unread
        ],
    )
    .map_err(db_err("write chat"))?;
    Ok(())
}

fn upsert_message(conn: &Connection, m: &ChatMessage) -> Result<()> {
    let error = match &m.state {
        MessageState::Failed(reason) => Some(reason.as_str()),
        _ => None,
    };
    conn.execute(
        "INSERT INTO messages (network, chat_id, id, ts, sender, from_me, text, state, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (network, chat_id, id) DO UPDATE SET
            ts = excluded.ts, sender = excluded.sender, from_me = excluded.from_me,
            text = excluded.text, state = excluded.state, error = excluded.error",
        params![
            m.network.key(),
            m.chat_id,
            m.id,
            m.ts,
            m.sender,
            m.from_me,
            m.text,
            m.state.key(),
            error
        ],
    )
    .map_err(db_err("write message"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, ts: Option<i64>) -> ChatSummary {
        ChatSummary {
            network: Network::Teams,
            id: id.into(),
            title: format!("chat {id}"),
            kind: ChatKind::Direct,
            last_ts: ts,
            last_preview: String::new(),
            unread: 0,
        }
    }

    fn msg(chat_id: &str, id: &str, ts: i64, state: MessageState) -> ChatMessage {
        ChatMessage {
            network: Network::Teams,
            chat_id: chat_id.into(),
            id: id.into(),
            ts,
            sender: "Jane".into(),
            from_me: false,
            text: format!("text {id}"),
            state,
        }
    }

    #[test]
    fn chats_order_newest_first_and_networks_stay_apart() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("a", Some(10)), chat("b", Some(30)), chat("c", None)])
            .unwrap();
        let mut wa = chat("a", Some(99));
        wa.network = Network::WhatsApp;
        s.upsert_chats(&[wa]).unwrap();
        let ids: Vec<_> = s
            .chats(Network::Teams)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["b", "a", "c"]);
        assert_eq!(s.chats(Network::WhatsApp).unwrap().len(), 1);
    }

    #[test]
    fn newest_synced_ignores_local_failed_and_pending_sends() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.newest_synced_ts(Network::Teams, "a").unwrap(), None);
        s.upsert_messages(&[
            msg("a", "m1", 100, MessageState::Sent),
            msg("a", "local-1", 500, MessageState::Failed("x".into())),
            msg("a", "local-2", 600, MessageState::Pending),
        ])
        .unwrap();
        assert_eq!(s.newest_synced_ts(Network::Teams, "a").unwrap(), Some(100));
    }

    #[test]
    fn chat_looks_up_one_row_by_network_and_id() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("a", Some(10))]).unwrap();
        assert_eq!(
            s.chat(Network::Teams, "a").unwrap(),
            Some(chat("a", Some(10)))
        );
        assert_eq!(s.chat(Network::Teams, "zzz").unwrap(), None);
        assert_eq!(s.chat(Network::WhatsApp, "a").unwrap(), None);
    }

    #[test]
    fn replace_chats_drops_missing_chats_and_their_messages() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("keep", Some(1)), chat("gone", Some(2))])
            .unwrap();
        s.upsert_messages(&[msg("gone", "m1", 1, MessageState::Sent)])
            .unwrap();
        s.replace_chats(Network::Teams, &[chat("keep", Some(3))])
            .unwrap();
        let ids: Vec<_> = s
            .chats(Network::Teams)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["keep"]);
        assert!(s.messages(Network::Teams, "gone", 10).unwrap().is_empty());
    }

    #[test]
    fn messages_newest_limit_returned_oldest_first() {
        let s = Store::open_in_memory().unwrap();
        let all: Vec<_> = (1..=5)
            .map(|i| msg("c", &format!("m{i}"), i, MessageState::Sent))
            .collect();
        s.upsert_messages(&all).unwrap();
        let ids: Vec<_> = s
            .messages(Network::Teams, "c", 3)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["m3", "m4", "m5"]);
    }

    #[test]
    fn pending_is_replaced_on_confirm_and_failure_reason_survives() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(&[msg("c", "client-1", 5, MessageState::Pending)])
            .unwrap();
        s.confirm_message("client-1", &msg("c", "real-1", 6, MessageState::Sent))
            .unwrap();
        let ids: Vec<_> = s
            .messages(Network::Teams, "c", 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["real-1"]);
        s.upsert_messages(&[msg(
            "c",
            "client-2",
            7,
            MessageState::Failed("403 Forbidden".into()),
        )])
        .unwrap();
        let last = s.messages(Network::Teams, "c", 1).unwrap().pop().unwrap();
        assert_eq!(last.state, MessageState::Failed("403 Forbidden".into()));
    }

    #[test]
    fn meta_set_get_clear() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.meta("k").unwrap(), None);
        s.set_meta("k", Some("v1")).unwrap();
        s.set_meta("k", Some("v2")).unwrap();
        assert_eq!(s.meta("k").unwrap().as_deref(), Some("v2"));
        s.set_meta("k", None).unwrap();
        assert_eq!(s.meta("k").unwrap(), None);
    }

    #[test]
    fn file_store_reopens_with_data() {
        let dir = std::env::temp_dir().join(format!("chatcli-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_chats(&[chat("a", Some(1))]).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.chats(Network::Teams).unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
