//! What the app hands to each backend and to the TUI at start. Plain data, no behaviour: the
//! app crate builds these in `main.rs`, so tests can build them with a temp folder.

use std::path::PathBuf;
use std::sync::Arc;

use crate::media::MediaCache;
use crate::settings::Settings;
use crate::store::Store;
use crate::Result;

/// Everything one network backend needs besides its channels.
pub struct BackendCtx {
    /// The backend's own connection.
    pub store: Store,
    pub settings: Arc<Settings>,
    pub media: MediaCache,
}

/// One line for the TUI's notice bar at start (settings problems and the like).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupNotice {
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub body: String,
    pub sound: bool,
}

/// Shows a desktop notification. The TUI never links a toast crate: the app crate provides the
/// implementation. The log never holds toast text.
pub trait Notifier: Send + Sync {
    /// Must not block for long. An error is logged once by the caller, never shown in a loop.
    fn notify(&self, toast: &Toast) -> Result<()>;
}

/// Does nothing. The default for tests and `--demo`.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _toast: &Toast) -> Result<()> {
        Ok(())
    }
}

pub struct UiCtx {
    pub settings: Arc<Settings>,
    /// Settings problems and the like, shown in the notice line at start.
    pub startup_notices: Vec<StartupNotice>,
    /// The TUI's own connection: search, emoji recents (its only writes) and the members cache.
    /// Never used for message history: that comes through `OpenChat` / `LoadOlder`.
    pub store: Store,
    pub media: MediaCache,
    pub notifier: Arc<dyn Notifier>,
    pub demo: bool,
    /// The file the settings screen saves to. None = changes are not saved (tests). The demo
    /// points this at its own temp copy, never the user's file.
    pub settings_path: Option<PathBuf>,
    /// `--tour`: play the guided tour at start.
    pub tour: bool,
}

impl UiCtx {
    /// Default settings and `NoNotifier`: for tests and simple callers.
    pub fn new(store: Store, media: MediaCache) -> UiCtx {
        UiCtx {
            settings: Arc::new(Settings::default()),
            startup_notices: Vec::new(),
            store,
            media,
            notifier: Arc::new(NoNotifier),
            demo: false,
            settings_path: None,
            tour: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_ctx_new_uses_defaults_and_a_silent_notifier() {
        let dir = std::env::temp_dir().join(format!("chatcli-runtime-test-{}", std::process::id()));
        let media = MediaCache::open(&dir, &Settings::default().media).unwrap();
        let ctx = UiCtx::new(Store::open_in_memory().unwrap(), media);
        assert_eq!(*ctx.settings, Settings::default());
        assert!(ctx.startup_notices.is_empty() && !ctx.demo);
        assert!(ctx.settings_path.is_none() && !ctx.tour);
        let toast = Toast {
            title: "t".into(),
            body: "b".into(),
            sound: false,
        };
        assert!(ctx.notifier.notify(&toast).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
