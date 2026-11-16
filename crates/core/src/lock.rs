//! Cross-process file lock for the single-instance check and the credential-store guard.
//!
//! Uses the standard library's advisory lock (`File::try_lock`: LockFileEx on Windows, flock on
//! Linux and macOS), so no extra crate is needed. The OS drops the lock when the process dies,
//! so a crash never leaves a stale lock behind. The lock lives as long as the returned `File`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// Takes an exclusive lock on `path` (created if missing) without waiting.
/// `Ok(Some(file))` = we hold it, `Ok(None)` = another holder has it, `Err` = the lock file
/// could not be opened or locked for another reason (read-only folder, no access).
pub fn try_acquire(path: &Path) -> io::Result<Option<File>> {
    let file = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        // chatcli 0.1 held this file with Windows share mode 0, so while a 0.1 instance runs
        // even opening it fails with ERROR_SHARING_VIOLATION: that is "held", not a fault.
        Err(e) if is_sharing_violation(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn is_sharing_violation(e: &io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_admits_one_holder_at_a_time_and_frees_on_drop() {
        let dir = std::env::temp_dir().join(format!("chatcli-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chatcli.lock");
        let first = try_acquire(&path).unwrap();
        assert!(first.is_some(), "a free lock is taken");
        assert!(
            try_acquire(&path).unwrap().is_none(),
            "a held lock reports None, not an error"
        );
        drop(first);
        assert!(try_acquire(&path).unwrap().is_some(), "dropping frees it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_is_an_error_not_a_held_lock() {
        let path = std::env::temp_dir()
            .join(format!("chatcli-lock-none-{}", std::process::id()))
            .join("no-such-folder")
            .join("x.lock");
        assert!(try_acquire(&path).is_err());
    }
}
