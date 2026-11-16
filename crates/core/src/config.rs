//! Non-secret settings in `config.json` in the data folder.
//!
//! Every user registers their own Microsoft Entra app and deploys their own relay, so no app
//! id, tenant or URL is built into the binary. Secrets (tokens) never go here: they live in
//! Windows Credential Manager (`secrets`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Application (client) id of the user's own Entra app registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teams_client_id: Option<String>,
    /// Directory (tenant) id or domain the app is registered in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teams_tenant_id: Option<String>,
    /// Base URL of the user's relay Worker, e.g. `https://chatcli-relay.example.workers.dev`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

pub fn load() -> Result<Config> {
    load_from(&crate::paths::config_file()?)
}

pub fn save(config: &Config) -> Result<()> {
    save_to(&crate::paths::config_file()?, config)
}

/// A missing file is an empty config. A broken file is an error naming the file, never a
/// silent reset (that would quietly drop the user's setup).
pub fn load_from(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not valid settings JSON ({e}). Fix or delete it.",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn save_to(path: &Path, config: &Config) -> Result<()> {
    let text = serde_json::to_string_pretty(config).map_err(|e| format!("settings: {e}"))?;
    write_atomic(path, &text)
}

/// Writes a temp file then renames it, so a crash mid-write cannot leave half a file. The temp
/// file is this save's own (process id + counter, created exclusively): CLI setup commands run
/// without the instance lock, so two saves at once must never share one temp file, or one would
/// rename the other's content and the second rename would fail. The temp file is removed if
/// writing or renaming fails, and the old file is then untouched.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    write_atomic_as(path, text, false)
}

/// `write_atomic` for a file only its owner may read: on Unix the temp file is created with
/// mode 0600, so the content is never readable by anyone else, not even before the rename.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    write_atomic_as(path, text, true)
}

fn write_atomic_as(path: &Path, text: &str, private: bool) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SAVES: AtomicU64 = AtomicU64::new(0);

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy())
        .unwrap_or_default();
    let tmp = path.with_extension(format!(
        "{ext}.{}.{}.tmp",
        std::process::id(),
        SAVES.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

/// 8-4-4-4-12 hex digits, the shape of every Entra id.
pub fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A tenant is a GUID or a verified domain (`contoso.onmicrosoft.com`). The value goes into
/// the sign-in URL path, so only these characters are allowed.
pub fn is_tenant(s: &str) -> bool {
    is_guid(s)
        || (s.contains('.')
            && s.len() <= 255
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
}

/// `https://host` only: no path, query or credentials. Returned without a trailing slash.
pub fn normalize_relay_url(s: &str) -> Result<String> {
    let s = s.trim().trim_end_matches('/');
    let host = s
        .strip_prefix("https://")
        .ok_or("The relay URL must start with https://")?;
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
    {
        return Err("The relay URL must be just https://<host>, with no path or query".into());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_and_tenant_shapes() {
        assert!(is_guid("00000000-0000-0000-0000-000000000000"));
        assert!(is_guid("aBcDeF01-2345-6789-abcd-ef0123456789"));
        assert!(!is_guid("0000000-0000-0000-0000-000000000000"));
        assert!(!is_guid("zzzzzzzz-0000-0000-0000-000000000000"));
        assert!(is_tenant("contoso.onmicrosoft.com"));
        assert!(is_tenant("00000000-0000-0000-0000-000000000000"));
        assert!(!is_tenant("contoso/../x"));
        assert!(
            !is_tenant("common"),
            "a named tenant is required, not the multi-tenant alias"
        );
    }

    #[test]
    fn relay_url_is_https_host_only() {
        assert_eq!(
            normalize_relay_url(" https://relay.example.workers.dev/ ").unwrap(),
            "https://relay.example.workers.dev"
        );
        assert!(normalize_relay_url("http://relay.example.dev").is_err());
        assert!(normalize_relay_url("https://relay.example.dev/path").is_err());
        assert!(normalize_relay_url("https://user:pw@relay.example.dev").is_err());
        assert!(normalize_relay_url("https://").is_err());
    }

    #[test]
    fn missing_file_is_empty_broken_file_is_an_error_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("chatcli-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path).unwrap(), Config::default());
        let config = Config {
            teams_client_id: Some("00000000-0000-0000-0000-000000000001".into()),
            teams_tenant_id: Some("contoso.onmicrosoft.com".into()),
            relay_url: None,
        };
        save_to(&path, &config).unwrap();
        assert_eq!(load_from(&path).unwrap(), config);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_from(&path)
            .unwrap_err()
            .contains("not valid settings JSON"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_never_share_a_temp_file_and_leave_none_behind() {
        let dir = std::env::temp_dir().join(format!("chatcli-config-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let config = Config {
                        relay_url: Some(format!("https://relay{i}.example.invalid")),
                        ..Config::default()
                    };
                    for _ in 0..20 {
                        save_to(&path, &config)?;
                    }
                    Ok::<(), String>(())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every save succeeds");
        }
        // The file is one whole config (the last writer's), never a mix.
        let loaded = load_from(&path).unwrap();
        assert!(loaded.relay_url.unwrap().starts_with("https://relay"));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rename_removes_its_temp_file() {
        let dir = std::env::temp_dir().join(format!("chatcli-config-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The target is a non-empty folder, so the rename cannot replace it.
        let path = dir.join("config.json");
        std::fs::create_dir_all(path.join("inner")).unwrap();
        assert!(save_to(&path, &Config::default()).is_err());
        let tmps = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(tmps, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
