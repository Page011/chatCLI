//! Types for `chatcli doctor`. The checks themselves live next to what they inspect: the Teams
//! and WhatsApp crates each return a list of `Check`s, and the app crate prints them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    /// What to do about a Warn or Fail, one sentence.
    pub fix: Option<String>,
}
