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
    const BUS: &str = "org.freedesktop.DBus";
    const BUS_PATH: &str = "/org/freedesktop/DBus";
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
        /// The daemon whose capabilities are known: its unique bus name (`:1.38`) and whether it
        /// reads markup in the body (capability `body-markup`). Another daemon can take over the
        /// well-known name while we run (dunst restarted, swapped for mako), so every call goes
        /// to this unique name: a call can never reach a daemon we have not asked.
        daemon: Option<(String, bool)>,
    }

    fn failed(what: &'static str) -> impl Fn(dbus::Error) -> String {
        move |e| format!("{what} ({})", e.name().unwrap_or("no reply"))
    }

    /// The unique name that owns `org.freedesktop.Notifications` now, starting the daemon by
    /// D-Bus activation when none runs.
    fn daemon_owner(conn: &Connection) -> std::result::Result<String, String> {
        let bus = conn.with_proxy(BUS, BUS_PATH, TIMEOUT);
        let owner = || {
            bus.method_call(BUS, "GetNameOwner", (DEST,))
                .map(|(o,): (String,)| o)
        };
        match owner() {
            Ok(o) => Ok(o),
            Err(_) => {
                let _: (u32,) = bus
                    .method_call(BUS, "StartServiceByName", (DEST, 0u32))
                    .map_err(failed("no notification service"))?;
                owner().map_err(failed("no notification service"))
            }
        }
    }

    fn send(bus: &mut Option<Bus>, toast: &Toast) -> std::result::Result<(), String> {
        if bus.is_none() {
            let conn = Connection::new_session().map_err(failed("no D-Bus session bus"))?;
            *bus = Some(Bus { conn, daemon: None });
        }
        let Some(b) = bus.as_mut() else {
            return Err("no D-Bus connection".into());
        };
        let owner = daemon_owner(&b.conn)?;
        let body_markup = match &b.daemon {
            Some((known, markup)) if *known == owner => *markup,
            _ => {
                let (caps,): (Vec<String>,) = b
                    .conn
                    .with_proxy(owner.as_str(), PATH, TIMEOUT)
                    .method_call(DEST, "GetCapabilities", ())
                    .map_err(failed("the notification service did not answer"))?;
                let markup = caps.iter().any(|c| c == "body-markup");
                b.daemon = Some((owner.clone(), markup));
                markup
            }
        };
        let note = fdo_note(toast, body_markup);
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
            .with_proxy(owner.as_str(), PATH, TIMEOUT)
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
            .map_err(failed("the notification service refused it"))?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::process::{Child, Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::{channel, Receiver};
        use std::sync::Arc;
        use std::thread::JoinHandle;

        use dbus::channel::MatchingReceiver;
        use dbus::message::MatchRule;

        use super::*;

        /// A private bus, so the test never touches the desktop's daemon. `None` (test skipped)
        /// where no `dbus-daemon` is installed.
        /// Killed on drop, so a failing test leaves no daemon behind.
        struct PrivateBus(Child);

        impl Drop for PrivateBus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn private_bus() -> Option<(PrivateBus, String)> {
            let mut bus = PrivateBus(
                Command::new("dbus-daemon")
                    .args(["--session", "--nofork", "--print-address"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()?,
            );
            let mut line = String::new();
            std::io::BufRead::read_line(
                &mut std::io::BufReader::new(bus.0.stdout.take()?),
                &mut line,
            )
            .ok()?;
            let addr = line.trim().to_string();
            (!addr.is_empty()).then_some((bus, addr))
        }

        struct Fake {
            stop: Arc<AtomicBool>,
            thread: JoinHandle<()>,
            bodies: Receiver<String>,
        }

        impl Fake {
            /// Owns `org.freedesktop.Notifications` on `addr` before it returns.
            fn start(addr: &str, caps: &'static [&'static str]) -> Fake {
                let (ready_tx, ready) = channel();
                let (body_tx, bodies) = channel();
                let stop = Arc::new(AtomicBool::new(false));
                let flag = stop.clone();
                let addr = addr.to_string();
                let thread = std::thread::spawn(move || {
                    let conn = Connection::new_address(&addr).unwrap();
                    conn.request_name(DEST, false, false, true).unwrap();
                    conn.start_receive(
                        MatchRule::new_method_call(),
                        Box::new(move |msg, c| {
                            let reply = match msg.member().as_deref() {
                                Some("GetCapabilities") => msg.method_return().append1(
                                    caps.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                                ),
                                Some("Notify") => {
                                    let (_, _, _, _, body): (String, u32, String, String, String) =
                                        msg.read5().unwrap();
                                    body_tx.send(body).unwrap();
                                    msg.method_return().append1(1u32)
                                }
                                _ => return true,
                            };
                            let _ = dbus::channel::Sender::send(c, reply);
                            true
                        }),
                    );
                    ready_tx.send(()).unwrap();
                    while !flag.load(Ordering::Relaxed) {
                        conn.process(Duration::from_millis(20)).unwrap();
                    }
                });
                ready.recv().unwrap();
                Fake {
                    stop,
                    thread,
                    bodies,
                }
            }

            /// Leaves the bus, which gives up the name.
            fn quit(self) {
                self.stop.store(true, Ordering::Relaxed);
                self.thread.join().unwrap();
            }
        }

        #[test]
        fn a_daemon_that_takes_over_the_name_gets_its_own_capability_check() {
            let Some((_daemon, addr)) = private_bus() else {
                eprintln!("no dbus-daemon: skipped");
                return;
            };
            let toast = Toast {
                title: "t".into(),
                body: "<b>x</b>".into(),
                sound: true,
            };
            let mut bus = Some(Bus {
                conn: Connection::new_address(&addr).unwrap(),
                daemon: None,
            });

            let plain = Fake::start(&addr, &[]);
            send(&mut bus, &toast).unwrap();
            assert_eq!(plain.bodies.recv().unwrap(), "<b>x</b>");
            plain.quit();

            // Same connection, new daemon that reads markup: the text must be escaped for it.
            let marked = Fake::start(&addr, &["body", "body-markup"]);
            send(&mut bus, &toast).unwrap();
            assert_eq!(marked.bodies.recv().unwrap(), "&lt;b&gt;x&lt;/b&gt;");
            marked.quit();
        }
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
