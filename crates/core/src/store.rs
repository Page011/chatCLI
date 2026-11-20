//! Local copy of chats and messages in SQLite (`store.db` in the data folder).
//!
//! Why: the sidebar shows instantly on start, chats stay readable offline, and WhatsApp has
//! no "list my chats" call at all (chats arrive by history sync and new messages only).
//! Retention: text is kept forever. 1,000 messages a day is ~365k rows a year, indexed by
//! (network, chat_id, ts); every read has a LIMIT or a full primary key. Media bytes are never
//! stored here (only their metadata and fetch recipe); they live in the media cache.
//!
//! Schema v2 adds attachments, reactions, chat flags, members, recent emoji and an FTS5 index
//! over message text. One connection behind a mutex. Schema v3 and v4 add read and delivery
//! marks (how far each other person has read, or received, your messages). Every call is a few small statements, so
//! holding the lock briefly from async code is fine.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use crate::model::{
    Attachment, ChatFlags, ChatKind, ChatMessage, ChatSummary, FetchRef, MediaKind, MessageState,
    Network, PersonRef, QuotedRef, Reaction,
};
use crate::Result;

const SCHEMA_VERSION: i64 = 4;

/// `reactions.reactor` for the signed-in user.
pub const REACTOR_ME: &str = "@me";

/// `delivered_marks.reader` of a chat's delivery baseline (never a real user part).
pub const BASELINE_READER: &str = "";

/// Members kept per chat; a bigger group is cut here so one chat cannot grow the table forever.
const MAX_MEMBERS: usize = 1024;
/// Receipt marks read per chat: a full group's members under both of their addresses (phone
/// number and LID), with room for readers who have left and the delivery baseline, so no
/// current member's mark is cut off before the group is worked out.
const MAX_MARKS: usize = 4 * MAX_MEMBERS;
const MAX_RECENT_EMOJI: u32 = 30;

/// FTS5 snippet markers around a hit. Control characters never occur in message text we store
/// (Graph and WhatsApp text are printable), and `search` strips them before returning.
const HIT_START: char = '\x02';
const HIT_END: char = '\x03';

/// Schema 0 -> 1: the tables the first release shipped. Kept as it was so a fresh database
/// walks the same path as a real old one.
const V1_SQL: &str = "CREATE TABLE IF NOT EXISTS chats (
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
    CREATE TABLE IF NOT EXISTS names (
        network TEXT NOT NULL,
        kind TEXT NOT NULL,
        jid_user TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (network, kind, jid_user)
    );";

/// Schema 1 -> 2. The messages table is rebuilt to get a stable integer rowid: FTS5 with
/// external content needs one, and an implicit rowid may change on VACUUM.
const V2_SQL: &str = "CREATE TABLE messages_v2 (
        rid INTEGER PRIMARY KEY,
        network TEXT NOT NULL, chat_id TEXT NOT NULL, id TEXT NOT NULL,
        ts INTEGER NOT NULL, sender TEXT NOT NULL, from_me INTEGER NOT NULL,
        text TEXT NOT NULL, state TEXT NOT NULL, error TEXT,
        sender_id TEXT NOT NULL DEFAULT '',
        edited INTEGER NOT NULL DEFAULT 0,
        quote_id TEXT, quote_sender TEXT, quote_sender_id TEXT, quote_preview TEXT,
        mentions TEXT NOT NULL DEFAULT '',
        mentions_me INTEGER NOT NULL DEFAULT 0,
        UNIQUE (network, chat_id, id)
    );
    INSERT INTO messages_v2 (network, chat_id, id, ts, sender, from_me, text, state, error)
        SELECT network, chat_id, id, ts, sender, from_me, text, state, error FROM messages;
    DROP TABLE messages;
    ALTER TABLE messages_v2 RENAME TO messages;
    CREATE INDEX messages_by_time ON messages (network, chat_id, ts);

    ALTER TABLE chats ADD COLUMN mentions INTEGER NOT NULL DEFAULT 0;

    CREATE TABLE attachments (
        network TEXT NOT NULL, chat_id TEXT NOT NULL, message_id TEXT NOT NULL, idx INTEGER NOT NULL,
        kind TEXT NOT NULL, name TEXT, mime TEXT, size INTEGER,
        fetch_ref TEXT NOT NULL, open_url TEXT,
        PRIMARY KEY (network, chat_id, message_id, idx)
    );
    CREATE TABLE reactions (
        network TEXT NOT NULL, chat_id TEXT NOT NULL, message_id TEXT NOT NULL,
        reactor TEXT NOT NULL,
        emoji TEXT NOT NULL, from_me INTEGER NOT NULL, ts INTEGER NOT NULL,
        PRIMARY KEY (network, chat_id, message_id, reactor)
    );
    CREATE TABLE chat_flags (
        network TEXT NOT NULL, chat_id TEXT NOT NULL,
        pinned INTEGER NOT NULL DEFAULT 0, muted_until INTEGER, hidden INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (network, chat_id)
    );
    CREATE TABLE members (
        network TEXT NOT NULL, chat_id TEXT NOT NULL, member_id TEXT NOT NULL, name TEXT NOT NULL,
        PRIMARY KEY (network, chat_id, member_id)
    );
    CREATE TABLE emoji_recent (emoji TEXT PRIMARY KEY, used_at INTEGER NOT NULL);

    CREATE VIRTUAL TABLE messages_fts USING fts5(
        text, content='messages', content_rowid='rid', tokenize='unicode61 remove_diacritics 2');
    CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, text) VALUES (new.rid, new.text); END;
    CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.rid, old.text); END;
    CREATE TRIGGER messages_au AFTER UPDATE OF text ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.rid, old.text);
        INSERT INTO messages_fts(rowid, text) VALUES (new.rid, new.text); END;
    INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');";

/// Schema 2 -> 3: per reader, the time of your newest message they have read (WhatsApp read
/// receipts). `reader` is the WhatsApp user part (phone number or LID).
const V3_SQL: &str = "CREATE TABLE read_marks (
        network TEXT NOT NULL, chat_id TEXT NOT NULL, reader TEXT NOT NULL, ts INTEGER NOT NULL,
        PRIMARY KEY (network, chat_id, reader)
    );";

/// Schema 3 -> 4: per reader, the time of your newest message that reached their phone
/// (WhatsApp delivery receipts). The reader `BASELINE_READER` holds the time before which every
/// message of yours counts as delivered (set once, at your first message in the chat after this
/// arrived), so messages from before delivery was tracked are never shown as undelivered.
const V4_SQL: &str = "CREATE TABLE delivered_marks (
        network TEXT NOT NULL, chat_id TEXT NOT NULL, reader TEXT NOT NULL, ts INTEGER NOT NULL,
        PRIMARY KEY (network, chat_id, reader)
    );";

/// Columns of one message row, in the order `message_from_row` reads them.
const MESSAGE_COLS: &str = "id, ts, sender, from_me, text, state, error, sender_id, edited,
    quote_id, quote_sender, quote_sender_id, quote_preview, mentions, mentions_me";

/// One chat row plus its flags, in the order `chat_from_row` reads them.
const CHAT_SELECT: &str = "SELECT c.id, c.title, c.kind, c.last_ts, c.last_preview, c.unread,
    c.mentions, COALESCE(f.pinned, 0), f.muted_until, COALESCE(f.hidden, 0)
    FROM chats c LEFT JOIN chat_flags f ON f.network = c.network AND f.chat_id = c.id";

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// One reactor's reaction, as Teams hands over the full list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionRow {
    pub reactor: String,
    pub emoji: String,
    pub from_me: bool,
}

/// A piece of a search snippet: `hit` pieces are the matched words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub network: Network,
    pub chat_id: String,
    pub chat_title: String,
    pub message_id: String,
    pub ts: i64,
    pub sender: String,
    pub snippet: Vec<SnippetPart>,
}

fn db_err(context: &str) -> impl Fn(rusqlite::Error) -> String + '_ {
    move |e| format!("local store ({context}): {e}")
}

fn newer_error(version: i64) -> String {
    format!(
        "local store was written by a newer chatcli (schema {version}, this one knows {SCHEMA_VERSION}). Update chatcli."
    )
}

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// Forward-only migrations, 0 -> 1 -> 2 -> 3 -> 4, in one `BEGIN IMMEDIATE` transaction. The version is
/// re-read inside it, so two processes opening at once migrate once: the second waits on
/// `busy_timeout`, then finds the new version and does nothing.
///
/// The version is set inside the transaction (SQLite rolls `user_version` back with it) rather
/// than after the commit: a crash between commit and a separate version write would leave a v2
/// layout labelled v1, and the next start would fail trying to build the tables again.
fn migrate(conn: &mut Connection) -> Result<()> {
    let fail = |e: rusqlite::Error| {
        format!(
            "Could not upgrade the local store ({e}). Your data is unchanged: report this and use the previous chatcli."
        )
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    let version = user_version(&tx).map_err(fail)?;
    if version > SCHEMA_VERSION {
        return Err(newer_error(version));
    }
    if version < 1 {
        tx.execute_batch(V1_SQL).map_err(fail)?;
    }
    if version < 2 {
        tx.execute_batch(V2_SQL).map_err(fail)?;
    }
    if version < 3 {
        tx.execute_batch(V3_SQL).map_err(fail)?;
    }
    if version < 4 {
        tx.execute_batch(V4_SQL).map_err(fail)?;
    }
    if version < SCHEMA_VERSION {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(fail)?;
    }
    tx.commit().map_err(fail)
}

/// WAL keeps a crash from corrupting the file mid-write. In-memory DBs ignore it.
/// Switching the journal mode does not wait on `busy_timeout`: a second process opening a
/// fresh file at the same moment gets "database is locked" at once, so retry for a few seconds.
fn enable_wal(conn: &Connection) -> Result<()> {
    let mut tries = 0;
    loop {
        match conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if tries < 50
                    && matches!(
                        e.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(db_err("journal mode")(e)),
        }
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).map_err(db_err("open"))?;
        Store::init(conn)
    }

    /// Opens an existing store for reading only (`doctor`, `search`): never creates, never
    /// migrates, so running them cannot upgrade a file a 0.1 client may still be using. An older
    /// layout answers with one sentence that says to start chatcli once.
    pub fn open_read_only(path: &Path) -> Result<Store> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_err("open"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err("busy timeout"))?;
        let version = user_version(&conn).map_err(db_err("schema version"))?;
        if version > SCHEMA_VERSION {
            return Err(newer_error(version));
        }
        if version < SCHEMA_VERSION {
            return Err(
                "The store is not upgraded yet: start chatcli once, then run this again.".into(),
            );
        }
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory().map_err(db_err("open"))?)
    }

    fn init(mut conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err("busy timeout"))?;
        enable_wal(&conn)?;
        let version = user_version(&conn).map_err(db_err("schema version"))?;
        if version > SCHEMA_VERSION {
            return Err(newer_error(version));
        }
        if version < SCHEMA_VERSION {
            migrate(&mut conn)?;
        }
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

    /// The network's full chat list: chats not in `chats` are removed with their messages,
    /// attachments, reactions, members and read marks (left or deleted on the network). Their
    /// chat flags stay: a chat that comes back keeps its pin and mute.
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
            for (table, column) in [
                ("messages", "chat_id"),
                ("attachments", "chat_id"),
                ("reactions", "chat_id"),
                ("members", "chat_id"),
                ("read_marks", "chat_id"),
                ("delivered_marks", "chat_id"),
                ("chats", "id"),
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE network = ?1 AND {column} = ?2"),
                    params![network.key(), id],
                )
                .map_err(db_err("delete chat"))?;
            }
        }
        for c in chats {
            upsert_chat(&tx, c)?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// Newest first; chats with no messages last. Pinned chats are NOT first here: the TUI sorts.
    pub fn chats(&self, network: Network) -> Result<Vec<ChatSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "{CHAT_SELECT} WHERE c.network = ?1
                 ORDER BY c.last_ts IS NULL, c.last_ts DESC, c.title"
            ))
            .map_err(db_err("read chats"))?;
        let rows = stmt
            .query_map(params![network.key()], |r| chat_from_row(r, network))
            .map_err(db_err("read chats"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read chats"))
    }

    pub fn chat(&self, network: Network, chat_id: &str) -> Result<Option<ChatSummary>> {
        let conn = self.lock()?;
        read_chat(&conn, network, chat_id)
    }

    /// Sets the unread count. Zero also clears the mention counter.
    pub fn set_unread(&self, network: Network, chat_id: &str, unread: u32) -> Result<()> {
        self.lock()?
            .execute(
                "UPDATE chats SET unread = ?3, mentions = CASE WHEN ?3 = 0 THEN 0 ELSE mentions END
                 WHERE network = ?1 AND id = ?2",
                params![network.key(), chat_id, unread],
            )
            .map_err(db_err("set unread"))?;
        Ok(())
    }

    /// +1 on the chat's unread-mention counter (a live message mentioned the user).
    pub fn bump_mentions(&self, network: Network, chat_id: &str) -> Result<()> {
        self.lock()?
            .execute(
                "UPDATE chats SET mentions = mentions + 1 WHERE network = ?1 AND id = ?2",
                params![network.key(), chat_id],
            )
            .map_err(db_err("bump mentions"))?;
        Ok(())
    }

    /// Folds chat `from` into chat `to` (the same conversation under a second id) in one
    /// transaction: messages move over (a message id both chats hold keeps `to`'s row, with its
    /// attachments and reactions), attachments, reactions, members, read marks and chat flags
    /// move too (`to` keeps its own on conflict), the chat rows merge (newest time and its preview win, unread
    /// and mentions add up, `to`'s title is kept) and the `from` row is deleted. `to` is created
    /// from `from` when it does not exist. Returns false when `from` does not exist.
    pub fn rename_chat(&self, network: Network, from: &str, to: &str) -> Result<bool> {
        if from == to {
            return Ok(false);
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        let Some(old) = read_chat(&tx, network, from)? else {
            return Ok(false);
        };
        let merged = match read_chat(&tx, network, to)? {
            None => ChatSummary {
                id: to.to_string(),
                ..old.clone()
            },
            Some(mut keep) => {
                if old.last_ts > keep.last_ts {
                    keep.last_ts = old.last_ts;
                    keep.last_preview = old.last_preview.clone();
                }
                keep.unread = keep.unread.saturating_add(old.unread);
                keep.mentions = keep.mentions.saturating_add(old.mentions);
                keep
            }
        };
        let net = network.key();
        // Rows of a message `to` already holds are dropped first, so the move below never mixes
        // two versions of one message's attachments or reactions.
        for table in ["attachments", "reactions"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE network = ?1 AND chat_id = ?2 AND message_id IN
                     (SELECT id FROM messages WHERE network = ?1 AND chat_id = ?3)"
                ),
                params![net, from, to],
            )
            .map_err(db_err("drop duplicate message rows"))?;
        }
        // A reader's marks only move forward, so one both chats hold keeps the later of the two
        // (the move below would keep `to`'s alone). The delivery baseline is not a reader: `to`
        // keeps its own, as the move does.
        for table in ["read_marks", "delivered_marks"] {
            tx.execute(
                &format!(
                    "INSERT INTO {table} (network, chat_id, reader, ts)
                     SELECT network, ?3, reader, ts FROM {table}
                     WHERE network = ?1 AND chat_id = ?2 AND reader != ?4
                     ON CONFLICT (network, chat_id, reader)
                     DO UPDATE SET ts = MAX({table}.ts, excluded.ts)"
                ),
                params![net, from, to, BASELINE_READER],
            )
            .map_err(db_err("merge receipt marks"))?;
        }
        for (table, column) in [
            ("messages", "chat_id"),
            ("attachments", "chat_id"),
            ("reactions", "chat_id"),
            ("members", "chat_id"),
            ("read_marks", "chat_id"),
            ("delivered_marks", "chat_id"),
            ("chat_flags", "chat_id"),
        ] {
            tx.execute(
                &format!(
                    "UPDATE OR IGNORE {table} SET {column} = ?3 WHERE network = ?1 AND {column} = ?2"
                ),
                params![net, from, to],
            )
            .map_err(db_err("move chat rows"))?;
            // Rows the update skipped are duplicates of rows `to` already holds.
            tx.execute(
                &format!("DELETE FROM {table} WHERE network = ?1 AND {column} = ?2"),
                params![net, from],
            )
            .map_err(db_err("drop duplicate chat rows"))?;
        }
        tx.execute(
            "DELETE FROM chats WHERE network = ?1 AND id = ?2",
            params![net, from],
        )
        .map_err(db_err("delete chat"))?;
        upsert_chat(&tx, &merged)?;
        // upsert_chat keeps the stored mention counter; the merge sums the two.
        tx.execute(
            "UPDATE chats SET mentions = MIN(?3, unread) WHERE network = ?1 AND id = ?2",
            params![net, to, merged.mentions],
        )
        .map_err(db_err("merge mentions"))?;
        tx.commit().map_err(db_err("commit"))?;
        Ok(true)
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
        for table in ["messages", "attachments", "reactions"] {
            let key = if table == "messages" {
                "id"
            } else {
                "message_id"
            };
            tx.execute(
                &format!("DELETE FROM {table} WHERE network = ?1 AND chat_id = ?2 AND {key} = ?3"),
                params![message.network.key(), message.chat_id, client_id],
            )
            .map_err(db_err("drop pending"))?;
        }
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
        let mut out = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {MESSAGE_COLS} FROM messages
                     WHERE network = ?1 AND chat_id = ?2 ORDER BY ts DESC, id DESC LIMIT ?3"
                ))
                .map_err(db_err("read messages"))?;
            let rows = stmt
                .query_map(params![network.key(), chat_id, limit], |r| {
                    message_from_row(r, network, chat_id)
                })
                .map_err(db_err("read messages"))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err("read messages"))?
        };
        out.reverse();
        hydrate(&conn, &mut out)?;
        Ok(out)
    }

    /// One message by its full key, with attachments and reactions.
    pub fn message(
        &self,
        network: Network,
        chat_id: &str,
        id: &str,
    ) -> Result<Option<ChatMessage>> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLS} FROM messages
                     WHERE network = ?1 AND chat_id = ?2 AND id = ?3"
                ),
                params![network.key(), chat_id, id],
                |r| message_from_row(r, network, chat_id),
            )
            .optional()
            .map_err(db_err("read message"))?;
        let Some(msg) = found else { return Ok(None) };
        let mut one = [msg];
        hydrate(&conn, &mut one)?;
        let [msg] = one;
        Ok(Some(msg))
    }

    /// Messages strictly before `(before_ts, before_id)` in `(ts, id)` order: the newest `limit`
    /// of them, returned oldest first. `limit` is clamped to 1..=200.
    pub fn messages_before(
        &self,
        network: Network,
        chat_id: &str,
        before_ts: i64,
        before_id: &str,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        let limit = limit.clamp(1, 200);
        let conn = self.lock()?;
        let mut out = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {MESSAGE_COLS} FROM messages
                     WHERE network = ?1 AND chat_id = ?2 AND (ts < ?3 OR (ts = ?3 AND id < ?4))
                     ORDER BY ts DESC, id DESC LIMIT ?5"
                ))
                .map_err(db_err("read older messages"))?;
            let rows = stmt
                .query_map(
                    params![network.key(), chat_id, before_ts, before_id, limit],
                    |r| message_from_row(r, network, chat_id),
                )
                .map_err(db_err("read older messages"))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err("read older messages"))?
        };
        out.reverse();
        hydrate(&conn, &mut out)?;
        Ok(out)
    }

    /// Messages from `from` (included) up to `to` (excluded; `None` = up to the newest), both
    /// `(ts, id)`: the oldest `limit` of them, oldest first. A jump to a search hit reads the hit
    /// and what follows it, then the next stretch each time the user moves down towards the
    /// messages the TUI already holds. `limit` is clamped to 1..=200.
    pub fn messages_range(
        &self,
        network: Network,
        chat_id: &str,
        from: (i64, &str),
        to: Option<(i64, &str)>,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        let limit = limit.clamp(1, 200);
        let (to_ts, to_id) = to.unzip();
        let conn = self.lock()?;
        let mut out = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {MESSAGE_COLS} FROM messages
                     WHERE network = ?1 AND chat_id = ?2 AND (ts > ?3 OR (ts = ?3 AND id >= ?4))
                       AND (?5 IS NULL OR ts < ?5 OR (ts = ?5 AND id < ?6))
                     ORDER BY ts, id LIMIT ?7"
                ))
                .map_err(db_err("read messages"))?;
            let rows = stmt
                .query_map(
                    params![network.key(), chat_id, from.0, from.1, to_ts, to_id, limit],
                    |r| message_from_row(r, network, chat_id),
                )
                .map_err(db_err("read messages"))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err("read messages"))?
        };
        hydrate(&conn, &mut out)?;
        Ok(out)
    }

    /// The oldest stored message (`(ts, id)` order). WhatsApp needs it to ask for older history.
    pub fn oldest_message(&self, network: Network, chat_id: &str) -> Result<Option<ChatMessage>> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLS} FROM messages
                     WHERE network = ?1 AND chat_id = ?2 ORDER BY ts, id LIMIT 1"
                ),
                params![network.key(), chat_id],
                |r| message_from_row(r, network, chat_id),
            )
            .optional()
            .map_err(db_err("read oldest message"))?;
        let Some(msg) = found else { return Ok(None) };
        let mut one = [msg];
        hydrate(&conn, &mut one)?;
        let [msg] = one;
        Ok(Some(msg))
    }

    /// Sets (Some) or clears (None) one reactor's reaction. One reaction per reactor: a new
    /// emoji replaces the old one. Returns false (and writes nothing) when the target message
    /// is not stored.
    // The argument list is the frozen 0.2 contract signature.
    #[allow(clippy::too_many_arguments)]
    pub fn set_reaction(
        &self,
        network: Network,
        chat_id: &str,
        message_id: &str,
        reactor: &str,
        from_me: bool,
        emoji: Option<&str>,
        ts: i64,
    ) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        if !message_exists(&tx, network, chat_id, message_id)? {
            return Ok(false);
        }
        match emoji {
            Some(emoji) => tx.execute(
                "INSERT INTO reactions (network, chat_id, message_id, reactor, emoji, from_me, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (network, chat_id, message_id, reactor) DO UPDATE SET
                    emoji = excluded.emoji, from_me = excluded.from_me, ts = excluded.ts",
                params![
                    network.key(),
                    chat_id,
                    message_id,
                    reactor,
                    emoji,
                    from_me,
                    ts
                ],
            ),
            None => tx.execute(
                "DELETE FROM reactions
                 WHERE network = ?1 AND chat_id = ?2 AND message_id = ?3 AND reactor = ?4",
                params![network.key(), chat_id, message_id, reactor],
            ),
        }
        .map_err(db_err("write reaction"))?;
        tx.commit().map_err(db_err("commit"))?;
        Ok(true)
    }

    /// Replaces all reactions of one message (Teams gives the full list each time). A message
    /// that is not stored gets nothing: the rows would belong to no message.
    pub fn replace_reactions(
        &self,
        network: Network,
        chat_id: &str,
        message_id: &str,
        rows: &[ReactionRow],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        if !message_exists(&tx, network, chat_id, message_id)? {
            return Ok(());
        }
        tx.execute(
            "DELETE FROM reactions WHERE network = ?1 AND chat_id = ?2 AND message_id = ?3",
            params![network.key(), chat_id, message_id],
        )
        .map_err(db_err("clear reactions"))?;
        let now = crate::model::now_ms();
        for r in rows {
            tx.execute(
                "INSERT OR REPLACE INTO reactions
                    (network, chat_id, message_id, reactor, emoji, from_me, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    network.key(),
                    chat_id,
                    message_id,
                    r.reactor,
                    r.emoji,
                    r.from_me,
                    now
                ],
            )
            .map_err(db_err("write reaction"))?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    pub fn chat_flags(&self, network: Network, chat_id: &str) -> Result<ChatFlags> {
        self.lock()?
            .query_row(
                "SELECT pinned, muted_until, hidden FROM chat_flags
                 WHERE network = ?1 AND chat_id = ?2",
                params![network.key(), chat_id],
                |r| {
                    Ok(ChatFlags {
                        pinned: r.get(0)?,
                        muted_until: r.get(1)?,
                        hidden: r.get(2)?,
                    })
                },
            )
            .optional()
            .map(Option::unwrap_or_default)
            .map_err(db_err("read chat flags"))
    }

    pub fn set_chat_flags(&self, network: Network, chat_id: &str, flags: ChatFlags) -> Result<()> {
        self.lock()?
            .execute(
                "INSERT INTO chat_flags (network, chat_id, pinned, muted_until, hidden)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (network, chat_id) DO UPDATE SET
                    pinned = excluded.pinned, muted_until = excluded.muted_until,
                    hidden = excluded.hidden",
                params![
                    network.key(),
                    chat_id,
                    flags.pinned,
                    flags.muted_until,
                    flags.hidden
                ],
            )
            .map_err(db_err("write chat flags"))?;
        Ok(())
    }

    /// Replaces the cached member list of a chat (capped at 1024 rows; the extra are dropped).
    pub fn replace_members(
        &self,
        network: Network,
        chat_id: &str,
        members: &[PersonRef],
    ) -> Result<()> {
        if members.len() > MAX_MEMBERS {
            crate::log::info(
                "store",
                &format!(
                    "member list cut from {} to {MAX_MEMBERS} for one chat",
                    members.len()
                ),
            );
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        tx.execute(
            "DELETE FROM members WHERE network = ?1 AND chat_id = ?2",
            params![network.key(), chat_id],
        )
        .map_err(db_err("clear members"))?;
        for m in members.iter().take(MAX_MEMBERS) {
            tx.execute(
                "INSERT OR REPLACE INTO members (network, chat_id, member_id, name)
                 VALUES (?1, ?2, ?3, ?4)",
                params![network.key(), chat_id, m.id, m.name],
            )
            .map_err(db_err("write member"))?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// Ordered by name. `limit` is clamped to 1..=1024.
    pub fn members(&self, network: Network, chat_id: &str, limit: u32) -> Result<Vec<PersonRef>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT member_id, name FROM members WHERE network = ?1 AND chat_id = ?2
                 ORDER BY name, member_id LIMIT ?3",
            )
            .map_err(db_err("read members"))?;
        let rows = stmt
            .query_map(
                params![network.key(), chat_id, limit.clamp(1, MAX_MEMBERS as u32)],
                |r| {
                    Ok(PersonRef {
                        id: r.get(0)?,
                        name: r.get(1)?,
                    })
                },
            )
            .map_err(db_err("read members"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read members"))
    }

    /// Records that `reader` has read your messages up to `ts`. A mark only moves forward:
    /// returns true when it did (a new reader, or a later `ts`).
    pub fn advance_read_mark(
        &self,
        network: Network,
        chat_id: &str,
        reader: &str,
        ts: i64,
    ) -> Result<bool> {
        self.advance_mark("read_marks", network, chat_id, reader, ts)
    }

    /// Records that your messages up to `ts` reached `reader`'s phone. Forward only, like
    /// `advance_read_mark`. A read mark counts as delivered too; callers combine the two.
    pub fn advance_delivered_mark(
        &self,
        network: Network,
        chat_id: &str,
        reader: &str,
        ts: i64,
    ) -> Result<bool> {
        self.advance_mark("delivered_marks", network, chat_id, reader, ts)
    }

    /// Sets the chat's delivery baseline (`BASELINE_READER`) when it has none: your messages
    /// up to `ts` count as delivered. Returns true when it was set now. Never moves it later,
    /// which would hide a message that really is undelivered.
    pub fn set_delivery_baseline(&self, network: Network, chat_id: &str, ts: i64) -> Result<bool> {
        let n = self
            .lock()?
            .execute(
                "INSERT OR IGNORE INTO delivered_marks (network, chat_id, reader, ts)
                 VALUES (?1, ?2, ?3, ?4)",
                params![network.key(), chat_id, BASELINE_READER, ts],
            )
            .map_err(db_err("write delivery baseline"))?;
        Ok(n > 0)
    }

    /// Every reader's read mark in one chat, as (reader, ts), ordered by reader. At most
    /// `MAX_MARKS` rows.
    pub fn read_marks(&self, network: Network, chat_id: &str) -> Result<Vec<(String, i64)>> {
        self.marks("read_marks", network, chat_id)
    }

    /// Every reader's delivery mark in one chat, the baseline (reader `BASELINE_READER`)
    /// included, ordered by reader. At most `MAX_MARKS` rows.
    pub fn delivered_marks(&self, network: Network, chat_id: &str) -> Result<Vec<(String, i64)>> {
        self.marks("delivered_marks", network, chat_id)
    }

    /// `table` is one of the two mark tables (a constant, never user input).
    fn advance_mark(
        &self,
        table: &str,
        network: Network,
        chat_id: &str,
        reader: &str,
        ts: i64,
    ) -> Result<bool> {
        let n = self
            .lock()?
            .execute(
                &format!(
                    "INSERT INTO {table} (network, chat_id, reader, ts) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (network, chat_id, reader) DO UPDATE SET ts = excluded.ts
                     WHERE excluded.ts > {table}.ts"
                ),
                params![network.key(), chat_id, reader, ts],
            )
            .map_err(db_err("write receipt mark"))?;
        Ok(n > 0)
    }

    fn marks(&self, table: &str, network: Network, chat_id: &str) -> Result<Vec<(String, i64)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT reader, ts FROM {table} WHERE network = ?1 AND chat_id = ?2
                 ORDER BY reader LIMIT ?3"
            ))
            .map_err(db_err("read receipt marks"))?;
        let rows = stmt
            .query_map(params![network.key(), chat_id, MAX_MARKS as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(db_err("read receipt marks"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read receipt marks"))
    }

    /// Moves `emoji` to the front of the recents and keeps the newest 30.
    pub fn touch_recent_emoji(&self, emoji: &str, now_ms: i64) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        // Delete then insert, so the row gets a fresh rowid: two touches in one millisecond
        // still order the later one first.
        tx.execute("DELETE FROM emoji_recent WHERE emoji = ?1", params![emoji])
            .map_err(db_err("write recent emoji"))?;
        tx.execute(
            "INSERT INTO emoji_recent (emoji, used_at) VALUES (?1, ?2)",
            params![emoji, now_ms],
        )
        .map_err(db_err("write recent emoji"))?;
        tx.execute(
            "DELETE FROM emoji_recent WHERE emoji NOT IN
             (SELECT emoji FROM emoji_recent ORDER BY used_at DESC, rowid DESC LIMIT ?1)",
            params![MAX_RECENT_EMOJI],
        )
        .map_err(db_err("trim recent emoji"))?;
        tx.commit().map_err(db_err("commit"))
    }

    /// Newest first. `limit` is clamped to 1..=30.
    pub fn recent_emoji(&self, limit: u32) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT emoji FROM emoji_recent ORDER BY used_at DESC, rowid DESC LIMIT ?1")
            .map_err(db_err("read recent emoji"))?;
        let rows = stmt
            .query_map(params![limit.clamp(1, MAX_RECENT_EMOJI)], |r| r.get(0))
            .map_err(db_err("read recent emoji"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read recent emoji"))
    }

    /// Full-text search over message text, newest first. `network` None searches both.
    /// `limit` is clamped to 1..=200. An empty or punctuation-only query gives no hits.
    /// Deleted messages never match. A query FTS5 cannot read is an error sentence, never a panic.
    pub fn search(
        &self,
        query: &str,
        network: Option<Network>,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        let Some(expr) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let unreadable =
            |_: rusqlite::Error| "Search could not read that query: try plain words.".to_string();
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT m.network, m.chat_id, COALESCE(c.title, ''), m.id, m.ts, m.sender,
                        snippet(messages_fts, 0, char(2), char(3), '…', 12)
                 FROM messages_fts JOIN messages m ON m.rid = messages_fts.rowid
                 LEFT JOIN chats c ON c.network = m.network AND c.id = m.chat_id
                 WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR m.network = ?2)
                   AND m.state != 'deleted'
                 ORDER BY m.ts DESC LIMIT ?3",
            )
            .map_err(unreadable)?;
        let rows = stmt
            .query_map(
                params![expr, network.map(Network::key), limit.clamp(1, 200)],
                |r| {
                    let net: String = r.get(0)?;
                    let snippet: String = r.get(6)?;
                    Ok((
                        net,
                        SearchHit {
                            network: Network::Teams,
                            chat_id: r.get(1)?,
                            chat_title: r.get(2)?,
                            message_id: r.get(3)?,
                            ts: r.get(4)?,
                            sender: r.get(5)?,
                            snippet: split_snippet(&snippet),
                        },
                    ))
                },
            )
            .map_err(unreadable)?;
        let mut hits = Vec::new();
        for row in rows {
            let (net, mut hit) = row.map_err(unreadable)?;
            // A row of an unknown network key (a newer chatcli's) cannot be shown: skip it.
            if let Some(n) = Network::from_key(&net) {
                hit.network = n;
                hits.push(hit);
            }
        }
        Ok(hits)
    }

    /// Incoming senders of a chat with how many messages each wrote (most first). Lets a caller
    /// recover a contact's name from the labels already stored on their messages.
    pub fn sender_counts(&self, network: Network, chat_id: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT sender, COUNT(*) AS n FROM messages
                 WHERE network = ?1 AND chat_id = ?2 AND from_me = 0
                 GROUP BY sender ORDER BY n DESC, sender",
            )
            .map_err(db_err("read senders"))?;
        let rows = stmt
            .query_map(params![network.key(), chat_id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
            })
            .map_err(db_err("read senders"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read senders"))
    }

    /// Saves `(kind, jid_user, value)` name rows; a repeated key keeps the newest value.
    pub fn upsert_names(&self, network: Network, rows: &[(String, String, String)]) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        for (kind, user, value) in rows {
            tx.execute(
                "INSERT INTO names (network, kind, jid_user, value) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (network, kind, jid_user) DO UPDATE SET value = excluded.value",
                params![network.key(), kind, user, value],
            )
            .map_err(db_err("write name"))?;
        }
        tx.commit().map_err(db_err("commit"))
    }

    /// Every saved `(kind, jid_user, value)` name row of a network.
    pub fn names(&self, network: Network) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT kind, jid_user, value FROM names WHERE network = ?1")
            .map_err(db_err("read names"))?;
        let rows = stmt
            .query_map(params![network.key()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(db_err("read names"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_err("read names"))
    }

    /// Deletes every saved contact name and id link of a network (address-book names and phone
    /// numbers). WhatsApp logout calls this, so unlinking leaves none of them behind. Returns
    /// the rows removed.
    pub fn clear_names(&self, network: Network) -> Result<u64> {
        let n = self
            .lock()?
            .execute(
                "DELETE FROM names WHERE network = ?1",
                params![network.key()],
            )
            .map_err(db_err("clear names"))?;
        Ok(n as u64)
    }

    /// Ties the cached data of `network` to the signed-in account. Rows keep account-relative
    /// facts (`from_me`, unread, flags), so a different account on the same chats would show the
    /// old account's messages as "You". The first call only records `account`. A later call
    /// with another id deletes the network's chats, messages, attachments, reactions, members,
    /// read marks, flags, names and saved older-history cursors, records the new id, and returns
    /// true (the caller reloads from the network). Same id: false, nothing changes.
    pub fn bind_account(&self, network: Network, account: &str) -> Result<bool> {
        let key = format!("{}.account", network.key());
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        let stored: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err("read account"))?;
        let switched = stored.as_deref().is_some_and(|s| s != account);
        if switched {
            for table in [
                "messages",
                "attachments",
                "reactions",
                "members",
                "read_marks",
                "delivered_marks",
                "chat_flags",
                "chats",
                "names",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE network = ?1"),
                    params![network.key()],
                )
                .map_err(db_err("clear account data"))?;
            }
            tx.execute(
                "DELETE FROM meta WHERE key LIKE ?1",
                params![format!("{}.older.%", network.key())],
            )
            .map_err(db_err("clear account data"))?;
        }
        if stored.as_deref() != Some(account) {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, account],
            )
            .map_err(db_err("write account"))?;
        }
        tx.commit().map_err(db_err("commit"))?;
        Ok(switched)
    }

    /// Moves every message dated later than `now_ms + skew_ms` back to the newest believable time
    /// of its chat (the chat's newest message that is not in the future, else `unknown_ms`),
    /// keeping the messages' order with 1 ms steps. Never deletes.
    ///
    /// `unknown_ms` is the anchor for a chat with nothing believable in it. It must be an old
    /// time, never now: now would put a chat of bad times above every chat with real activity. Returns `(chat_id, messages moved)`
    /// per affected chat; empty when nothing was ahead, so it is safe to run on every start.
    ///
    /// Why not something smarter: a stored row has only its own time, so there is nothing left to
    /// recover the real time from. A fresh history sync (which carries receipts and ordering)
    /// repairs rows more faithfully, because its upsert overwrites these.
    pub fn fix_future_messages(
        &self,
        network: Network,
        now_ms: i64,
        skew_ms: i64,
        unknown_ms: i64,
    ) -> Result<Vec<(String, usize)>> {
        let limit = now_ms.saturating_add(skew_ms);
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_err("begin"))?;
        let chats: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT chat_id FROM messages WHERE network = ?1 AND ts > ?2
                     ORDER BY chat_id",
                )
                .map_err(db_err("find future messages"))?;
            let rows = stmt
                .query_map(params![network.key(), limit], |r| r.get(0))
                .map_err(db_err("find future messages"))?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(db_err("find future messages"))?
        };
        let mut out = Vec::with_capacity(chats.len());
        for chat_id in chats {
            let newest_sane: Option<i64> = tx
                .query_row(
                    "SELECT MAX(ts) FROM messages WHERE network = ?1 AND chat_id = ?2 AND ts <= ?3",
                    params![network.key(), chat_id, limit],
                    |r| r.get(0),
                )
                .map_err(db_err("read newest believable message"))?;
            let base = newest_sane.unwrap_or(unknown_ms).min(now_ms);
            let ids: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM messages WHERE network = ?1 AND chat_id = ?2 AND ts > ?3
                         ORDER BY ts, id",
                    )
                    .map_err(db_err("list future messages"))?;
                let rows = stmt
                    .query_map(params![network.key(), chat_id, limit], |r| r.get(0))
                    .map_err(db_err("list future messages"))?;
                rows.collect::<std::result::Result<_, _>>()
                    .map_err(db_err("list future messages"))?
            };
            for (i, id) in ids.iter().enumerate() {
                tx.execute(
                    "UPDATE messages SET ts = ?4 WHERE network = ?1 AND chat_id = ?2 AND id = ?3",
                    params![network.key(), chat_id, id, base + 1 + i as i64],
                )
                .map_err(db_err("move future message"))?;
            }
            out.push((chat_id, ids.len()));
        }
        tx.commit().map_err(db_err("commit"))?;
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

/// User text -> FTS5 MATCH expression, or None for "no search". Splits on whitespace, drops
/// tokens with no letter or digit, removes `"`, wraps each token in double quotes and adds `*`
/// to the last one (prefix match while typing). Tokens are ANDed. The result is bound as a
/// parameter, never concatenated into SQL.
pub fn fts_query(user: &str) -> Option<String> {
    let tokens: Vec<String> = user
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| t.replace('"', ""))
        .collect();
    let last = tokens.len().checked_sub(1)?;
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == last {
                    format!("\"{t}\"*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Splits an FTS5 snippet on the hit markers. No control character reaches the caller.
fn split_snippet(raw: &str) -> Vec<SnippetPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut hit = false;
    let flush = |text: &mut String, hit: bool, parts: &mut Vec<SnippetPart>| {
        if !text.is_empty() {
            parts.push(SnippetPart {
                text: std::mem::take(text),
                hit,
            });
        }
    };
    for c in raw.chars() {
        match c {
            HIT_START => {
                flush(&mut text, hit, &mut parts);
                hit = true;
            }
            HIT_END => {
                flush(&mut text, hit, &mut parts);
                hit = false;
            }
            c => text.push(c),
        }
    }
    flush(&mut text, hit, &mut parts);
    parts
}

fn chat_from_row(r: &Row<'_>, network: Network) -> rusqlite::Result<ChatSummary> {
    Ok(ChatSummary {
        network,
        id: r.get(0)?,
        title: r.get(1)?,
        kind: ChatKind::from_key(&r.get::<_, String>(2)?),
        last_ts: r.get(3)?,
        last_preview: r.get(4)?,
        unread: r.get(5)?,
        mentions: r.get(6)?,
        flags: ChatFlags {
            pinned: r.get(7)?,
            muted_until: r.get(8)?,
            hidden: r.get(9)?,
        },
    })
}

fn read_chat(conn: &Connection, network: Network, chat_id: &str) -> Result<Option<ChatSummary>> {
    conn.query_row(
        &format!("{CHAT_SELECT} WHERE c.network = ?1 AND c.id = ?2"),
        params![network.key(), chat_id],
        |r| chat_from_row(r, network),
    )
    .optional()
    .map_err(db_err("read chat"))
}

fn upsert_chat(conn: &Connection, c: &ChatSummary) -> Result<()> {
    // The mention counter belongs to `bump_mentions` / `set_unread`: a chat-list refresh builds
    // summaries without it, and must not zero what a live message just counted. It stays within
    // the unread count, and a read chat has none.
    conn.execute(
        "INSERT INTO chats (network, id, title, kind, last_ts, last_preview, unread, mentions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, MIN(?8, ?7))
         ON CONFLICT (network, id) DO UPDATE SET
            title = excluded.title, kind = excluded.kind, last_ts = excluded.last_ts,
            last_preview = excluded.last_preview, unread = excluded.unread,
            mentions = MIN(chats.mentions, excluded.unread)",
        params![
            c.network.key(),
            c.id,
            c.title,
            c.kind.key(),
            c.last_ts,
            c.last_preview,
            c.unread,
            c.mentions
        ],
    )
    .map_err(db_err("write chat"))?;
    Ok(())
}

fn message_exists(conn: &Connection, network: Network, chat_id: &str, id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM messages WHERE network = ?1 AND chat_id = ?2 AND id = ?3",
        params![network.key(), chat_id, id],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .map_err(db_err("read message"))
}

fn message_from_row(r: &Row<'_>, network: Network, chat_id: &str) -> rusqlite::Result<ChatMessage> {
    let state: String = r.get(5)?;
    let error: Option<String> = r.get(6)?;
    let quote_id: Option<String> = r.get(9)?;
    let mentions: String = r.get(13)?;
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
        sender_id: r.get(7)?,
        edited: r.get(8)?,
        reply_to: match quote_id {
            Some(id) => Some(QuotedRef {
                id,
                sender: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                sender_id: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                preview: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
            }),
            None => None,
        },
        mentions: parse_mentions(&mentions),
        mentions_me: r.get(14)?,
        // Filled by `hydrate`.
        reactions: Vec::new(),
        attachments: Vec::new(),
    })
}

/// `[{"id","name"}]` or ''. A damaged value reads as no mentions: the text still shows "@name".
fn parse_mentions(raw: &str) -> Vec<PersonRef> {
    if raw.is_empty() {
        return Vec::new();
    }
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            Some(PersonRef {
                id: v.get("id")?.as_str()?.to_string(),
                name: v.get("name")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn mentions_json(mentions: &[PersonRef]) -> String {
    if mentions.is_empty() {
        return String::new();
    }
    serde_json::Value::Array(
        mentions
            .iter()
            .map(|p| serde_json::json!({ "id": p.id, "name": p.name }))
            .collect(),
    )
    .to_string()
}

/// Loads attachments and reactions per message over the primary-key index. A deleted message
/// carries neither, whatever rows are left behind.
fn hydrate(conn: &Connection, msgs: &mut [ChatMessage]) -> Result<()> {
    for m in msgs.iter_mut().filter(|m| m.state != MessageState::Deleted) {
        m.attachments = read_attachments(conn, m)?;
        m.reactions = read_reactions(conn, m)?;
    }
    Ok(())
}

fn read_attachments(conn: &Connection, m: &ChatMessage) -> Result<Vec<Attachment>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT kind, name, mime, size, fetch_ref, open_url FROM attachments
             WHERE network = ?1 AND chat_id = ?2 AND message_id = ?3 ORDER BY idx",
        )
        .map_err(db_err("read attachments"))?;
    let rows = stmt
        .query_map(params![m.network.key(), m.chat_id, m.id], |r| {
            Ok(Attachment {
                kind: MediaKind::from_key(&r.get::<_, String>(0)?),
                name: r.get(1)?,
                mime: r.get(2)?,
                size: r.get::<_, Option<i64>>(3)?.map(|n| n.max(0) as u64),
                fetch_ref: FetchRef::new(r.get::<_, String>(4)?),
                open_url: r.get(5)?,
            })
        })
        .map_err(db_err("read attachments"))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(db_err("read attachments"))
}

fn read_reactions(conn: &Connection, m: &ChatMessage) -> Result<Vec<Reaction>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT emoji, COUNT(*), MAX(from_me) FROM reactions
             WHERE network = ?1 AND chat_id = ?2 AND message_id = ?3
             GROUP BY emoji ORDER BY COUNT(*) DESC, emoji",
        )
        .map_err(db_err("read reactions"))?;
    let rows = stmt
        .query_map(params![m.network.key(), m.chat_id, m.id], |r| {
            Ok(Reaction {
                emoji: r.get(0)?,
                count: r.get(1)?,
                mine: r.get(2)?,
            })
        })
        .map_err(db_err("read reactions"))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(db_err("read reactions"))
}

/// Writes the message row, then replaces its attachment rows. Never touches reactions: they
/// are written by `set_reaction` / `replace_reactions` and outlive an edit.
fn upsert_message(conn: &Connection, m: &ChatMessage) -> Result<()> {
    let error = match &m.state {
        MessageState::Failed(reason) => Some(reason.as_str()),
        _ => None,
    };
    let (quote_id, quote_sender, quote_sender_id, quote_preview) = match &m.reply_to {
        Some(q) => (
            Some(q.id.as_str()),
            Some(q.sender.as_str()),
            Some(q.sender_id.as_str()),
            Some(q.preview.as_str()),
        ),
        None => (None, None, None, None),
    };
    conn.execute(
        "INSERT INTO messages (network, chat_id, id, ts, sender, from_me, text, state, error,
                               sender_id, edited, quote_id, quote_sender, quote_sender_id,
                               quote_preview, mentions, mentions_me)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT (network, chat_id, id) DO UPDATE SET
            ts = excluded.ts, sender = excluded.sender, from_me = excluded.from_me,
            text = excluded.text, state = excluded.state, error = excluded.error,
            sender_id = excluded.sender_id, edited = excluded.edited,
            quote_id = excluded.quote_id, quote_sender = excluded.quote_sender,
            quote_sender_id = excluded.quote_sender_id, quote_preview = excluded.quote_preview,
            mentions = excluded.mentions, mentions_me = excluded.mentions_me",
        params![
            m.network.key(),
            m.chat_id,
            m.id,
            m.ts,
            m.sender,
            m.from_me,
            m.text,
            m.state.key(),
            error,
            m.sender_id,
            m.edited,
            quote_id,
            quote_sender,
            quote_sender_id,
            quote_preview,
            mentions_json(&m.mentions),
            m.mentions_me
        ],
    )
    .map_err(db_err("write message"))?;
    conn.execute(
        "DELETE FROM attachments WHERE network = ?1 AND chat_id = ?2 AND message_id = ?3",
        params![m.network.key(), m.chat_id, m.id],
    )
    .map_err(db_err("clear attachments"))?;
    for (idx, a) in m.attachments.iter().enumerate() {
        conn.execute(
            "INSERT INTO attachments
                (network, chat_id, message_id, idx, kind, name, mime, size, fetch_ref, open_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.network.key(),
                m.chat_id,
                m.id,
                idx as i64,
                a.kind.key(),
                a.name,
                a.mime,
                a.size.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
                a.fetch_ref.as_str(),
                a.open_url
            ],
        )
        .map_err(db_err("write attachment"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MUTE_FOREVER;

    fn chat(id: &str, ts: Option<i64>) -> ChatSummary {
        ChatSummary {
            id: id.into(),
            title: format!("chat {id}"),
            last_ts: ts,
            ..ChatSummary::blank(Network::Teams)
        }
    }

    fn msg(chat_id: &str, id: &str, ts: i64, state: MessageState) -> ChatMessage {
        ChatMessage {
            chat_id: chat_id.into(),
            id: id.into(),
            ts,
            sender: "Jane".into(),
            text: format!("text {id}"),
            state,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    fn attachment(name: &str) -> Attachment {
        Attachment {
            kind: MediaKind::Image,
            name: Some(name.into()),
            mime: Some("image/png".into()),
            size: Some(12),
            fetch_ref: FetchRef::new(format!("{{\"id\":\"{name}\"}}")),
            open_url: None,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chatcli-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ids(msgs: Vec<ChatMessage>) -> Vec<String> {
        msgs.into_iter().map(|m| m.id).collect()
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
    fn replace_chats_cleans_dependent_rows_but_keeps_flags() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("keep", Some(1)), chat("gone", Some(2))])
            .unwrap();
        let mut m = msg("gone", "m1", 1, MessageState::Sent);
        m.attachments = vec![attachment("a.png")];
        s.upsert_messages(&[m]).unwrap();
        s.set_reaction(Network::Teams, "gone", "m1", "u1", false, Some("+1"), 5)
            .unwrap();
        s.replace_members(
            Network::Teams,
            "gone",
            &[PersonRef {
                id: "u1".into(),
                name: "Alex Example".into(),
            }],
        )
        .unwrap();
        let pinned = ChatFlags {
            pinned: true,
            muted_until: Some(MUTE_FOREVER),
            hidden: false,
        };
        s.advance_read_mark(Network::Teams, "gone", "u1", 1)
            .unwrap();
        s.set_chat_flags(Network::Teams, "gone", pinned).unwrap();
        s.replace_chats(Network::Teams, &[chat("keep", Some(3))])
            .unwrap();
        let conn = s.lock().unwrap();
        for table in [
            "messages",
            "attachments",
            "reactions",
            "members",
            "read_marks",
            "delivered_marks",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} rows of a removed chat are deleted");
        }
        drop(conn);
        assert_eq!(s.chat_flags(Network::Teams, "gone").unwrap(), pinned);
    }

    #[test]
    fn messages_newest_limit_returned_oldest_first() {
        let s = Store::open_in_memory().unwrap();
        let all: Vec<_> = (1..=5)
            .map(|i| msg("c", &format!("m{i}"), i, MessageState::Sent))
            .collect();
        s.upsert_messages(&all).unwrap();
        let ids = ids(s.messages(Network::Teams, "c", 3).unwrap());
        assert_eq!(ids, ["m3", "m4", "m5"]);
    }

    #[test]
    fn messages_before_and_range_read_in_ts_id_order_and_clamp() {
        let s = Store::open_in_memory().unwrap();
        // Same ts on b and c: the id breaks the tie.
        let all = [
            msg("c", "a", 1, MessageState::Sent),
            msg("c", "b", 2, MessageState::Sent),
            msg("c", "c", 2, MessageState::Sent),
            msg("c", "d", 3, MessageState::Sent),
        ];
        s.upsert_messages(&all).unwrap();
        let page = s.messages_before(Network::Teams, "c", 3, "d", 2).unwrap();
        assert_eq!(ids(page), ["b", "c"], "newest two before d, oldest first");
        let page = s.messages_before(Network::Teams, "c", 2, "c", 10).unwrap();
        assert_eq!(ids(page), ["a", "b"], "strictly before (2, c)");
        assert!(s
            .messages_before(Network::Teams, "c", 1, "a", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            s.messages_before(Network::Teams, "c", 9, "z", 0)
                .unwrap()
                .len(),
            1,
            "limit 0 clamps to 1"
        );
        // The range: from (included) up to (excluded), the oldest first, in (ts, id) order.
        let range = |from: (i64, &str), to: Option<(i64, &str)>, limit| {
            ids(s
                .messages_range(Network::Teams, "c", from, to, limit)
                .unwrap())
        };
        assert_eq!(range((2, "b"), None, 10), ["b", "c", "d"], "to the newest");
        assert_eq!(range((2, "b"), Some((3, "d")), 10), ["b", "c"]);
        assert_eq!(
            range((2, "c"), Some((3, "d")), 10),
            ["c"],
            "the id breaks the tie"
        );
        assert_eq!(
            range((1, "a"), None, 2),
            ["a", "b"],
            "the oldest of the range"
        );
        assert_eq!(range((1, "a"), None, 0), ["a"], "limit 0 clamps to 1");
        assert!(range((3, "d"), Some((3, "d")), 10).is_empty());
        assert!(s
            .messages_range(Network::Teams, "other", (0, ""), None, 10)
            .unwrap()
            .is_empty());
        let oldest = s.oldest_message(Network::Teams, "c").unwrap().unwrap();
        assert_eq!(oldest.id, "a");
        assert!(s.oldest_message(Network::Teams, "none").unwrap().is_none());
    }

    #[test]
    fn message_round_trips_every_field() {
        let s = Store::open_in_memory().unwrap();
        let mut m = msg("c", "m1", 10, MessageState::Sent);
        m.sender_id = "aad-1".into();
        m.edited = true;
        m.mentions_me = true;
        m.reply_to = Some(QuotedRef {
            id: "q1".into(),
            sender: "Sam Sample".into(),
            sender_id: "aad-2".into(),
            preview: "earlier text".into(),
        });
        m.mentions = vec![PersonRef {
            id: "aad-3".into(),
            name: "Jordan Placeholder".into(),
        }];
        m.attachments = vec![attachment("one.png"), attachment("two.png")];
        s.upsert_messages(&[m.clone()]).unwrap();
        assert_eq!(s.message(Network::Teams, "c", "m1").unwrap(), Some(m));
        assert_eq!(s.message(Network::Teams, "c", "nope").unwrap(), None);
    }

    #[test]
    fn pending_is_replaced_on_confirm_and_failure_reason_survives() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(&[msg("c", "client-1", 5, MessageState::Pending)])
            .unwrap();
        s.confirm_message("client-1", &msg("c", "real-1", 6, MessageState::Sent))
            .unwrap();
        let ids = ids(s.messages(Network::Teams, "c", 10).unwrap());
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
    fn confirm_removes_the_pending_rows_attachments_and_reactions() {
        let s = Store::open_in_memory().unwrap();
        let mut pending = msg("c", "client-1", 5, MessageState::Pending);
        pending.attachments = vec![attachment("local.png")];
        s.upsert_messages(&[pending]).unwrap();
        s.set_reaction(
            Network::Teams,
            "c",
            "client-1",
            REACTOR_ME,
            true,
            Some("+1"),
            5,
        )
        .unwrap();
        let mut real = msg("c", "real-1", 6, MessageState::Sent);
        real.attachments = vec![attachment("remote.png")];
        s.confirm_message("client-1", &real).unwrap();
        let conn = s.lock().unwrap();
        let left = |table: &str| -> i64 {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE message_id = 'client-1'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((left("attachments"), left("reactions")), (0, 0));
        drop(conn);
        let got = s.message(Network::Teams, "c", "real-1").unwrap().unwrap();
        assert_eq!(got.attachments, real.attachments);
    }

    #[test]
    fn future_messages_move_back_in_order_and_the_repair_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let now = 1_000_000_000_000;
        let skew = 600_000;
        s.upsert_messages(&[
            msg("a", "ok1", now - 5_000, MessageState::Sent),
            msg("a", "ok2", now - 1_000, MessageState::Sent),
            // Two future messages: their relative order must survive.
            msg("a", "f2", now + 5_000_000_000, MessageState::Sent),
            msg("a", "f1", now + 4_000_000_000, MessageState::Sent),
            // Only future messages in this chat: anchored at the old `unknown` time, not now.
            msg("b", "g1", now + 9_000_000_000, MessageState::Sent),
            // Inside the skew window: left alone.
            msg("c", "near", now + 1_000, MessageState::Sent),
        ])
        .unwrap();
        let moved = s
            .fix_future_messages(Network::Teams, now, skew, 5_000)
            .unwrap();
        assert_eq!(moved, vec![("a".to_string(), 2), ("b".to_string(), 1)]);
        let a = s.messages(Network::Teams, "a", 10).unwrap();
        let order: Vec<_> = a.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, ["ok1", "ok2", "f1", "f2"]);
        assert!(
            a.iter().all(|m| m.ts <= now + 10),
            "nothing is ahead of now"
        );
        assert_eq!(
            a[2].ts,
            now - 1_000 + 1,
            "right after the newest believable message"
        );
        let b = s.messages(Network::Teams, "b", 10).unwrap();
        assert_eq!(b[0].ts, 5_000 + 1);
        assert_eq!(
            s.messages(Network::Teams, "c", 10).unwrap()[0].ts,
            now + 1_000
        );
        // Nothing deleted, and a second run finds nothing.
        assert_eq!(s.messages(Network::Teams, "a", 10).unwrap().len(), 4);
        assert!(s
            .fix_future_messages(Network::Teams, now, skew, 5_000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rename_chat_merges_messages_rows_and_unread() {
        let s = Store::open_in_memory().unwrap();
        let mut old = chat("lid", Some(50));
        old.unread = 2;
        old.last_preview = "newer".into();
        let mut keep = chat("pn", Some(40));
        keep.unread = 1;
        s.upsert_chats(&[old, keep]).unwrap();
        s.upsert_messages(&[
            msg("lid", "m1", 10, MessageState::Sent),
            msg("lid", "dup", 20, MessageState::Sent),
            msg("pn", "dup", 21, MessageState::Sent),
            msg("pn", "m2", 30, MessageState::Sent),
        ])
        .unwrap();
        assert!(s.rename_chat(Network::Teams, "lid", "pn").unwrap());
        assert!(s.chat(Network::Teams, "lid").unwrap().is_none());
        let c = s.chat(Network::Teams, "pn").unwrap().unwrap();
        assert_eq!((c.last_ts, c.unread), (Some(50), 3));
        assert_eq!(c.last_preview, "newer");
        assert_eq!(c.title, "chat pn", "the target's title is kept");
        let ids: Vec<_> = s
            .messages(Network::Teams, "pn", 10)
            .unwrap()
            .into_iter()
            .map(|m| (m.id, m.ts))
            .collect();
        assert_eq!(
            ids,
            [("m1".into(), 10), ("dup".into(), 21), ("m2".into(), 30)]
        );
        assert!(s.messages(Network::Teams, "lid", 10).unwrap().is_empty());
        // Nothing left to move: a second call is a no-op.
        assert!(!s.rename_chat(Network::Teams, "lid", "pn").unwrap());
        // A target that does not exist yet is created from the source.
        s.upsert_chats(&[chat("solo", Some(7))]).unwrap();
        assert!(s.rename_chat(Network::Teams, "solo", "new").unwrap());
        assert_eq!(
            s.chat(Network::Teams, "new").unwrap().unwrap().last_ts,
            Some(7)
        );
    }

    #[test]
    fn rename_chat_moves_the_new_tables_and_target_wins_conflicts() {
        let s = Store::open_in_memory().unwrap();
        let mut old = chat("lid", Some(50));
        old.unread = 3;
        let mut keep = chat("pn", Some(40));
        keep.unread = 3;
        s.upsert_chats(&[old, keep]).unwrap();
        s.bump_mentions(Network::Teams, "lid").unwrap();
        s.bump_mentions(Network::Teams, "pn").unwrap();
        let mut moved = msg("lid", "m1", 10, MessageState::Sent);
        moved.attachments = vec![attachment("m1.png")];
        // "dup" exists in both chats: the target's attachment must stay alone.
        let mut dup_from = msg("lid", "dup", 20, MessageState::Sent);
        dup_from.attachments = vec![attachment("from0.png"), attachment("from1.png")];
        let mut dup_to = msg("pn", "dup", 21, MessageState::Sent);
        dup_to.attachments = vec![attachment("to0.png")];
        s.upsert_messages(&[moved, dup_from, dup_to]).unwrap();
        s.set_reaction(Network::Teams, "lid", "m1", "u1", false, Some("+1"), 1)
            .unwrap();
        s.set_reaction(Network::Teams, "lid", "dup", "u1", false, Some("A"), 1)
            .unwrap();
        s.set_reaction(Network::Teams, "pn", "dup", "u2", false, Some("B"), 1)
            .unwrap();
        let person = |id: &str, name: &str| PersonRef {
            id: id.into(),
            name: name.into(),
        };
        s.replace_members(Network::Teams, "lid", &[person("u1", "From Name")])
            .unwrap();
        s.replace_members(Network::Teams, "pn", &[person("u1", "To Name")])
            .unwrap();
        let from_flags = ChatFlags {
            pinned: true,
            ..ChatFlags::default()
        };
        let to_flags = ChatFlags {
            hidden: true,
            ..ChatFlags::default()
        };
        s.set_chat_flags(Network::Teams, "lid", from_flags).unwrap();
        s.set_chat_flags(Network::Teams, "pn", to_flags).unwrap();

        assert!(s.rename_chat(Network::Teams, "lid", "pn").unwrap());

        let m1 = s.message(Network::Teams, "pn", "m1").unwrap().unwrap();
        assert_eq!(m1.attachments.len(), 1);
        assert_eq!(m1.reactions.len(), 1);
        let dup = s.message(Network::Teams, "pn", "dup").unwrap().unwrap();
        let names: Vec<_> = dup
            .attachments
            .iter()
            .filter_map(|a| a.name.clone())
            .collect();
        assert_eq!(names, ["to0.png"], "the target's own attachments win");
        assert_eq!(
            dup.reactions
                .iter()
                .map(|r| r.emoji.as_str())
                .collect::<Vec<_>>(),
            ["B"]
        );
        assert_eq!(
            s.members(Network::Teams, "pn", 10).unwrap(),
            vec![person("u1", "To Name")]
        );
        assert_eq!(s.chat_flags(Network::Teams, "pn").unwrap(), to_flags);
        assert_eq!(
            s.chat_flags(Network::Teams, "lid").unwrap(),
            ChatFlags::default(),
            "source flags are gone"
        );
        let c = s.chat(Network::Teams, "pn").unwrap().unwrap();
        assert_eq!((c.unread, c.mentions), (6, 2), "mentions add up");
        let conn = s.lock().unwrap();
        for (table, col) in [
            ("attachments", "chat_id"),
            ("reactions", "chat_id"),
            ("members", "chat_id"),
            ("chat_flags", "chat_id"),
        ] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {col} = 'lid'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{table} has no row under the old id");
        }
    }

    #[test]
    fn future_repair_leaves_other_networks_alone() {
        let s = Store::open_in_memory().unwrap();
        let mut m = msg("a", "f", 9_999_999_999_999, MessageState::Sent);
        m.network = Network::WhatsApp;
        s.upsert_messages(&[m]).unwrap();
        assert!(s
            .fix_future_messages(Network::Teams, 1_000, 10, 5)
            .unwrap()
            .is_empty());
        assert_eq!(
            s.messages(Network::WhatsApp, "a", 1).unwrap()[0].ts,
            9_999_999_999_999
        );
    }

    #[test]
    fn name_rows_round_trip_per_network_and_clear() {
        let s = Store::open_in_memory().unwrap();
        let row = |k: &str, u: &str, v: &str| (k.to_string(), u.to_string(), v.to_string());
        s.upsert_names(
            Network::WhatsApp,
            &[row("book", "1", "Alex Example"), row("link", "9", "1")],
        )
        .unwrap();
        s.upsert_names(Network::WhatsApp, &[row("book", "1", "Alex E")])
            .unwrap();
        s.upsert_names(Network::Teams, &[row("book", "5", "Sam Sample")])
            .unwrap();
        let mut got = s.names(Network::WhatsApp).unwrap();
        got.sort();
        assert_eq!(got, vec![row("book", "1", "Alex E"), row("link", "9", "1")]);
        assert_eq!(s.clear_names(Network::WhatsApp).unwrap(), 2);
        assert!(s.names(Network::WhatsApp).unwrap().is_empty());
        assert_eq!(
            s.names(Network::Teams).unwrap().len(),
            1,
            "only the named network is cleared"
        );
        assert_eq!(s.clear_names(Network::WhatsApp).unwrap(), 0);
    }

    #[test]
    fn sender_counts_only_incoming_most_first() {
        let s = Store::open_in_memory().unwrap();
        let mut rows = Vec::new();
        for (id, sender, from_me) in [
            ("1", "Alex Example", false),
            ("2", "Alex Example", false),
            ("3", "+15550100001", false),
            ("4", "You", true),
        ] {
            let mut m = msg("a", id, 1, MessageState::Sent);
            m.sender = sender.into();
            m.from_me = from_me;
            rows.push(m);
        }
        s.upsert_messages(&rows).unwrap();
        assert_eq!(
            s.sender_counts(Network::Teams, "a").unwrap(),
            vec![
                ("Alex Example".to_string(), 2),
                ("+15550100001".to_string(), 1)
            ]
        );
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
    fn file_store_reopens_with_data_and_reopening_is_a_no_op() {
        let dir = temp_dir("reopen");
        let path = dir.join("store.db");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_chats(&[chat("a", Some(1))]).unwrap();
        }
        for _ in 0..2 {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.chats(Network::Teams).unwrap().len(), 1);
            let v = user_version(&s.lock().unwrap()).unwrap();
            assert_eq!(v, SCHEMA_VERSION);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v1 -> v2 ----

    /// The v1 schema exactly as the first release wrote it (copied, not shared with V1_SQL).
    const LITERAL_V1: &str = "CREATE TABLE IF NOT EXISTS chats (
            network TEXT NOT NULL, id TEXT NOT NULL, title TEXT NOT NULL, kind TEXT NOT NULL,
            last_ts INTEGER, last_preview TEXT NOT NULL DEFAULT '',
            unread INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (network, id));
        CREATE TABLE IF NOT EXISTS messages (
            network TEXT NOT NULL, chat_id TEXT NOT NULL, id TEXT NOT NULL, ts INTEGER NOT NULL,
            sender TEXT NOT NULL, from_me INTEGER NOT NULL, text TEXT NOT NULL,
            state TEXT NOT NULL, error TEXT, PRIMARY KEY (network, chat_id, id));
        CREATE INDEX IF NOT EXISTS messages_by_time ON messages (network, chat_id, ts);
        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS names (
            network TEXT NOT NULL, kind TEXT NOT NULL, jid_user TEXT NOT NULL,
            value TEXT NOT NULL, PRIMARY KEY (network, kind, jid_user));
        PRAGMA user_version = 1;";

    fn build_v1(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(LITERAL_V1).unwrap();
        conn.execute(
            "INSERT INTO chats VALUES ('teams', 'c1', 'Old chat', 'group', 50, 'last', 4)",
            [],
        )
        .unwrap();
        for (id, ts, text, state, error) in [
            ("m1", 10, "hello pineapple world", "sent", None),
            ("m2", 20, "second message", "failed", Some("403 Forbidden")),
            ("m3", 30, "gone text", "deleted", None),
        ] {
            conn.execute(
                "INSERT INTO messages VALUES ('teams', 'c1', ?1, ?2, 'Jane', 0, ?3, ?4, ?5)",
                params![id, ts, text, state, error],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO meta VALUES ('k', 'v')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO names VALUES ('whatsapp', 'book', '1555', 'Alex Example')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v1_database_opens_as_v2_with_every_row_and_searchable_text() {
        let dir = temp_dir("v1");
        let path = dir.join("store.db");
        build_v1(&path);
        let s = Store::open(&path).unwrap();
        assert_eq!(user_version(&s.lock().unwrap()).unwrap(), SCHEMA_VERSION);
        let c = s.chat(Network::Teams, "c1").unwrap().unwrap();
        assert_eq!(
            (c.title.as_str(), c.unread, c.mentions, c.last_ts),
            ("Old chat", 4, 0, Some(50))
        );
        assert_eq!(c.kind, ChatKind::Group);
        let m = s.messages(Network::Teams, "c1", 10).unwrap();
        assert_eq!(ids(m.clone()), ["m1", "m2", "m3"]);
        assert_eq!(m[0].text, "hello pineapple world");
        assert_eq!(m[1].state, MessageState::Failed("403 Forbidden".into()));
        assert_eq!(m[2].state, MessageState::Deleted);
        assert!(m
            .iter()
            .all(|m| m.sender_id.is_empty() && m.attachments.is_empty()));
        assert_eq!(s.meta("k").unwrap().as_deref(), Some("v"));
        assert_eq!(s.names(Network::WhatsApp).unwrap().len(), 1);
        let hits = s.search("pineapple", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "the rebuild indexed the old text");
        assert_eq!(hits[0].message_id, "m1");
        assert_eq!(hits[0].chat_title, "Old chat");
        assert!(
            s.search("gone", None, 10).unwrap().is_empty(),
            "deleted rows never match"
        );
        drop(s);
        // Reopening is a no-op.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.messages(Network::Teams, "c1", 10).unwrap().len(), 3);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_open_never_migrates_and_reads_a_current_store() {
        let dir = temp_dir("readonly");
        let path = dir.join("store.db");
        build_v1(&path);
        let err = Store::open_read_only(&path).err().expect("v1 is refused");
        assert!(err.starts_with("The store is not upgraded yet"), "{err}");
        assert_eq!(
            user_version(&Connection::open(&path).unwrap()).unwrap(),
            1,
            "the file was not migrated"
        );
        // Once chatcli has upgraded it, the read-only handle reads and cannot write.
        drop(Store::open(&path).unwrap());
        let s = Store::open_read_only(&path).unwrap();
        assert_eq!(s.search("pineapple", None, 10).unwrap().len(), 1);
        assert_eq!(s.meta("k").unwrap().as_deref(), Some("v"));
        assert!(s.upsert_chats(&[chat("zz", Some(1))]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_opens_of_one_fresh_file_both_succeed() {
        let dir = temp_dir("race");
        let path = dir.join("store.db");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (p, b) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    Store::open(&p).map(|_| ())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("both opens succeed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_migration_rolls_back_and_says_data_is_unchanged() {
        let dir = temp_dir("fail");
        let path = dir.join("store.db");
        build_v1(&path);
        // A leftover table with a v2 name makes the v1 -> v2 script fail halfway.
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE attachments (x INTEGER)")
            .unwrap();
        let err = Store::open(&path).err().expect("migration must fail");
        assert!(
            err.starts_with("Could not upgrade the local store ("),
            "{err}"
        );
        assert!(err.contains("Your data is unchanged"), "{err}");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3, "the old messages table is untouched");
        let has_v2: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_v2'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        assert!(!has_v2, "the half-built table was rolled back");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = temp_dir("newer");
        let path = dir.join("store.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        let err = Store::open(&path).err().expect("newer must be refused");
        assert!(err.contains("newer chatcli"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fts5_is_compiled_in() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x)")
            .unwrap();
    }

    // ---- search ----

    fn text_msg(id: &str, ts: i64, text: &str) -> ChatMessage {
        ChatMessage {
            text: text.into(),
            ..msg("c", id, ts, MessageState::Sent)
        }
    }

    #[test]
    fn edit_updates_the_index_and_delete_removes_it() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("c", Some(1))]).unwrap();
        s.upsert_messages(&[text_msg("m1", 1, "lighthouse keeper")])
            .unwrap();
        assert_eq!(s.search("lighthouse", None, 10).unwrap().len(), 1);
        s.upsert_messages(&[text_msg("m1", 1, "harbour master")])
            .unwrap();
        assert!(s.search("lighthouse", None, 10).unwrap().is_empty());
        assert_eq!(s.search("harbour", None, 10).unwrap().len(), 1);
        // Delete = state Deleted (text emptied) on upsert...
        let mut gone = text_msg("m1", 1, "");
        gone.state = MessageState::Deleted;
        s.upsert_messages(&[gone]).unwrap();
        assert!(s.search("harbour", None, 10).unwrap().is_empty());
        // ...and a real row delete (chat removed) removes it from the index too.
        s.upsert_messages(&[text_msg("m2", 2, "winter garden")])
            .unwrap();
        assert_eq!(s.search("garden", None, 10).unwrap().len(), 1);
        s.replace_chats(Network::Teams, &[]).unwrap();
        assert!(s.search("garden", None, 10).unwrap().is_empty());
        let conn = s.lock().unwrap();
        conn.execute(
            "INSERT INTO messages_fts(messages_fts) VALUES ('integrity-check')",
            [],
        )
        .expect("index matches the table");
    }

    #[test]
    fn search_is_newest_first_across_networks_with_snippet_parts() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("c", Some(1))]).unwrap();
        let mut wa = text_msg("w1", 30, "the Pineapple arrives");
        wa.network = Network::WhatsApp;
        s.upsert_messages(&[
            text_msg("t1", 10, "pineapple pizza is fine"),
            text_msg("t2", 20, "no fruit here"),
            wa,
        ])
        .unwrap();
        let hits = s.search("pinea", None, 10).unwrap();
        let order: Vec<_> = hits
            .iter()
            .map(|h| (h.network, h.message_id.as_str()))
            .collect();
        assert_eq!(
            order,
            [(Network::WhatsApp, "w1"), (Network::Teams, "t1")],
            "prefix match, newest first, both networks"
        );
        let only_teams = s.search("pineapple", Some(Network::Teams), 10).unwrap();
        assert_eq!(only_teams.len(), 1);
        let parts = &only_teams[0].snippet;
        assert!(parts
            .iter()
            .any(|p| p.hit && p.text.to_lowercase() == "pineapple"));
        assert!(parts
            .iter()
            .all(|p| !p.text.contains('\x02') && !p.text.contains('\x03')));
        assert_eq!(
            s.search("pineapple", None, 1).unwrap().len(),
            1,
            "limit applies"
        );
        assert_eq!(only_teams[0].chat_title, "chat c");
        // Diacritics fold: "cafe" finds "café".
        s.upsert_messages(&[text_msg("t3", 40, "meet at the café")])
            .unwrap();
        assert_eq!(s.search("cafe", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn hostile_search_input_never_errors() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(&[text_msg("m1", 1, "a normal message with x in it")])
            .unwrap();
        for q in [
            "\"", "*", "NEAR(", "\"\"", "   ", "a\"b", "-x", "AND", "OR NOT", "(", "^x", "a:b",
        ] {
            assert!(s.search(q, None, 10).is_ok(), "query {q:?} must not error");
        }
        assert!(s.search("", None, 10).unwrap().is_empty());
        assert!(s.search("-x", None, 10).unwrap().len() <= 1);
    }

    #[test]
    fn fts_query_table() {
        let cases: [(&str, Option<&str>); 10] = [
            ("", None),
            ("   ", None),
            ("*", None),
            ("\"", None),
            ("hello", Some("\"hello\"*")),
            ("hello world", Some("\"hello\" \"world\"*")),
            ("a\"b", Some("\"ab\"*")),
            ("-x", Some("\"-x\"*")),
            ("NEAR( ok", Some("\"NEAR(\" \"ok\"*")),
            ("cafe ... bar", Some("\"cafe\" \"bar\"*")),
        ];
        for (input, want) in cases {
            assert_eq!(fts_query(input).as_deref(), want, "input {input:?}");
        }
    }

    /// Measures the claim in issue 33 (search time on a large store). Run with:
    /// `cargo test -p chatcli-core search_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check: fills 200,000 rows"]
    fn search_speed_on_a_large_store() {
        let dir = temp_dir("speed");
        let s = Store::open(&dir.join("store.db")).unwrap();
        let words = [
            "invoice", "meeting", "friday", "release", "review", "lunch", "budget", "ticket",
            "deploy", "holiday", "report", "design", "contract", "roadmap", "feedback", "update",
        ];
        let rows = 200_000;
        for chunk in 0..(rows / 10_000) {
            let batch: Vec<_> = (0..10_000)
                .map(|i| {
                    let n = chunk * 10_000 + i;
                    let text = format!(
                        "{} {} {} number{n}",
                        words[n % 16],
                        words[(n / 16) % 16],
                        words[(n / 256) % 16]
                    );
                    text_msg(&format!("m{n}"), n as i64, &text)
                })
                .collect();
            s.upsert_messages(&batch).unwrap();
        }
        for (label, q) in [
            ("one common word", "invoice"),
            ("two words", "invoice meeting"),
            ("rare token", "number123456"),
            ("prefix", "numb"),
        ] {
            let started = std::time::Instant::now();
            let hits = s.search(q, None, 50).unwrap();
            println!(
                "search {label:>16}: {:>6.1} ms, {} hits (limit 50), {rows} rows",
                started.elapsed().as_secs_f64() * 1000.0,
                hits.len()
            );
        }
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippet_split_has_no_control_characters() {
        let parts = split_snippet("a \x02hit\x03 b \x02two\x03");
        let text: Vec<_> = parts.iter().map(|p| (p.text.as_str(), p.hit)).collect();
        assert_eq!(
            text,
            [("a ", false), ("hit", true), (" b ", false), ("two", true)]
        );
        assert!(split_snippet("").is_empty());
    }

    // ---- reactions, attachments, flags, members, recents ----

    #[test]
    fn reactions_one_per_reactor_replace_clear_and_unknown_message() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(&[msg("c", "m1", 1, MessageState::Sent)])
            .unwrap();
        let set = |reactor: &str, from_me: bool, emoji: Option<&str>| {
            s.set_reaction(Network::Teams, "c", "m1", reactor, from_me, emoji, 1)
                .unwrap()
        };
        assert!(set(REACTOR_ME, true, Some("+1")));
        assert!(set("u1", false, Some("+1")));
        assert!(set("u2", false, Some("A")));
        let r = s
            .message(Network::Teams, "c", "m1")
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            r,
            vec![
                Reaction {
                    emoji: "+1".into(),
                    count: 2,
                    mine: true
                },
                Reaction {
                    emoji: "A".into(),
                    count: 1,
                    mine: false
                },
            ],
            "count desc, mine flag from the own reactor"
        );
        // Setting a different emoji replaces the old one.
        assert!(set(REACTOR_ME, true, Some("B")));
        let r = s
            .message(Network::Teams, "c", "m1")
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            r.iter()
                .map(|x| (x.emoji.as_str(), x.count, x.mine))
                .collect::<Vec<_>>(),
            [("+1", 1, false), ("A", 1, false), ("B", 1, true)],
            "ties order by emoji ascending"
        );
        // None removes.
        assert!(set(REACTOR_ME, true, None));
        assert!(set("u1", false, None));
        assert!(set("u2", false, None));
        assert!(s
            .message(Network::Teams, "c", "m1")
            .unwrap()
            .unwrap()
            .reactions
            .is_empty());
        // Unknown message: false, nothing written.
        assert!(!s
            .set_reaction(Network::Teams, "c", "nope", "u1", false, Some("A"), 1)
            .unwrap());
        let n: i64 = s
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn replace_reactions_swaps_the_whole_list() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(&[msg("c", "m1", 1, MessageState::Sent)])
            .unwrap();
        let row = |reactor: &str, emoji: &str, from_me: bool| ReactionRow {
            reactor: reactor.into(),
            emoji: emoji.into(),
            from_me,
        };
        s.replace_reactions(
            Network::Teams,
            "c",
            "m1",
            &[row("u1", "A", false), row("u2", "A", false)],
        )
        .unwrap();
        s.replace_reactions(Network::Teams, "c", "m1", &[row(REACTOR_ME, "B", true)])
            .unwrap();
        let r = s
            .message(Network::Teams, "c", "m1")
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            r,
            vec![Reaction {
                emoji: "B".into(),
                count: 1,
                mine: true
            }]
        );
        // A message that is not stored gets no orphan rows.
        s.replace_reactions(Network::Teams, "c", "ghost", &[row("u1", "A", false)])
            .unwrap();
        let n: i64 = s
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM reactions WHERE message_id = 'ghost'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn attachments_survive_a_reaction_and_an_edit_replaces_them_by_idx() {
        let s = Store::open_in_memory().unwrap();
        let mut m = msg("c", "m1", 1, MessageState::Sent);
        m.attachments = vec![attachment("a.png"), attachment("b.png")];
        s.upsert_messages(&[m.clone()]).unwrap();
        s.set_reaction(Network::Teams, "c", "m1", "u1", false, Some("A"), 1)
            .unwrap();
        let got = s.message(Network::Teams, "c", "m1").unwrap().unwrap();
        assert_eq!(
            got.attachments, m.attachments,
            "a reaction never touches them"
        );
        // Upsert replaces the attachment rows and keeps the reactions.
        m.attachments = vec![attachment("c.png")];
        m.edited = true;
        s.upsert_messages(&[m.clone()]).unwrap();
        let got = s.message(Network::Teams, "c", "m1").unwrap().unwrap();
        assert_eq!(got.attachments, m.attachments);
        assert_eq!(got.reactions.len(), 1, "upsert never touches reactions");
        assert!(got.edited);
        // A deleted message reads with no attachments or reactions.
        let mut gone = msg("c", "m1", 1, MessageState::Deleted);
        gone.text = String::new();
        s.upsert_messages(&[gone]).unwrap();
        let got = s.message(Network::Teams, "c", "m1").unwrap().unwrap();
        assert!(got.attachments.is_empty() && got.reactions.is_empty());
    }

    #[test]
    fn chat_flags_round_trip_and_ride_on_chat_rows() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_chats(&[chat("a", Some(1)), chat("b", Some(2))])
            .unwrap();
        assert_eq!(
            s.chat_flags(Network::Teams, "a").unwrap(),
            ChatFlags::default()
        );
        let flags = ChatFlags {
            pinned: true,
            muted_until: Some(MUTE_FOREVER),
            hidden: true,
        };
        s.set_chat_flags(Network::Teams, "a", flags).unwrap();
        assert_eq!(s.chat_flags(Network::Teams, "a").unwrap(), flags);
        assert_eq!(s.chat(Network::Teams, "a").unwrap().unwrap().flags, flags);
        let list = s.chats(Network::Teams).unwrap();
        assert_eq!(list[0].id, "b", "order is still by time, pinned or not");
        assert_eq!(list[1].flags, flags);
        s.set_chat_flags(Network::Teams, "a", ChatFlags::default())
            .unwrap();
        assert_eq!(
            s.chat_flags(Network::Teams, "a").unwrap(),
            ChatFlags::default()
        );
    }

    #[test]
    fn mentions_count_survives_a_list_refresh_and_clears_on_read() {
        let s = Store::open_in_memory().unwrap();
        let mut c = chat("a", Some(1));
        c.unread = 3;
        s.upsert_chats(&[c.clone()]).unwrap();
        s.bump_mentions(Network::Teams, "a").unwrap();
        s.bump_mentions(Network::Teams, "a").unwrap();
        assert_eq!(s.chat(Network::Teams, "a").unwrap().unwrap().mentions, 2);
        // A chat-list refresh builds summaries with mentions 0: the counter stays.
        s.upsert_chats(&[c.clone()]).unwrap();
        assert_eq!(s.chat(Network::Teams, "a").unwrap().unwrap().mentions, 2);
        // It never exceeds the unread count.
        c.unread = 1;
        s.upsert_chats(&[c.clone()]).unwrap();
        assert_eq!(s.chat(Network::Teams, "a").unwrap().unwrap().mentions, 1);
        s.set_unread(Network::Teams, "a", 2).unwrap();
        assert_eq!(s.chat(Network::Teams, "a").unwrap().unwrap().mentions, 1);
        s.set_unread(Network::Teams, "a", 0).unwrap();
        let after = s.chat(Network::Teams, "a").unwrap().unwrap();
        assert_eq!((after.unread, after.mentions), (0, 0));
    }

    #[test]
    fn delivery_marks_move_forward_and_the_baseline_is_set_once() {
        let s = Store::open_in_memory().unwrap();
        let net = Network::WhatsApp;
        assert!(s.set_delivery_baseline(net, "c", 100).unwrap());
        assert!(
            !s.set_delivery_baseline(net, "c", 500).unwrap(),
            "never moved"
        );
        assert!(s.advance_delivered_mark(net, "c", "a", 200).unwrap());
        assert!(!s.advance_delivered_mark(net, "c", "a", 150).unwrap());
        assert_eq!(
            s.delivered_marks(net, "c").unwrap(),
            vec![(BASELINE_READER.to_string(), 100), ("a".to_string(), 200)]
        );
        assert!(
            s.read_marks(net, "c").unwrap().is_empty(),
            "separate tables"
        );
    }

    #[test]
    fn a_full_group_under_both_addresses_reads_back_every_mark() {
        let s = Store::open_in_memory().unwrap();
        let net = Network::WhatsApp;
        // 1024 members, each with a phone-number mark and a LID mark.
        for i in 0..MAX_MEMBERS {
            s.advance_delivered_mark(net, "g", &format!("pn{i:04}"), 1)
                .unwrap();
            s.advance_delivered_mark(net, "g", &format!("lid{i:04}"), 2)
                .unwrap();
        }
        s.set_delivery_baseline(net, "g", 0).unwrap();
        assert_eq!(
            s.delivered_marks(net, "g").unwrap().len(),
            2 * MAX_MEMBERS + 1
        );
    }

    #[test]
    fn a_merge_keeps_the_later_mark_and_the_target_baseline() {
        let s = Store::open_in_memory().unwrap();
        let net = Network::WhatsApp;
        let mut lid = chat("lid", Some(1));
        lid.network = net;
        let mut pn = chat("pn", Some(1));
        pn.network = net;
        s.upsert_chats(&[lid, pn]).unwrap();
        // The same reader in both chats: the LID chat holds the later marks.
        s.advance_read_mark(net, "lid", "a", 300).unwrap();
        s.advance_read_mark(net, "pn", "a", 100).unwrap();
        s.advance_delivered_mark(net, "lid", "a", 400).unwrap();
        s.advance_delivered_mark(net, "pn", "a", 200).unwrap();
        // Only in the LID chat: moves over as it is.
        s.advance_read_mark(net, "lid", "b", 50).unwrap();
        // Both have a baseline: the target keeps its own (not the later one).
        s.set_delivery_baseline(net, "lid", 90).unwrap();
        s.set_delivery_baseline(net, "pn", 10).unwrap();
        assert!(s.rename_chat(net, "lid", "pn").unwrap());
        assert_eq!(
            s.read_marks(net, "pn").unwrap(),
            vec![("a".to_string(), 300), ("b".to_string(), 50)]
        );
        assert_eq!(
            s.delivered_marks(net, "pn").unwrap(),
            vec![(BASELINE_READER.to_string(), 10), ("a".to_string(), 400)]
        );
        assert!(s.read_marks(net, "lid").unwrap().is_empty());
        assert!(s.delivered_marks(net, "lid").unwrap().is_empty());
    }

    #[test]
    fn read_marks_only_move_forward_and_follow_a_renamed_chat() {
        let s = Store::open_in_memory().unwrap();
        let net = Network::WhatsApp;
        assert!(s.read_marks(net, "c").unwrap().is_empty());
        assert!(
            s.advance_read_mark(net, "c", "b", 20).unwrap(),
            "new reader"
        );
        assert!(s.advance_read_mark(net, "c", "a", 10).unwrap());
        assert!(!s.advance_read_mark(net, "c", "b", 15).unwrap(), "older");
        assert!(!s.advance_read_mark(net, "c", "b", 20).unwrap(), "same");
        assert!(s.advance_read_mark(net, "c", "b", 30).unwrap(), "newer");
        assert_eq!(
            s.read_marks(net, "c").unwrap(),
            vec![("a".to_string(), 10), ("b".to_string(), 30)]
        );
        // A LID chat folded into its phone-number chat takes its marks along.
        let mut lid = chat("c", Some(1));
        lid.network = net;
        s.upsert_chats(&[lid]).unwrap();
        assert!(s.rename_chat(net, "c", "pn").unwrap());
        assert!(s.read_marks(net, "c").unwrap().is_empty());
        assert_eq!(s.read_marks(net, "pn").unwrap().len(), 2);
    }

    #[test]
    fn members_round_trip_ordered_by_name_and_capped() {
        let s = Store::open_in_memory().unwrap();
        let p = |id: &str, name: &str| PersonRef {
            id: id.into(),
            name: name.into(),
        };
        s.replace_members(
            Network::Teams,
            "g",
            &[p("2", "Sam Sample"), p("1", "Alex Example")],
        )
        .unwrap();
        assert_eq!(
            s.members(Network::Teams, "g", 10).unwrap(),
            vec![p("1", "Alex Example"), p("2", "Sam Sample")]
        );
        s.replace_members(Network::Teams, "g", &[p("3", "Jordan Placeholder")])
            .unwrap();
        assert_eq!(
            s.members(Network::Teams, "g", 10).unwrap().len(),
            1,
            "replace, not add"
        );
        let many: Vec<_> = (0..1100)
            .map(|i| p(&format!("u{i}"), &format!("n{i:04}")))
            .collect();
        s.replace_members(Network::Teams, "big", &many).unwrap();
        assert_eq!(s.members(Network::Teams, "big", 5000).unwrap().len(), 1024);
        assert_eq!(
            s.members(Network::Teams, "big", 0).unwrap().len(),
            1,
            "limit 0 clamps to 1"
        );
    }

    #[test]
    fn recent_emoji_keep_30_newest_first() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..35 {
            s.touch_recent_emoji(&format!("e{i}"), 1000 + i).unwrap();
        }
        let got = s.recent_emoji(100).unwrap();
        assert_eq!(got.len(), 30);
        assert_eq!(got[0], "e34");
        assert_eq!(got[29], "e5", "the five oldest were trimmed");
        // Touching an old one moves it to the front, with no duplicate.
        s.touch_recent_emoji("e10", 5000).unwrap();
        let got = s.recent_emoji(3).unwrap();
        assert_eq!(got, ["e10", "e34", "e33"]);
        assert_eq!(s.recent_emoji(100).unwrap().len(), 30);
        // Two touches in the same millisecond: the later one is first.
        s.touch_recent_emoji("x", 9000).unwrap();
        s.touch_recent_emoji("y", 9000).unwrap();
        assert_eq!(s.recent_emoji(2).unwrap(), ["y", "x"]);
    }

    #[test]
    fn bind_account_wipes_only_on_a_different_account() {
        let s = Store::open_in_memory().unwrap();
        let mut c = chat("a", Some(1));
        c.unread = 1;
        s.upsert_chats(&[c]).unwrap();
        let mut m = msg("a", "m1", 1, MessageState::Sent);
        m.from_me = true;
        m.attachments = vec![attachment("a.png")];
        s.upsert_messages(&[m]).unwrap();
        s.set_reaction(Network::Teams, "a", "m1", "u1", false, Some("A"), 1)
            .unwrap();
        s.set_chat_flags(
            Network::Teams,
            "a",
            ChatFlags {
                pinned: true,
                ..ChatFlags::default()
            },
        )
        .unwrap();
        s.advance_read_mark(Network::Teams, "a", "u1", 1).unwrap();
        s.set_delivery_baseline(Network::Teams, "a", 1).unwrap();
        s.set_meta("teams.older.a", Some("cursor")).unwrap();
        s.set_meta("teams.subscription_id", Some("sub")).unwrap();
        let mut other = chat("w", Some(1));
        other.network = Network::WhatsApp;
        s.upsert_chats(&[other]).unwrap();

        // First call records the account and keeps the data.
        assert!(!s.bind_account(Network::Teams, "account-A").unwrap());
        assert_eq!(s.messages(Network::Teams, "a", 10).unwrap().len(), 1);
        // Same account: nothing changes.
        assert!(!s.bind_account(Network::Teams, "account-A").unwrap());
        assert_eq!(s.chats(Network::Teams).unwrap().len(), 1);
        // Another account: the network's cached data goes, the other network's stays.
        assert!(s.bind_account(Network::Teams, "account-B").unwrap());
        assert!(s.chats(Network::Teams).unwrap().is_empty());
        assert!(s.messages(Network::Teams, "a", 10).unwrap().is_empty());
        assert_eq!(
            s.chat_flags(Network::Teams, "a").unwrap(),
            ChatFlags::default()
        );
        assert_eq!(s.meta("teams.older.a").unwrap(), None);
        assert!(s.read_marks(Network::Teams, "a").unwrap().is_empty());
        assert!(s.delivered_marks(Network::Teams, "a").unwrap().is_empty());
        assert_eq!(
            s.meta("teams.subscription_id").unwrap().as_deref(),
            Some("sub")
        );
        assert_eq!(s.chats(Network::WhatsApp).unwrap().len(), 1);
        assert_eq!(
            s.meta("teams.account").unwrap().as_deref(),
            Some("account-B")
        );
        let conn = s.lock().unwrap();
        for table in ["attachments", "reactions"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        drop(conn);
        assert!(!s.bind_account(Network::Teams, "account-B").unwrap());
    }
}
