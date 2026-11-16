//! The JSON bodies chatcli sends to Graph, built as pure functions so every request shape is
//! unit tested without a network. Text is always HTML with escaped content: the escape comes
//! from `chatcli_core::limits::teams_escape`, so the length the TUI checks is the length sent.

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};

use chatcli_core::config::is_guid;
use chatcli_core::limits::teams_escape;
use chatcli_core::model::PersonRef;

/// Graph's temporary id that ties the `<img>` in the body to its `hostedContents` entry.
const IMAGE_TEMP_ID: &str = "1";
/// Longest OneDrive file name chatcli will ask for (OneDrive allows 255; keep room for a
/// conflict suffix).
const DRIVE_NAME_MAX: usize = 150;

/// An HTML message body plus the `mentions` entries that go with its `<at>` tags.
#[derive(Debug, PartialEq)]
pub struct Built {
    pub html: String,
    pub mentions: Vec<Value>,
    /// The people whose token was found in the text, in the order of the `mentions` entries.
    pub used: Vec<PersonRef>,
}

/// Escapes `text` and turns each mention into a real one. For every mention, longest name first
/// (so `@Alex Example` is never taken for `@Al`), the first not-yet-used `"@" + name` in `text`
/// that is not followed by a letter or digit becomes `<at id="n">name</at>` and gets a `mentions`
/// entry. A mention whose token is not in the text is dropped: the text goes out as typed.
/// With no mentions the HTML is exactly `teams_escape(text)`.
pub fn text_body(text: &str, mentions: &[PersonRef]) -> Built {
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    let mut kept: Vec<&PersonRef> = Vec::new();
    let mut by_length: Vec<&PersonRef> = mentions.iter().collect();
    // Stable: equal lengths keep list order.
    by_length.sort_by_key(|p| std::cmp::Reverse(p.name.chars().count()));
    for person in by_length {
        if person.id.is_empty() || person.name.is_empty() {
            continue;
        }
        let token = format!("@{}", person.name);
        let free = text
            .match_indices(&token)
            .map(|(at, _)| (at, at + token.len()))
            .filter(|(_, e)| !text[*e..].chars().next().is_some_and(char::is_alphanumeric))
            .find(|(s, e)| ranges.iter().all(|(rs, re, _)| e <= rs || s >= re));
        if let Some((start, end)) = free {
            ranges.push((start, end, kept.len()));
            kept.push(person);
        }
    }
    ranges.sort_by_key(|r| r.0);
    let mut html = String::with_capacity(text.len() + 16 * ranges.len());
    let mut pos = 0;
    for (start, end, n) in &ranges {
        html.push_str(&teams_escape(&text[pos..*start]));
        html.push_str(&format!(
            "<at id=\"{n}\">{}</at>",
            teams_escape(&kept[*n].name)
        ));
        pos = *end;
    }
    html.push_str(&teams_escape(&text[pos..]));
    let mentions = kept
        .iter()
        .enumerate()
        .map(|(n, p)| {
            json!({
                "id": n,
                "mentionText": p.name,
                "mentioned": { "user": {
                    "id": p.id,
                    "displayName": p.name,
                    "userIdentityType": "aadUser",
                } },
            })
        })
        .collect();
    let used = kept.into_iter().cloned().collect();
    Built {
        html,
        mentions,
        used,
    }
}

fn html_body(html: &str) -> Value {
    json!({ "contentType": "html", "content": html })
}

/// The chatMessage JSON for a text message (`POST /chats/{id}/messages`, or the `replyMessage`
/// of a reply).
pub fn message_json(built: &Built) -> Value {
    let mut message = json!({ "body": html_body(&built.html) });
    if !built.mentions.is_empty() {
        message["mentions"] = Value::Array(built.mentions.clone());
    }
    message
}

/// `POST /chats/{id}/messages/replyWithQuote`. One quoted message; `replyToId` is for channels.
pub fn reply_json(quoted_id: &str, reply_message: Value) -> Value {
    json!({ "messageIds": [quoted_id], "replyMessage": reply_message })
}

/// `PATCH /chats/{id}/messages/{id}`: the body changes, and the mentions go with it so the
/// people still count as mentioned.
pub fn edit_json(built: &Built) -> Value {
    message_json(built)
}

/// An image sent inline: the caption (if any), then the picture as hosted content.
pub fn image_message_json(caption: &str, mime: &str, bytes: &[u8]) -> Value {
    let img = format!("<img src=\"../hostedContents/{IMAGE_TEMP_ID}/$value\">");
    let html = if caption.is_empty() {
        img
    } else {
        format!("{}<br>{img}", teams_escape(caption))
    };
    json!({
        "body": html_body(&html),
        "hostedContents": [{
            "@microsoft.graph.temporaryId": IMAGE_TEMP_ID,
            "contentBytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            "contentType": mime,
        }],
    })
}

/// A file already in OneDrive, linked as a `reference` attachment. `attachment_id` goes in the
/// body tag and in the attachment; `content_url` is the item's webDavUrl.
pub fn file_message_json(
    caption: &str,
    attachment_id: &str,
    content_url: &str,
    name: &str,
) -> Value {
    let tag = format!("<attachment id=\"{attachment_id}\"></attachment>");
    let html = if caption.is_empty() {
        tag
    } else {
        format!("{}<br>{tag}", teams_escape(caption))
    };
    json!({
        "body": html_body(&html),
        "attachments": [{
            "id": attachment_id,
            "contentType": "reference",
            "contentUrl": content_url,
            "name": name,
        }],
    })
}

/// A file name OneDrive accepts: no path or reserved characters, no trailing dots or spaces,
/// bounded length, never empty. `#` and `%` go too: they break the path form of the upload URL.
pub fn drive_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\"*:<>?/\\|#%".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(DRIVE_NAME_MAX)
        .collect();
    // OneDrive refuses a name that ends in a dot or a space.
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The GUID inside a driveItem eTag (`"{74D20C7F-...},3"`), or None when it holds none.
pub fn etag_guid(etag: &str) -> Option<String> {
    let inner = match (etag.find('{'), etag.find('}')) {
        (Some(open), Some(close)) if open < close => &etag[open + 1..close],
        _ => etag.trim_matches('"'),
    };
    is_guid(inner).then(|| inner.to_ascii_lowercase())
}

/// A random GUID (Graph accepts any GUID as the attachment id).
pub fn random_guid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatcli_core::limits::teams_escape;

    fn person(id: &str, name: &str) -> PersonRef {
        PersonRef {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn no_mentions_is_exactly_the_escaped_text() {
        for text in ["hi", "a & b <c>\nline two", "q\"uote'", "", "\r\nx"] {
            assert_eq!(text_body(text, &[]).html, teams_escape(text), "{text:?}");
        }
        assert_eq!(text_body("x", &[]).mentions.len(), 0);
    }

    /// The length the TUI checks before sending is the length of the body that is sent.
    #[test]
    fn the_sent_body_is_exactly_as_long_as_core_says() {
        use chatcli_core::limits::teams_body_len;
        for text in [
            "",
            "plain",
            "a & b < c > d \"e\" 'f'",
            "line one\nline two\r\nthree",
            "caf\u{e9} \u{1F600} \u{4e2d}",
            "<<<<>>>>&&&&",
        ] {
            assert_eq!(
                text_body(text, &[]).html.len(),
                teams_body_len(text),
                "{text:?}"
            );
            assert_eq!(
                message_json(&text_body(text, &[]))["body"]["content"]
                    .as_str()
                    .unwrap()
                    .len(),
                teams_body_len(text)
            );
        }
    }

    #[test]
    fn a_mention_becomes_an_at_tag_and_an_entry() {
        let sam = person("00000000-0000-0000-0000-000000000002", "Sam Sample");
        let b = text_body("hi @Sam Sample & co", &[sam]);
        assert_eq!(b.html, "hi <at id=\"0\">Sam Sample</at> &amp; co");
        assert_eq!(
            b.mentions,
            vec![json!({
                "id": 0,
                "mentionText": "Sam Sample",
                "mentioned": { "user": {
                    "id": "00000000-0000-0000-0000-000000000002",
                    "displayName": "Sam Sample",
                    "userIdentityType": "aadUser",
                } },
            })]
        );
    }

    #[test]
    fn each_mention_takes_its_own_occurrence_in_list_order() {
        let sam = person("a", "Sam");
        let b = text_body("@Sam and @Sam again", &[sam.clone(), sam]);
        assert_eq!(
            b.html,
            "<at id=\"0\">Sam</at> and <at id=\"1\">Sam</at> again"
        );
        assert_eq!(b.mentions.len(), 2);
        // A longer name that starts with a shorter one never reuses its text.
        let b = text_body(
            "@Sam Sample @Sam",
            &[person("long", "Sam Sample"), person("short", "Sam")],
        );
        assert_eq!(b.html, "<at id=\"0\">Sam Sample</at> <at id=\"1\">Sam</at>");
    }

    #[test]
    fn a_mention_missing_from_the_text_is_dropped() {
        let b = text_body("plain <b>", &[person("a", "Sam")]);
        assert_eq!(b.html, "plain &lt;b&gt;");
        assert!(b.mentions.is_empty());
        // An unusable entry never produces a tag either.
        assert!(text_body("@x", &[person("", "x")]).mentions.is_empty());
        assert!(text_body("@", &[person("a", "")]).mentions.is_empty());
    }

    #[test]
    fn a_short_name_never_matches_inside_a_longer_one() {
        // `Al` is first in the list, but `@Alex Example` is the longer name and wins its text.
        let people = [person("al", "Al"), person("alex", "Alex Example")];
        let b = text_body("hi @Alex Example", &people);
        assert_eq!(b.html, "hi <at id=\"0\">Alex Example</at>");
        assert_eq!(b.used, vec![person("alex", "Alex Example")]);
        // Both people, each in their own place.
        let b = text_body("@Al and @Alex Example", &people);
        assert_eq!(
            b.html,
            "<at id=\"1\">Al</at> and <at id=\"0\">Alex Example</at>"
        );
        // A name followed by a letter is not a mention of that name.
        assert!(text_body("@Alexa", &[person("al", "Alex")])
            .mentions
            .is_empty());
        assert_eq!(
            text_body("@Alex, hi", &[person("al", "Alex")]).html,
            "<at id=\"0\">Alex</at>, hi"
        );
    }

    #[test]
    fn an_edit_keeps_the_mentions() {
        let sam = person("a", "Sam Sample");
        let built = text_body("@Sam Sample, fixed", std::slice::from_ref(&sam));
        let j = edit_json(&built);
        assert_eq!(j["body"]["content"], "<at id=\"0\">Sam Sample</at>, fixed");
        assert_eq!(j["mentions"][0]["mentioned"]["user"]["id"], "a");
        assert_eq!(built.used, vec![sam]);
    }

    #[test]
    fn mention_names_are_escaped() {
        let b = text_body("@A<B", &[person("a", "A<B")]);
        assert_eq!(b.html, "<at id=\"0\">A&lt;B</at>");
    }

    #[test]
    fn message_json_has_html_body_and_mentions_only_when_present() {
        let plain = message_json(&text_body("hi", &[]));
        assert_eq!(
            plain,
            json!({ "body": { "contentType": "html", "content": "hi" } })
        );
        let with = message_json(&text_body("@Sam", &[person("a", "Sam")]));
        assert_eq!(with["mentions"][0]["id"], 0);
        assert_eq!(with["body"]["content"], "<at id=\"0\">Sam</at>");
    }

    #[test]
    fn reply_follows_the_learn_example() {
        let reply = reply_json(
            "1728088338580",
            message_json(&text_body("Hello World", &[])),
        );
        assert_eq!(
            reply,
            json!({
                "messageIds": ["1728088338580"],
                "replyMessage": { "body": { "contentType": "html", "content": "Hello World" } },
            })
        );
    }

    #[test]
    fn edit_escapes_the_new_text() {
        assert_eq!(
            edit_json(&text_body("a < b\nc", &[])),
            json!({ "body": { "contentType": "html", "content": "a &lt; b<br>c" } })
        );
    }

    #[test]
    fn image_message_uses_hosted_contents_with_a_temporary_id() {
        let m = image_message_json("look", "image/png", &[1, 2, 3]);
        assert_eq!(
            m["body"]["content"],
            "look<br><img src=\"../hostedContents/1/$value\">"
        );
        assert_eq!(m["hostedContents"][0]["@microsoft.graph.temporaryId"], "1");
        assert_eq!(m["hostedContents"][0]["contentBytes"], "AQID");
        assert_eq!(m["hostedContents"][0]["contentType"], "image/png");
        let bare = image_message_json("", "image/jpeg", &[]);
        assert_eq!(
            bare["body"]["content"],
            "<img src=\"../hostedContents/1/$value\">"
        );
    }

    #[test]
    fn file_message_is_a_reference_attachment_with_a_body_tag() {
        let m = file_message_json(
            "",
            "11111111-1111-1111-1111-111111111111",
            "https://contoso.example.invalid/personal/alex/Documents/Microsoft%20Teams%20Chat%20Files/a.docx",
            "a.docx",
        );
        assert_eq!(
            m["body"]["content"],
            "<attachment id=\"11111111-1111-1111-1111-111111111111\"></attachment>"
        );
        assert_eq!(m["attachments"][0]["contentType"], "reference");
        assert_eq!(m["attachments"][0]["name"], "a.docx");
        assert_eq!(
            m["attachments"][0]["id"],
            "11111111-1111-1111-1111-111111111111"
        );
    }

    #[test]
    fn drive_names_are_safe() {
        assert_eq!(drive_name("report.docx"), "report.docx");
        assert_eq!(
            drive_name("a/b\\c:d*e?f\"g<h>i|j#k%l.txt"),
            "a_b_c_d_e_f_g_h_i_j_k_l.txt"
        );
        assert_eq!(drive_name("  name. . "), "name");
        assert_eq!(drive_name("..."), "file");
        assert_eq!(drive_name(""), "file");
        assert_eq!(drive_name(&"x".repeat(400)).chars().count(), DRIVE_NAME_MAX);
    }

    #[test]
    fn etag_guid_comes_back_lower_case() {
        const MIXED: &str = "aBcDeF01-2345-6789-abcd-ef0123456789";
        let etag = format!("\"{{{}}},2\"", MIXED.to_uppercase());
        assert_eq!(etag_guid(&etag), Some(MIXED.to_lowercase()));
    }

    #[test]
    fn etag_guid_is_read_from_the_braces() {
        assert_eq!(
            etag_guid("\"{11111111-1111-1111-1111-111111111111},3\"").as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            etag_guid("\"11111111-1111-1111-1111-111111111111\"").as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(etag_guid("\"abc,1\""), None);
        assert_eq!(etag_guid(""), None);
    }

    #[test]
    fn random_guids_have_the_guid_shape() {
        let (a, b) = (random_guid(), random_guid());
        assert!(is_guid(&a) && a != b);
    }
}
