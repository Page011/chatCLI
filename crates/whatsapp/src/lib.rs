//! WhatsApp backend for chatcli: links as a WhatsApp Web device through whatsapp-rust.
//!
//! - `backend`: the task (`run`) that owns the session, commands, store and events.
//! - `map`: pure mapping from WhatsApp messages / JIDs to the shared model.
//! - `tls`: TLS roots (Mozilla + OS store) so the socket works behind TLS inspection.

mod backend;
mod map;
mod tls;

pub use backend::run;

/// Tests must never touch the real data folder: point `CHATCLI_DATA_DIR` at a temp dir once for
/// the whole test binary, before anything logs.
#[cfg(test)]
pub(crate) fn test_data_dir() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir =
            std::env::temp_dir().join(format!("chatcli-whatsapp-test-{}", std::process::id()));
        std::env::set_var("CHATCLI_DATA_DIR", dir);
    });
}
