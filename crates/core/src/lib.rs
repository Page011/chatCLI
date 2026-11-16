//! chatcli core: the contract every crate shares.
//!
//! - `model`: chats, messages, per-network status, and the Event / Command enums that flow
//!   between the network backends and the TUI.
//! - `store`: the local SQLite copy of chats, messages, reactions, attachments metadata, chat
//!   flags and the full-text index (instant start, offline reading, search).
//! - `settings`: the `settings.toml` loader with per-section fallback, and its comment-preserving writer.
//! - `media`, `limits`, `open`: the capped media cache, size limits and opening links/files.
//! - `runtime`: what `main` hands to each backend and the TUI.
//! - `doctor`: result types for `chatcli doctor`.
//! - `secrets`: the OS credential store (Windows Credential Manager, macOS Keychain, Linux Secret Service, or a private file on Linux with `CHATCLI_SECRET_STORE=file`), chunked past its 2560-byte limit.
//! - `config`, `paths`, `log`: `config.json`, where data lives, and the rolling log file.

pub mod config;
pub mod doctor;
pub mod limits;
pub mod lock;
pub mod log;
pub mod media;
pub mod model;
pub mod open;
pub mod paths;
pub mod runtime;
pub mod secrets;
pub mod settings;
pub mod store;

/// Every error the user sees is one sentence that says what to do next.
pub type Result<T> = std::result::Result<T, String>;

/// Error text plus every `source()` cause. reqwest's own Display hides the real reason
/// (for example the TLS failure behind "error sending request").
pub fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = inner.source();
    }
    if out.contains("UnknownIssuer") && std::env::var_os("SSL_CERT_FILE").is_some() {
        // rustls-native-certs reads ONLY this file when it is set, not the Windows store.
        out.push_str(" (SSL_CERT_FILE is set and replaces the Windows root store: unset it)");
    }
    out
}
