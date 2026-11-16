//! Where chatcli keeps its files (store, WhatsApp session, log, config): `%LOCALAPPDATA%\chatcli`
//! on Windows, see `os_data_dir` for macOS and Linux. `CHATCLI_DATA_DIR` overrides it, so tests never touch the real data.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Result;

pub fn data_dir() -> Result<PathBuf> {
    let dir = data_dir_path()?;
    create_private_dir(&dir)
        .map_err(|e| format!("cannot create data folder {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Where the data folder is, without creating it (for `--help` and the About page).
pub fn data_dir_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CHATCLI_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        // A test that forgot CHATCLI_DATA_DIR must not write into the user's real log, store
        // or session. Cargo puts every test binary in a `deps` folder; the app never runs there.
        None if is_test_binary() => {
            std::env::temp_dir().join(format!("chatcli-test-{}", std::process::id()))
        }
        None => os_data_dir(std::env::consts::OS, |k| std::env::var_os(k))?,
    };
    Ok(dir)
}

/// Creates the folder (and parents). On Linux and macOS it is mode 0700, and an existing folder
/// that others can read is tightened: it holds message text, the WhatsApp device keys and the
/// media cache. Windows keeps the per-user ACL of `%LOCALAPPDATA%`.
#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The per-user data folder of each OS, the same places the `dirs` crate would pick (written by
/// hand: three lines of env lookup do not justify a dependency):
/// - Windows: `%LOCALAPPDATA%\chatcli`. Unchanged since 0.1, so existing users keep their data.
/// - macOS: `~/Library/Application Support/chatcli`.
/// - Linux and other Unix: `$XDG_DATA_HOME/chatcli` (only when absolute, per the XDG spec),
///   else `~/.local/share/chatcli`.
///
/// `os` is `std::env::consts::OS` and `get` reads the environment; both are parameters so every
/// branch is unit-tested on any OS.
fn os_data_dir(os: &str, get: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let non_empty = |k: &str| get(k).filter(|v| !v.is_empty());
    let home = || {
        non_empty("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set, so chatcli has nowhere to keep its data")
    };
    let dir = match os {
        "windows" => PathBuf::from(
            non_empty("LOCALAPPDATA")
                .ok_or("LOCALAPPDATA is not set, so chatcli has nowhere to keep its data")?,
        ),
        "macos" => home()?.join("Library").join("Application Support"),
        _ => match non_empty("XDG_DATA_HOME").map(PathBuf::from) {
            Some(xdg) if xdg.is_absolute() => xdg,
            _ => home()?.join(".local").join("share"),
        },
    };
    Ok(dir.join("chatcli"))
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

/// The log file's path without creating the data folder.
pub fn log_file_path() -> Result<PathBuf> {
    Ok(data_dir_path()?.join("chatcli.log"))
}

/// Non-secret settings (relay URL). Secrets live in Credential Manager.
pub fn config_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("config.json"))
}

/// `settings.toml`: hand-edited, and written only by the settings screen (see `settings`).
pub fn settings_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("settings.toml"))
}

/// The downloaded-media cache folder. Not encrypted: it holds what the user chose to open.
pub fn media_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("media"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn each_os_gets_its_own_data_folder() {
        let d = |os, pairs: &[(&str, &str)]| super::os_data_dir(os, env(pairs));
        // Windows must stay exactly %LOCALAPPDATA%\chatcli so 0.1 users keep their data.
        assert_eq!(
            d(
                "windows",
                &[("LOCALAPPDATA", "C:\\Users\\Alex\\AppData\\Local")]
            )
            .unwrap(),
            PathBuf::from("C:\\Users\\Alex\\AppData\\Local").join("chatcli")
        );
        assert!(d("windows", &[("HOME", "/home/alex")]).is_err());
        assert_eq!(
            d("macos", &[("HOME", "/Users/alex")]).unwrap(),
            PathBuf::from("/Users/alex/Library/Application Support/chatcli")
        );
        assert_eq!(
            d("linux", &[("HOME", "/home/alex")]).unwrap(),
            PathBuf::from("/home/alex/.local/share/chatcli")
        );
        // XDG_DATA_HOME wins when absolute; a relative or empty one is ignored (XDG spec).
        let xdg = if cfg!(windows) { "C:\\xdg" } else { "/xdg" };
        assert_eq!(
            d("linux", &[("HOME", "/home/alex"), ("XDG_DATA_HOME", xdg)]).unwrap(),
            PathBuf::from(xdg).join("chatcli")
        );
        for bad in ["relative/dir", ""] {
            assert_eq!(
                d("linux", &[("HOME", "/home/alex"), ("XDG_DATA_HOME", bad)]).unwrap(),
                PathBuf::from("/home/alex/.local/share/chatcli"),
                "{bad:?}"
            );
        }
        assert!(d("linux", &[]).is_err());
        assert!(d("macos", &[("HOME", "")]).is_err());
    }

    /// #40: the data folder is private on Linux and macOS, and a loose one is tightened.
    #[cfg(unix)]
    #[test]
    fn the_data_folder_is_private_on_unix() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!("chatcli-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let fresh = base.join("a").join("chatcli");
        super::create_private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        let loose = base.join("loose");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&loose)
            .unwrap();
        super::create_private_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

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
