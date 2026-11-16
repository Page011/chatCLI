//! Desktop toast notifications (issue #24). The TUI decides whether and what to show; this is
//! the one place that talks to the operating system. Windows only: elsewhere `notify` says so
//! once and the TUI logs it once.
//!
//! Toast text can appear on a locked screen, which is why `[notifications] preview` defaults to
//! off. Nothing here logs the toast text.

use std::sync::Arc;

use chatcli_core::runtime::{Notifier, Toast};
use chatcli_core::Result;

/// The app id Windows files our toasts under. An unpackaged program that does not register one
/// either gets no toast or shows the wrong app name.
#[cfg(windows)]
const APP_ID: &str = "chatcli";

pub fn system_notifier() -> Arc<dyn Notifier> {
    Arc::new(SystemNotifier)
}

struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn notify(&self, toast: &Toast) -> Result<()> {
        show(toast)
    }
}

/// Toast XML. Pure, so the escaping is unit tested. Control characters are dropped and the
/// five XML specials escaped: a message sender controls this text.
#[cfg(any(windows, test))]
fn toast_xml(toast: &Toast) -> String {
    fn clean(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                c if c.is_control() => out.push(' '),
                c => out.push(c),
            }
        }
        out
    }
    let audio = if toast.sound {
        ""
    } else {
        "<audio silent=\"true\"/>"
    };
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>",
        clean(&toast.title),
        clean(&toast.body),
    )
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

#[cfg(not(windows))]
fn show(_toast: &Toast) -> Result<()> {
    Err("Desktop notifications are only built for Windows.".to_string())
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
}
