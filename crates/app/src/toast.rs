//! Desktop toast notifications (issue #24). The TUI decides whether and what to show; this is
//! the one place that talks to the operating system. Windows uses a WinRT toast; Linux sends a
//! freedesktop notification over the session D-Bus (`org.freedesktop.Notifications`), which any
//! notification daemon answers (dunst, mako, GNOME Shell, KDE Plasma). Elsewhere `notify` says it
//! is not built and the TUI logs that once.
//!
//! Toast text can appear on a locked screen. `[notifications] preview` defaults to full (the
//! text is shown); `"off"` or `"sender"` keeps message text off a locked screen. Nothing here
//! logs the toast text.

use std::sync::Arc;

use chatcli_core::runtime::{Notifier, Toast};
use chatcli_core::Result;

/// The app id Windows files our toasts under. An unpackaged program that does not register one
/// either gets no toast or shows the wrong app name.
#[cfg(windows)]
const APP_ID: &str = "chatcli";

pub fn system_notifier() -> Arc<dyn Notifier> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::DbusNotifier::start())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(SystemNotifier)
    }
}

#[cfg(not(target_os = "linux"))]
struct SystemNotifier;

#[cfg(not(target_os = "linux"))]
impl Notifier for SystemNotifier {
    fn notify(&self, toast: &Toast) -> Result<()> {
        show(toast)
    }
}

/// One line of sender-controlled text: control characters become spaces. With `escape`, the
/// markup specials are escaped too (`'` and `"` only when `quotes`).
#[cfg(any(windows, target_os = "linux", test))]
fn clean(s: &str, escape: bool, quotes: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' if escape => out.push_str("&amp;"),
            '<' if escape => out.push_str("&lt;"),
            '>' if escape => out.push_str("&gt;"),
            '"' if escape && quotes => out.push_str("&quot;"),
            '\'' if escape && quotes => out.push_str("&apos;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Toast XML. Pure, so the escaping is unit tested. Control characters are dropped and the
/// five XML specials escaped: a message sender controls this text.
#[cfg(any(windows, test))]
fn toast_xml(toast: &Toast) -> String {
    let audio = if toast.sound {
        ""
    } else {
        "<audio silent=\"true\"/>"
    };
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>",
        clean(&toast.title, true, true),
        clean(&toast.body, true, true),
    )
}

/// What goes into a freedesktop `Notify` call. Pure, so the escaping and hints are unit tested.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct FdoNote {
    /// Plain text by the spec: never markup.
    summary: String,
    /// Markup-escaped when the daemon reads body markup, else plain.
    body: String,
    /// `sound-name` to ask for, or `None` to send `suppress-sound`.
    sound_name: Option<&'static str>,
}

/// The freedesktop sound theme's name for an incoming instant message. The daemon (or a script
/// it runs) plays it; chatcli plays nothing itself.
#[cfg(any(target_os = "linux", test))]
const SOUND_NAME: &str = "message-new-instant";

/// Hint `category`: an instant message arrived.
#[cfg(any(target_os = "linux", test))]
const CATEGORY: &str = "im.received";

#[cfg(any(target_os = "linux", test))]
fn fdo_note(toast: &Toast, body_markup: bool) -> FdoNote {
    FdoNote {
        summary: clean(&toast.title, false, false),
        // Daemons that read body markup take `<b>`, `<a href>` and `<img>`: a sender must not
        // be able to put a link or a picture in our toast.
        body: clean(&toast.body, body_markup, false),
        sound_name: toast.sound.then_some(SOUND_NAME),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
    use std::time::Duration;

    use dbus::arg::{PropMap, RefArg, Variant};
    use dbus::blocking::Connection;

    use super::{fdo_note, Notifier, Result, Toast, CATEGORY};

    const DEST: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    /// A daemon that does not answer in this time counts as a failure. Only the worker waits.
    const TIMEOUT: Duration = Duration::from_secs(5);
    /// Toasts waiting for the worker. A hung daemon makes each one wait out `TIMEOUT`; past this
    /// many the rest are dropped rather than shown late in a burst.
    const QUEUE: usize = 8;

    /// Hands each toast to a worker thread that owns the D-Bus connection, so a slow or missing
    /// daemon never stalls the UI.
    pub struct DbusNotifier {
        tx: SyncSender<Toast>,
    }

    impl DbusNotifier {
        pub fn start() -> Self {
            let (tx, rx) = sync_channel::<Toast>(QUEUE);
            let spawned = std::thread::Builder::new()
                .name("notify".into())
                .spawn(move || {
                    let mut bus: Option<Bus> = None;
                    let mut failed = false;
                    for toast in rx {
                        if let Err(e) = send(&mut bus, &toast) {
                            // A broken connection is rebuilt on the next toast.
                            bus = None;
                            if !std::mem::replace(&mut failed, true) {
                                chatcli_core::log::error(
                                    "app",
                                    &format!(
                                        "desktop notification failed (later failures are not logged): {e}"
                                    ),
                                );
                            }
                        }
                    }
                });
            if spawned.is_err() {
                chatcli_core::log::error("app", "could not start the notification thread");
            }
            Self { tx }
        }
    }

    impl Notifier for DbusNotifier {
        fn notify(&self, toast: &Toast) -> Result<()> {
            self.tx.try_send(toast.clone()).map_err(|e| match e {
                TrySendError::Full(_) => {
                    "The notification service is not keeping up: toast dropped.".to_string()
                }
                TrySendError::Disconnected(_) => {
                    "The notification thread is not running.".to_string()
                }
            })
        }
    }

    struct Bus {
        conn: Connection,
        /// The daemon reads markup in the body (capability `body-markup`).
        body_markup: bool,
    }

    fn send(bus: &mut Option<Bus>, toast: &Toast) -> std::result::Result<(), String> {
        let fail = |e: dbus::Error| {
            format!(
                "the notification service refused it ({})",
                e.name().unwrap_or("no reply")
            )
        };
        if bus.is_none() {
            let conn = Connection::new_session().map_err(|e| {
                format!(
                    "no D-Bus session bus ({})",
                    e.name().unwrap_or("not reachable")
                )
            })?;
            let (caps,): (Vec<String>,) = conn
                .with_proxy(DEST, PATH, TIMEOUT)
                .method_call(DEST, "GetCapabilities", ())
                .map_err(fail)?;
            *bus = Some(Bus {
                conn,
                body_markup: caps.iter().any(|c| c == "body-markup"),
            });
        }
        let Some(b) = bus.as_ref() else {
            return Err("no D-Bus connection".into());
        };
        let note = fdo_note(toast, b.body_markup);
        let mut hints = PropMap::new();
        hints.insert(
            "category".into(),
            Variant(Box::new(CATEGORY.to_string()) as Box<dyn RefArg>),
        );
        match note.sound_name {
            Some(name) => hints.insert(
                "sound-name".into(),
                Variant(Box::new(name.to_string()) as Box<dyn RefArg>),
            ),
            None => hints.insert(
                "suppress-sound".into(),
                Variant(Box::new(true) as Box<dyn RefArg>),
            ),
        };
        let _: (u32,) = b
            .conn
            .with_proxy(DEST, PATH, TIMEOUT)
            .method_call(
                DEST,
                "Notify",
                (
                    "chatcli",            // app_name
                    0u32,                 // replaces_id: a new notification
                    "",                   // app_icon
                    note.summary,         // summary
                    note.body,            // body
                    Vec::<String>::new(), // actions
                    hints,
                    -1i32, // expire_timeout: the daemon's default
                ),
            )
            .map_err(fail)?;
        Ok(())
    }
}

#[cfg(windows)]
fn show(toast: &Toast) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let fail = |e: windows::core::Error| format!("Windows refused the notification ({e}).");
    register_app_id();
    let doc = XmlDocument::new().map_err(fail)?;
    doc.LoadXml(&HSTRING::from(toast_xml(toast)))
        .map_err(fail)?;
    let note = ToastNotification::CreateToastNotification(&doc).map_err(fail)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .map_err(fail)?
        .Show(&note)
        .map_err(fail)
}

/// Writes `HKCU\Software\Classes\AppUserModelId\chatcli\DisplayName` once per run so Windows
/// shows "chatcli" as the sender. Needs no admin rights. A failure is not fatal: the toast then
/// shows under a generic name or Windows drops it, and the caller logs the toast error.
#[cfg(windows)]
fn register_app_id() {
    use std::sync::Once;
    use windows::core::w;
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let name: Vec<u16> = "chatcli\0".encode_utf16().collect();
        // SAFETY: `name` outlives the call and the byte length covers it exactly.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Classes\\AppUserModelId\\chatcli"),
                w!("DisplayName"),
                REG_SZ.0,
                Some(name.as_ptr().cast()),
                (name.len() * 2) as u32,
            )
        };
        if status.0 != 0 {
            chatcli_core::log::error("app", "could not register the notification app name");
        }
    });
}

#[cfg(not(any(windows, target_os = "linux")))]
fn show(_toast: &Toast) -> Result<()> {
    Err("Desktop notifications are only built for Windows and Linux.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(title: &str, body: &str, sound: bool) -> Toast {
        Toast {
            title: title.into(),
            body: body.into(),
            sound,
        }
    }

    #[test]
    fn xml_escapes_what_a_sender_controls() {
        let xml = toast_xml(&toast(
            "A <b>&\"'",
            "x</text><audio src=\"evil\"/>\u{7}\nok",
            true,
        ));
        assert!(!xml.contains("<b>") && !xml.contains("evil\"/>"), "{xml}");
        assert!(xml.contains("A &lt;b&gt;&amp;&quot;&apos;"), "{xml}");
        assert!(xml.contains("x&lt;/text&gt;"), "{xml}");
        assert!(!xml.contains('\u{7}') && !xml.contains('\n'), "{xml}");
        assert_eq!(xml.matches("<text>").count(), 2);
    }

    #[test]
    fn sound_off_adds_a_silent_audio_element() {
        assert!(toast_xml(&toast("t", "b", false)).contains("<audio silent=\"true\"/>"));
        assert!(!toast_xml(&toast("t", "b", true)).contains("<audio"));
    }

    #[test]
    fn fdo_body_markup_is_escaped_only_when_the_daemon_reads_it() {
        let t = toast(
            "Tom & <Jerry>",
            "Sam: <a href=\"x\">hi</a> & 'bye'\u{1b}\nok",
            true,
        );
        let marked = fdo_note(&t, true);
        assert_eq!(
            marked.body,
            "Sam: &lt;a href=\"x\"&gt;hi&lt;/a&gt; &amp; 'bye'  ok"
        );
        let plain = fdo_note(&t, false);
        assert_eq!(plain.body, "Sam: <a href=\"x\">hi</a> & 'bye'  ok");
        // The summary is plain text by the spec, either way.
        assert_eq!(marked.summary, "Tom & <Jerry>");
        assert_eq!(plain.summary, marked.summary);
    }

    /// Shows a real notification through the session's daemon. By hand only:
    /// `cargo test -p chatcli -- --ignored live_notification`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a desktop session with a notification daemon"]
    fn live_notification() {
        let n = linux::DbusNotifier::start();
        n.notify(&toast("chatcli test", "Sam: <b>not bold</b> & ok", true))
            .unwrap();
        // The worker sends it; give it time before the process exits.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[test]
    fn fdo_sound_asks_for_the_im_sound_or_suppresses_it() {
        assert_eq!(
            fdo_note(&toast("t", "b", true), true).sound_name,
            Some(SOUND_NAME)
        );
        assert_eq!(fdo_note(&toast("t", "b", false), true).sound_name, None);
        assert_eq!(SOUND_NAME, "message-new-instant");
        assert_eq!(CATEGORY, "im.received");
    }
}
