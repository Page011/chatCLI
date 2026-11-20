//! The search box over the local store (issue 33): the query, the hits and the selected row. The
//! same box searches one chat (issue 78: `/` in message selection) when it has a `scope`.
//!
//! The store does the work (`Store::search`: FTS5, quoted, limited, never a panic); this module
//! only keeps what the overlay shows. A query runs when its text changes, on the UI thread: it
//! is one indexed read of at most `LIMIT` rows.

use crate::input::TextInput;
use crate::list;
use chatcli_core::model::Network;
use chatcli_core::store::{SearchHit, SearchScope, Store};

/// Hits shown at most. The store clamps to 200; a screen shows far fewer.
pub const LIMIT: u32 = 50;

/// The one chat a search looks in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatScope {
    pub network: Network,
    pub chat_id: String,
    /// For the box's title.
    pub title: String,
}

#[derive(Debug, Clone, Default)]
pub struct Search {
    /// `None`: every chat of both networks.
    pub scope: Option<ChatScope>,
    pub query: TextInput,
    pub hits: Vec<SearchHit>,
    pub sel: usize,
    /// First hit shown.
    pub top: usize,
    /// Why the last query failed (shown in the status row); `None` when it ran.
    pub error: Option<String>,
}

impl Search {
    /// An empty search of one chat.
    pub fn in_chat(network: Network, chat_id: &str, title: &str) -> Search {
        Search {
            scope: Some(ChatScope {
                network,
                chat_id: chat_id.to_string(),
                title: title.to_string(),
            }),
            ..Search::default()
        }
    }

    /// Runs the query again after its text changed. The selection goes back to the top.
    pub fn run(&mut self, store: &Store) {
        self.sel = 0;
        self.top = 0;
        let scope = match &self.scope {
            Some(c) => SearchScope::Chat(c.network, &c.chat_id),
            None => SearchScope::All,
        };
        match store.search(self.query.text(), scope, LIMIT) {
            Ok(hits) => {
                self.hits = hits;
                self.error = None;
            }
            Err(e) => {
                self.hits.clear();
                self.error = Some(e);
            }
        }
    }

    /// Moves the selection by `delta` rows, kept inside the hits.
    pub fn step(&mut self, delta: isize) {
        self.sel = list::page(self.sel, self.hits.len(), delta);
    }

    /// Scrolls so the selected row is inside a window of `rows` rows.
    pub fn keep_visible(&mut self, rows: usize) {
        self.top = list::keep_visible(self.top, self.sel, rows, self.hits.len());
    }

    pub fn chosen(&self) -> Option<&SearchHit> {
        self.hits.get(self.sel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::model::{ChatMessage, ChatSummary};

    fn store_with(texts: &[&str]) -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats(&[ChatSummary {
                id: "c".into(),
                title: "Alex Example".into(),
                ..ChatSummary::blank(Network::Teams)
            }])
            .unwrap();
        let msgs: Vec<ChatMessage> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| ChatMessage {
                chat_id: "c".into(),
                id: format!("m{i}"),
                ts: 1_000 + i as i64,
                text: (*t).into(),
                ..ChatMessage::blank(Network::Teams)
            })
            .collect();
        store.upsert_messages(&msgs).unwrap();
        store
    }

    #[test]
    fn a_query_finds_newest_first_and_resets_the_selection() {
        let store = store_with(&["budget draft", "lunch", "budget final"]);
        let mut s = Search {
            query: "budget".into(),
            sel: 5,
            top: 5,
            ..Search::default()
        };
        s.run(&store);
        let ids: Vec<&str> = s.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(ids, ["m2", "m0"]);
        assert_eq!((s.sel, s.top, s.error.clone()), (0, 0, None));
        assert_eq!(s.chosen().unwrap().chat_title, "Alex Example");
    }

    #[test]
    fn a_chat_search_finds_only_that_chat() {
        let store = store_with(&["budget draft"]);
        store
            .upsert_messages(&[ChatMessage {
                chat_id: "other".into(),
                id: "o1".into(),
                ts: 5_000,
                text: "budget elsewhere".into(),
                ..ChatMessage::blank(Network::Teams)
            }])
            .unwrap();
        let mut s = Search::in_chat(Network::Teams, "c", "Alex Example");
        s.query = "budget".into();
        s.run(&store);
        let ids: Vec<&str> = s.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(ids, ["m0"]);
        s.scope = None;
        s.run(&store);
        assert_eq!(s.hits.len(), 2, "every chat without a scope");
    }

    #[test]
    fn hostile_and_empty_queries_never_fail() {
        let store = store_with(&["plain words"]);
        let mut s = Search::default();
        for q in ["", "   ", "\"", "*", "NEAR(", "a\"b", "-x", "()"] {
            s.query = q.into();
            s.run(&store);
            assert!(s.error.is_none(), "{q:?}: {:?}", s.error);
            assert!(s.hits.is_empty(), "{q:?}");
        }
    }

    #[test]
    fn selection_stays_inside_the_hits_and_the_window_follows() {
        let store = store_with(&["one", "one", "one", "one", "one", "one"]);
        let mut s = Search {
            query: "one".into(),
            ..Search::default()
        };
        s.run(&store);
        assert_eq!(s.hits.len(), 6);
        s.step(-1);
        assert_eq!(s.sel, 0);
        s.step(100);
        assert_eq!(s.sel, 5);
        s.keep_visible(3);
        assert_eq!(s.top, 3, "the last row is the window's last row");
        s.step(-5);
        s.keep_visible(3);
        assert_eq!(s.top, 0);
        let mut none = Search::default();
        none.step(1);
        assert_eq!(none.sel, 0);
        assert!(none.chosen().is_none());
    }
}
