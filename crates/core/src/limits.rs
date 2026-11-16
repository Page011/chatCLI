//! Size limits shared by the TUI (pre-check before sending) and the backends (the real check).

use crate::model::{MediaKind, Network};

/// Teams advises keeping a message body under 80 KB. Counted on the HTML body after escaping.
pub const TEAMS_TEXT_MAX_BYTES: usize = 80 * 1024;
/// Provisional: the WhatsApp backend measures the real limit and reports it.
pub const WHATSAPP_TEXT_MAX_CHARS: usize = 60_000;

const MIB: u64 = 1024 * 1024;

/// The HTML for one character of a Teams message, or None when it is sent as is. `\r` is dropped
/// so a Windows line break becomes one `<br>`.
fn escape_char(c: char) -> Option<&'static str> {
    Some(match c {
        '&' => "&amp;",
        '<' => "&lt;",
        '>' => "&gt;",
        '"' => "&quot;",
        '\'' => "&#39;",
        '\n' => "<br>",
        '\r' => "",
        _ => return None,
    })
}

/// The Teams HTML body for `text`: `& < > " '` escaped, line breaks as `<br>`. The Teams body
/// builder should use this, so the length the TUI checks and the body that is sent cannot drift.
pub fn teams_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match escape_char(c) {
            Some(s) => out.push_str(s),
            None => out.push(c),
        }
    }
    out
}

/// Length in bytes of the Teams HTML body for `text`. Equals `teams_escape(text).len()`.
pub fn teams_body_len(text: &str) -> usize {
    text.chars()
        .map(|c| escape_char(c).map_or(c.len_utf8(), str::len))
        .sum()
}

/// Teams: the escaped body must fit `TEAMS_TEXT_MAX_BYTES`. WhatsApp: the character count must
/// fit `WHATSAPP_TEXT_MAX_CHARS`.
pub fn text_fits(network: Network, text: &str) -> bool {
    match network {
        Network::Teams => teams_body_len(text) <= TEAMS_TEXT_MAX_BYTES,
        Network::WhatsApp => text.chars().count() <= WHATSAPP_TEXT_MAX_CHARS,
    }
}

/// The TUI's pre-check before `SendMedia`; the backend checks again.
/// Teams: image 4 MiB, other 100 MiB. WhatsApp: image 16 MiB, other 100 MiB.
pub fn send_limit_bytes(network: Network, kind: MediaKind) -> u64 {
    match (network, kind) {
        (Network::Teams, MediaKind::Image) => 4 * MIB,
        (Network::WhatsApp, MediaKind::Image) => 16 * MIB,
        _ => 100 * MIB,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teams_body_len_matches_the_escaped_body_exactly() {
        for text in [
            "",
            "plain",
            "a & b < c > d \"e\" 'f'",
            "line one\nline two\r\nthree",
            "caf\u{e9} \u{1F600} \u{4e2d}\u{6587}",
            "<<<<>>>>&&&&",
        ] {
            assert_eq!(teams_body_len(text), teams_escape(text).len(), "{text:?}");
        }
        assert_eq!(teams_escape("a\r\nb"), "a<br>b");
        assert_eq!(teams_escape("<&>"), "&lt;&amp;&gt;");
        assert_eq!(teams_body_len("\n"), 4);
    }

    #[test]
    fn teams_limit_counts_escaped_bytes_not_characters() {
        // 80 KB of '<' is 4x that once escaped.
        let at_limit = "a".repeat(TEAMS_TEXT_MAX_BYTES);
        assert!(text_fits(Network::Teams, &at_limit));
        assert!(!text_fits(Network::Teams, &(at_limit.clone() + "a")));
        assert!(!text_fits(
            Network::Teams,
            &"<".repeat(TEAMS_TEXT_MAX_BYTES / 2)
        ));
        assert!(text_fits(
            Network::Teams,
            &"<".repeat(TEAMS_TEXT_MAX_BYTES / 4)
        ));
    }

    #[test]
    fn whatsapp_limit_counts_characters() {
        let at_limit = "\u{e9}".repeat(WHATSAPP_TEXT_MAX_CHARS);
        assert!(text_fits(Network::WhatsApp, &at_limit));
        assert!(!text_fits(Network::WhatsApp, &(at_limit + "x")));
    }

    #[test]
    fn send_limits_per_network_and_kind() {
        assert_eq!(send_limit_bytes(Network::Teams, MediaKind::Image), 4 << 20);
        assert_eq!(
            send_limit_bytes(Network::Teams, MediaKind::Document),
            100 << 20
        );
        assert_eq!(
            send_limit_bytes(Network::WhatsApp, MediaKind::Image),
            16 << 20
        );
        assert_eq!(
            send_limit_bytes(Network::WhatsApp, MediaKind::Video),
            100 << 20
        );
    }
}
