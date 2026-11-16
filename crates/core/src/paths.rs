//! Where chatcli keeps its files: `%LOCALAPPDATA%\chatcli` (store, WhatsApp session, log,
//! config). `CHATCLI_DATA_DIR` overrides it, so tests never touch the real data.

use std::path::PathBuf;

use crate::Result;

pub fn data_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("CHATCLI_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        // A test that forgot CHATCLI_DATA_DIR must not write into the user's real log, store
        // or session. Cargo puts every test binary in a `deps` folder; the app never runs there.
        None if is_test_binary() => {
            std::env::temp_dir().join(format!("chatcli-test-{}", std::process::id()))
        }
        None => {
            let base = std::env::var_os("LOCALAPPDATA")
                .ok_or("LOCALAPPDATA is not set, so chatcli has nowhere to keep its data")?;
            PathBuf::from(base).join("chatcli")
        }
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create data folder {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Cargo names every test binary `<crate>-<16 hex digits>` (in `deps\` or, with newer cargo,
/// `build\<pkg>\<hash>\out\`). The app always runs as plain `chatcli.exe`. The folder layout
/// changed between cargo versions; the hashed file name did not.
fn is_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_stem()
                .map(|s| is_hashed_name(&s.to_string_lossy()))
        })
        .unwrap_or(false)
}

fn is_hashed_name(stem: &str) -> bool {
    stem.rsplit_once('-').is_some_and(|(name, hash)| {
        !name.is_empty() && hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit())
    })
}

pub fn store_db() -> Result<PathBuf> {
    Ok(data_dir()?.join("store.db"))
}

/// whatsapp-rust's own session store (identity keys). Anyone with this file can act as the
/// linked device: never copy or commit it.
pub fn whatsapp_db() -> Result<PathBuf> {
    Ok(data_dir()?.join("whatsapp.db"))
}

pub fn log_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("chatcli.log"))
}

/// Non-secret settings (relay URL). Secrets live in Credential Manager.
pub fn config_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("config.json"))
}

#[cfg(test)]
mod tests {
    /// This test binary runs from target/debug/deps: without CHATCLI_DATA_DIR it must get a
    /// temp folder, never the user's real data folder.
    #[test]
    fn test_binaries_never_get_the_real_folder() {
        assert!(super::is_hashed_name("chatcli_core-c95379607136e937"));
        assert!(!super::is_hashed_name("chatcli"));
        assert!(!super::is_hashed_name("chatcli-tui"));
        assert!(!super::is_hashed_name("-c95379607136e937"));
        assert!(super::is_test_binary(), "{:?}", std::env::current_exe());
        if std::env::var_os("CHATCLI_DATA_DIR").is_none() {
            let dir = super::data_dir().unwrap();
            assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
        }
    }
}
