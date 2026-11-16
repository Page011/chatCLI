//! Secrets in the OS credential store: Windows Credential Manager, macOS Keychain, or the
//! Linux Secret Service (GNOME Keyring, KWallet) through the `keyring` crate.
//!
//! Credential Manager caps one secret at 2560 bytes of UTF-16 (CRED_MAX_CREDENTIAL_BLOB_SIZE).
//! Entra refresh tokens for work accounts are routinely longer, so a secret is stored in
//! pieces, and a single pointer entry says which set of pieces is the live one (see
//! `POINTER_PREFIX`). The same layout is used on every OS so one code path is tested.
//!
//! On Linux, `CHATCLI_SECRET_STORE=file` keeps the same entries in `secrets.json` in the data
//! folder instead (mode 0600, not encrypted), for systems with no Secret Service.

use std::fs::File;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::time::Duration;

use crate::{lock as file_lock, paths, Result};

pub const SERVICE: &str = "chatcli";
/// Tokens are ASCII, so 1000 chars is 2000 bytes of UTF-16, under the 2560 cap.
const CHUNK_CHARS: usize = 1000;
/// Room for a 16000-character secret, and a bound on every loop over the store.
const MAX_CHUNKS: usize = 16;

const LOCK_TRIES: u32 = 50;
const LOCK_WAIT: Duration = Duration::from_millis(100);

/// One writer or reader at a time across processes: the CLI subcommands and the full-screen
/// client can refresh a token together, and interleaved piece writes would store a token
/// stitched from two. Held while the returned file lives.
fn lock() -> Result<File> {
    lock_in(&paths::data_dir()?)
}

fn lock_in(dir: &Path) -> Result<File> {
    for _ in 0..LOCK_TRIES {
        match file_lock::try_acquire(&dir.join("secrets.lock")) {
            Ok(Some(f)) => return Ok(f),
            Ok(None) => std::thread::sleep(LOCK_WAIT),
            Err(e) => return Err(format!("credential store lock: {e}")),
        }
    }
    Err("credential store is busy: close other chatcli windows and try again.".into())
}

/// Where the pieces live. The OS credential store in the app (or `secrets.json` on Linux when
/// asked for), a map in tests.
trait Vault {
    fn get(&self, service: &str, user: &str) -> Result<Option<String>>;
    fn set(&self, service: &str, user: &str, value: &str) -> Result<()>;
    /// A missing entry is not an error.
    fn delete(&self, service: &str, user: &str) -> Result<()>;
}

struct OsVault;

fn os_error(e: keyring::Error) -> String {
    #[cfg(target_os = "linux")]
    if matches!(
        e,
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_)
    ) {
        return format!(
            "credential store: {e}. If no Secret Service runs here, set \
             {FILE_STORE_ENV}=file to keep secrets in a private file in the data folder."
        );
    }
    format!("credential store: {e}")
}

fn entry(service: &str, user: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, user).map_err(os_error)
}

impl Vault for OsVault {
    fn get(&self, service: &str, user: &str) -> Result<Option<String>> {
        match entry(service, user)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(os_error(e)),
        }
    }

    fn set(&self, service: &str, user: &str, value: &str) -> Result<()> {
        entry(service, user)?.set_password(value).map_err(os_error)
    }

    fn delete(&self, service: &str, user: &str) -> Result<()> {
        match entry(service, user)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(os_error(e)),
        }
    }
}

#[cfg(target_os = "linux")]
const FILE_STORE_ENV: &str = "CHATCLI_SECRET_STORE";
#[cfg(target_os = "linux")]
const FILE_STORE_NAME: &str = "secrets.json";

/// True when the user chose `CHATCLI_SECRET_STORE=file`. Any other value is an error rather
/// than a silent fall back to the keyring.
#[cfg(target_os = "linux")]
fn file_store_chosen() -> Result<bool> {
    match std::env::var(FILE_STORE_ENV) {
        Ok(v) if v == "file" => Ok(true),
        Ok(v) if v.is_empty() => Ok(false),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Ok(v) => Err(format!(
            "{FILE_STORE_ENV}={v} is not understood: set it to 'file' or unset it"
        )),
        Err(e) => Err(format!("{FILE_STORE_ENV}: {e}")),
    }
}

/// The Linux fallback for systems with no Secret Service: one JSON object of
/// `<service>/<user>` to value. Every change rewrites the whole file through
/// `config::write_private` (an exclusive temp file of mode 0600, then a rename), so a crash
/// leaves the old file or the new one. Not encrypted: it relies on the file mode.
#[cfg(target_os = "linux")]
struct FileVault {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl FileVault {
    fn key(service: &str, user: &str) -> String {
        format!("{service}/{user}")
    }

    fn load(&self) -> Result<std::collections::BTreeMap<String, String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("secrets file {} is damaged: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(format!("secrets file {}: {e}", self.path.display())),
        }
    }

    fn save(&self, all: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let text = serde_json::to_string_pretty(all).map_err(|e| format!("secrets file: {e}"))?;
        crate::config::write_private(&self.path, &text)
    }
}

#[cfg(target_os = "linux")]
impl Vault for FileVault {
    fn get(&self, service: &str, user: &str) -> Result<Option<String>> {
        Ok(self.load()?.remove(&Self::key(service, user)))
    }

    fn set(&self, service: &str, user: &str, value: &str) -> Result<()> {
        let mut all = self.load()?;
        all.insert(Self::key(service, user), value.to_string());
        self.save(&all)
    }

    fn delete(&self, service: &str, user: &str) -> Result<()> {
        let mut all = self.load()?;
        if all.remove(&Self::key(service, user)).is_some() {
            self.save(&all)?;
        }
        Ok(())
    }
}

/// Runs `f` against the store the user chose.
fn with_vault<T>(f: impl FnOnce(&dyn Vault) -> Result<T>) -> Result<T> {
    #[cfg(target_os = "linux")]
    if file_store_chosen()? {
        return f(&FileVault {
            path: paths::data_dir()?.join(FILE_STORE_NAME),
        });
    }
    f(&OsVault)
}

fn split_chunks(secret: &str, size: usize) -> Vec<String> {
    secret
        .chars()
        .collect::<Vec<char>>()
        .chunks(size)
        .map(|c| c.iter().collect())
        .collect()
}

/// Storage layout. The entry named `<name>` is a pointer, `@v2:<generation>:<pieces>`, and the
/// pieces sit under `<name>~<generation>~<index>`. A write fills the OTHER generation first and
/// only then rewrites the pointer, which is one credential and so one atomic switch: a process
/// killed half way leaves the pointer on the old, complete generation, never a token stitched
/// from two. Secrets written before this layout (piece 0 in `<name>`, the rest in `<name>-1`,
/// `<name>-2`, ...) still read, and the next write converts them.
const POINTER_PREFIX: &str = "@v2:";

fn piece_user(name: &str, generation: u8, index: usize) -> String {
    format!("{name}~{generation}~{index}")
}

fn legacy_user(name: &str, index: usize) -> String {
    if index == 0 {
        name.to_string()
    } else {
        format!("{name}-{index}")
    }
}

/// (generation, piece count) of a pointer value, None when it is not one.
fn parse_pointer(value: &str) -> Option<(u8, usize)> {
    let (generation, count) = value.strip_prefix(POINTER_PREFIX)?.split_once(':')?;
    let generation: u8 = generation.parse().ok().filter(|g| *g <= 1)?;
    let count: usize = count
        .parse()
        .ok()
        .filter(|n| (1..=MAX_CHUNKS).contains(n))?;
    Some((generation, count))
}

fn store_in(vault: &dyn Vault, service: &str, name: &str, secret: &str) -> Result<()> {
    let chunks = split_chunks(secret, CHUNK_CHARS);
    if chunks.is_empty() || chunks.len() > MAX_CHUNKS {
        return Err(format!(
            "credential store: secret '{name}' is empty or larger than {} characters",
            CHUNK_CHARS * MAX_CHUNKS
        ));
    }
    let current = vault
        .get(service, name)?
        .and_then(|p| parse_pointer(&p))
        .map(|(g, _)| g);
    let next = u8::from(current == Some(0));
    for (index, chunk) in chunks.iter().enumerate() {
        vault.set(service, &piece_user(name, next, index), chunk)?;
    }
    // Pieces a longer earlier secret left in this generation are not part of the new one.
    for index in chunks.len()..MAX_CHUNKS {
        vault.delete(service, &piece_user(name, next, index))?;
    }
    vault.set(
        service,
        name,
        &format!("{POINTER_PREFIX}{next}:{}", chunks.len()),
    )?;
    // The new secret is live. Clearing the old generation and any pre-v2 pieces is tidiness: a
    // failure here is harmless and must not turn a good write into an error.
    for index in 0..MAX_CHUNKS {
        let _ = vault.delete(service, &piece_user(name, 1 - next, index));
        if index > 0 {
            let _ = vault.delete(service, &legacy_user(name, index));
        }
    }
    Ok(())
}

fn read_from(vault: &dyn Vault, service: &str, name: &str) -> Result<Option<String>> {
    let Some(head) = vault.get(service, name)? else {
        return Ok(None);
    };
    let mut out = String::new();
    if let Some((generation, count)) = parse_pointer(&head) {
        for index in 0..count {
            match vault.get(service, &piece_user(name, generation, index))? {
                Some(part) => out.push_str(&part),
                None => {
                    // The pointer is only written after every piece, so this means someone
                    // deleted a piece by hand. Say so in the log; the caller sees "signed out".
                    crate::log::error(
                        "secrets",
                        &format!("stored secret '{name}' is incomplete: treated as missing"),
                    );
                    return Ok(None);
                }
            }
        }
    } else {
        // Pre-v2 layout: `head` is piece 0.
        out.push_str(&head);
        for index in 1..MAX_CHUNKS {
            match vault.get(service, &legacy_user(name, index))? {
                Some(part) => out.push_str(&part),
                None => break,
            }
        }
    }
    Ok((!out.is_empty()).then_some(out))
}

/// Deletes the pointer first (so a reader never sees a half-removed secret), then every piece.
/// Tries all of them even if one fails, then reports the failure.
fn clear_in(vault: &dyn Vault, service: &str, name: &str) -> Result<()> {
    let mut failure = vault.delete(service, name).err();
    for index in 0..MAX_CHUNKS {
        let users = [
            piece_user(name, 0, index),
            piece_user(name, 1, index),
            legacy_user(name, index),
        ];
        for user in users.iter().filter(|u| u.as_str() != name) {
            if let Err(e) = vault.delete(service, user) {
                failure = Some(e);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

/// What this OS calls the place secrets live, for user-facing text.
pub fn store_name() -> &'static str {
    #[cfg(target_os = "linux")]
    if matches!(file_store_chosen(), Ok(true)) {
        return "secrets.json in the data folder, owner-only and unencrypted";
    }
    match std::env::consts::OS {
        "windows" => "Windows Credential Manager",
        "macos" => "the macOS Keychain",
        _ => "the system keyring (Secret Service or the kernel keyring)",
    }
}

/// Stores a secret in pieces, switching to it only when every piece is written.
pub fn store(service: &str, name: &str, secret: &str) -> Result<()> {
    let _lock = lock()?;
    with_vault(|v| store_in(v, service, name, secret))
}

/// The whole secret, or None when nothing is stored.
pub fn read(service: &str, name: &str) -> Result<Option<String>> {
    let _lock = lock()?;
    with_vault(|v| read_from(v, service, name))
}

/// Deletes the secret and every piece of it.
pub fn clear(service: &str, name: &str) -> Result<()> {
    let _lock = lock()?;
    with_vault(|v| clear_in(v, service, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    #[test]
    fn lock_file_admits_one_holder_at_a_time() {
        let dir = std::env::temp_dir().join(format!("chatcli-secrets-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = file_lock::try_acquire(&dir.join("secrets.lock")).unwrap();
        assert!(first.is_some());
        let held = file_lock::try_acquire(&dir.join("secrets.lock")).unwrap();
        assert!(held.is_none(), "a second holder must be refused");
        drop(first);
        assert!(lock_in(&dir).is_ok(), "freed after drop");
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

    /// An in-memory vault that can start failing writes, to simulate a process killed half way.
    #[derive(Default)]
    struct MemVault {
        map: RefCell<BTreeMap<String, String>>,
        writes_left: Cell<Option<usize>>,
    }

    impl Vault for MemVault {
        fn get(&self, _service: &str, user: &str) -> Result<Option<String>> {
            Ok(self.map.borrow().get(user).cloned())
        }
        fn set(&self, _service: &str, user: &str, value: &str) -> Result<()> {
            if let Some(n) = self.writes_left.get() {
                if n == 0 {
                    return Err("simulated crash".into());
                }
                self.writes_left.set(Some(n - 1));
            }
            self.map.borrow_mut().insert(user.into(), value.into());
            Ok(())
        }
        fn delete(&self, _service: &str, user: &str) -> Result<()> {
            self.map.borrow_mut().remove(user);
            Ok(())
        }
    }

    fn token(c: char, n: usize) -> String {
        c.to_string().repeat(n)
    }

    #[test]
    fn a_crash_at_any_point_leaves_the_old_or_the_new_secret_never_a_mix() {
        let old = token('o', 3300);
        let new = token('n', 4100);
        for crash_after in 0..12 {
            let v = MemVault::default();
            store_in(&v, "s", "tok", &old).unwrap();
            v.writes_left.set(Some(crash_after));
            let result = store_in(&v, "s", "tok", &new);
            v.writes_left.set(None);
            let got = read_from(&v, "s", "tok")
                .unwrap()
                .expect("a secret is always readable");
            if result.is_ok() {
                assert_eq!(got, new, "a finished write reads back new");
            } else {
                assert!(
                    got == old || got == new,
                    "crash after {crash_after} writes gave a mix"
                );
            }
            // A later successful write always repairs whatever the crash left.
            store_in(&v, "s", "tok", &new).unwrap();
            assert_eq!(
                read_from(&v, "s", "tok").unwrap().as_deref(),
                Some(new.as_str())
            );
        }
    }

    #[test]
    fn shorter_and_longer_secrets_round_trip_and_leave_no_old_tail() {
        let v = MemVault::default();
        assert_eq!(read_from(&v, "s", "tok").unwrap(), None);
        for len in [3300, 120, 1000, 1001, 16000, 1] {
            let secret = token('x', len);
            store_in(&v, "s", "tok", &secret).unwrap();
            assert_eq!(
                read_from(&v, "s", "tok").unwrap().as_deref(),
                Some(secret.as_str()),
                "len {len}"
            );
        }
        // Only the live generation's piece and the pointer remain.
        assert_eq!(
            v.map.borrow().len(),
            2,
            "{:?}",
            v.map.borrow().keys().collect::<Vec<_>>()
        );
        assert!(store_in(&v, "s", "tok", "").is_err());
        assert!(store_in(&v, "s", "tok", &token('x', 16001)).is_err());
        assert_eq!(
            read_from(&v, "s", "tok").unwrap().map(|s| s.len()),
            Some(1),
            "failed stores change nothing"
        );
    }

    #[test]
    fn secrets_written_before_the_pointer_layout_still_read_and_are_converted() {
        let v = MemVault::default();
        let secret = token('l', 2500);
        let parts = split_chunks(&secret, CHUNK_CHARS);
        v.map.borrow_mut().insert("tok".into(), parts[0].clone());
        v.map.borrow_mut().insert("tok-1".into(), parts[1].clone());
        v.map.borrow_mut().insert("tok-2".into(), parts[2].clone());
        assert_eq!(
            read_from(&v, "s", "tok").unwrap().as_deref(),
            Some(secret.as_str())
        );
        let replacement = token('r', 1500);
        store_in(&v, "s", "tok", &replacement).unwrap();
        assert_eq!(
            read_from(&v, "s", "tok").unwrap().as_deref(),
            Some(replacement.as_str())
        );
        assert!(!v.map.borrow().contains_key("tok-1"), "old pieces removed");
        assert!(!v.map.borrow().contains_key("tok-2"));
    }

    #[test]
    fn clear_removes_every_piece_and_a_missing_piece_reads_as_missing() {
        let v = MemVault::default();
        store_in(&v, "s", "tok", &token('a', 2500)).unwrap();
        store_in(&v, "s", "tok", &token('b', 2500)).unwrap();
        store_in(&v, "s", "other", "keep").unwrap();
        clear_in(&v, "s", "tok").unwrap();
        assert_eq!(read_from(&v, "s", "tok").unwrap(), None);
        assert_eq!(
            v.map.borrow().keys().cloned().collect::<Vec<_>>(),
            ["other", "other~0~0"]
        );
        // A piece someone deleted by hand: the secret reads as missing, never as a short token.
        store_in(&v, "s", "tok", &token('c', 2500)).unwrap();
        let victim = v
            .map
            .borrow()
            .keys()
            .find(|k| k.starts_with("tok~"))
            .cloned()
            .unwrap();
        v.map.borrow_mut().remove(&victim);
        assert_eq!(read_from(&v, "s", "tok").unwrap(), None);
    }

    #[test]
    fn pointer_parsing_rejects_garbage() {
        assert_eq!(parse_pointer("@v2:0:3"), Some((0, 3)));
        assert_eq!(parse_pointer("@v2:1:16"), Some((1, 16)));
        for bad in [
            "@v2:2:3", "@v2:0:0", "@v2:0:17", "@v2:0", "@v2:a:b", "token", "",
        ] {
            assert_eq!(parse_pointer(bad), None, "{bad:?}");
        }
    }

    #[cfg(target_os = "linux")]
    fn file_vault(test: &str) -> (PathBuf, FileVault) {
        let dir = std::env::temp_dir().join(format!(
            "chatcli-secrets-file-{test}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_STORE_NAME);
        (dir, FileVault { path })
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_vault_round_trips_long_secrets_and_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, v) = file_vault("round-trip");
        assert_eq!(
            read_from(&v, "s", "tok").unwrap(),
            None,
            "no file is no secret"
        );
        let long = token('x', 3300);
        store_in(&v, "s", "tok", &long).unwrap();
        store_in(&v, "s", "other", "keep").unwrap();
        assert_eq!(
            read_from(&v, "s", "tok").unwrap().as_deref(),
            Some(long.as_str())
        );
        store_in(&v, "s", "tok", "short").unwrap();
        assert_eq!(read_from(&v, "s", "tok").unwrap().as_deref(), Some("short"));
        clear_in(&v, "s", "tok").unwrap();
        assert_eq!(read_from(&v, "s", "tok").unwrap(), None);
        assert_eq!(
            read_from(&v, "s", "other").unwrap().as_deref(),
            Some("keep")
        );

        let mode = std::fs::metadata(&v.path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "only the owner may read the secrets file"
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_STORE_NAME], "no temp files are left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_vault_replaces_a_loose_file_and_never_overwrites_a_damaged_one() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, v) = file_vault("modes");
        std::fs::write(&v.path, "{}").unwrap();
        std::fs::set_permissions(&v.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        store_in(&v, "s", "tok", "secret").unwrap();
        let mode = std::fs::metadata(&v.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a write leaves an owner-only file");

        std::fs::write(&v.path, "not json").unwrap();
        assert!(read_from(&v, "s", "tok").is_err());
        assert!(store_in(&v, "s", "tok", "new").is_err());
        assert_eq!(std::fs::read_to_string(&v.path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(&dir);
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
