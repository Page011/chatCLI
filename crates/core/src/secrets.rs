//! Secrets in Windows Credential Manager.
//!
//! Credential Manager caps one secret at 2560 bytes of UTF-16 (CRED_MAX_CREDENTIAL_BLOB_SIZE).
//! Entra refresh tokens for work accounts are routinely longer, so a secret is stored in
//! numbered pieces: `<name>`, `<name>-1`, `<name>-2`, ...

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use crate::{paths, Result};

pub const SERVICE: &str = "chatcli";
/// Tokens are ASCII, so 1000 chars is 2000 bytes of UTF-16, under the 2560 cap.
const CHUNK_CHARS: usize = 1000;
/// Room for a 16000-character secret, and a bound on every loop over the store.
const MAX_CHUNKS: usize = 16;

const LOCK_TRIES: u32 = 50;
const LOCK_WAIT: Duration = Duration::from_millis(100);

/// Opened with share mode 0, so a second process cannot open it until this handle drops.
fn try_lock(dir: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(dir.join("secrets.lock"))
}

/// One writer or reader at a time across processes: the CLI subcommands and the full-screen
/// client can refresh a token together, and interleaved piece writes would store a token
/// stitched from two. Held while the returned file lives.
fn lock() -> Result<File> {
    let dir = paths::data_dir()?;
    for _ in 0..LOCK_TRIES {
        match try_lock(&dir) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(32) => std::thread::sleep(LOCK_WAIT),
            Err(e) => return Err(format!("credential store lock: {e}")),
        }
    }
    Err("credential store is busy: close other chatcli windows and try again.".into())
}

fn entry_at(service: &str, name: &str, index: usize) -> Result<keyring::Entry> {
    let user = if index == 0 {
        name.to_string()
    } else {
        format!("{name}-{index}")
    };
    keyring::Entry::new(service, &user).map_err(|e| format!("credential store: {e}"))
}

fn split_chunks(secret: &str, size: usize) -> Vec<String> {
    secret
        .chars()
        .collect::<Vec<char>>()
        .chunks(size)
        .map(|c| c.iter().collect())
        .collect()
}

/// Writes the pieces, then clears any piece a longer previous secret left behind, so a
/// shorter secret never reads back with an old tail glued to it.
pub fn store(service: &str, name: &str, secret: &str) -> Result<()> {
    let chunks = split_chunks(secret, CHUNK_CHARS);
    let _lock = lock()?;
    if chunks.is_empty() || chunks.len() > MAX_CHUNKS {
        return Err(format!(
            "credential store: secret '{name}' is empty or larger than {} characters",
            CHUNK_CHARS * MAX_CHUNKS
        ));
    }
    for (index, chunk) in chunks.iter().enumerate() {
        entry_at(service, name, index)?
            .set_password(chunk)
            .map_err(|e| format!("credential store: {e}"))?;
    }
    for index in chunks.len()..MAX_CHUNKS {
        delete_entry(service, name, index)?;
    }
    Ok(())
}

/// The whole secret, or None when nothing is stored.
pub fn read(service: &str, name: &str) -> Result<Option<String>> {
    let _lock = lock()?;
    let mut out = String::new();
    for index in 0..MAX_CHUNKS {
        match entry_at(service, name, index)?.get_password() {
            Ok(part) => out.push_str(&part),
            Err(keyring::Error::NoEntry) => break,
            Err(e) => return Err(format!("credential store: {e}")),
        }
    }
    Ok((!out.is_empty()).then_some(out))
}

fn delete_entry(service: &str, name: &str, index: usize) -> Result<()> {
    match entry_at(service, name, index)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("credential store: {e}")),
    }
}

/// Deletes every piece. Tries all of them even if one fails, then reports the failure.
pub fn clear(service: &str, name: &str) -> Result<()> {
    let _lock = lock()?;
    let mut failure = None;
    for index in 0..MAX_CHUNKS {
        if let Err(e) = delete_entry(service, name, index) {
            failure = Some(e);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_file_admits_one_holder_at_a_time() {
        let dir = std::env::temp_dir().join(format!("chatcli-secrets-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = try_lock(&dir).unwrap();
        let second = try_lock(&dir).unwrap_err();
        assert_eq!(second.raw_os_error(), Some(32), "{second}");
        drop(first);
        assert!(try_lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunking_round_trips() {
        assert_eq!(split_chunks("", 4), Vec::<String>::new());
        assert_eq!(split_chunks("abcdefgh", 4), vec!["abcd", "efgh"]);
        assert_eq!(split_chunks("abcdefghi", 4), vec!["abcd", "efgh", "i"]);
        let token = "A".repeat(2600) + "." + &"b".repeat(700);
        let chunks = split_chunks(&token, CHUNK_CHARS);
        assert_eq!(chunks.len(), 4, "a 3301-character token needs four pieces");
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
        assert_eq!(chunks.concat(), token, "pieces rebuild the token exactly");
    }

    /// The real Windows store, under its own service name so it cannot touch a signed-in
    /// account, with a secret far past the 2560-byte blob limit.
    /// Run with: cargo test credential_store -- --ignored
    #[test]
    #[ignore = "writes to the Windows credential store"]
    fn credential_store_holds_a_long_secret() {
        const TEST_SERVICE: &str = "chatcli (test round trip)";
        const NAME: &str = "long-secret";
        let long = "x".repeat(3300);
        clear(TEST_SERVICE, NAME).unwrap();
        store(TEST_SERVICE, NAME, &long).expect("store");
        assert_eq!(
            read(TEST_SERVICE, NAME).unwrap().as_deref(),
            Some(long.as_str())
        );
        let shorter = "y".repeat(120);
        store(TEST_SERVICE, NAME, &shorter).expect("store shorter");
        assert_eq!(
            read(TEST_SERVICE, NAME).unwrap().as_deref(),
            Some(shorter.as_str()),
            "a shorter secret must not read back with the old tail attached"
        );
        clear(TEST_SERVICE, NAME).unwrap();
        assert_eq!(read(TEST_SERVICE, NAME).unwrap(), None);
    }
}
