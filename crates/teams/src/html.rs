//! Teams message bodies are HTML. The terminal shows one line of plain text.

use chatcli_core::open::web_url;

/// What one pass over a message body finds.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Plain text on one line (see `to_text`).
    pub text: String,
    /// `(id, name)` of every `<at id="..">name</at>` tag, in body order. The text holds the
    /// same name as "@" + name.
    pub at: Vec<(String, String)>,
    /// Ids of inline images that live in Graph's hostedContents, in body order, no repeats.
    pub hosted: Vec<String>,
}

/// Plain text on one line: tags dropped, images/attachments as markers, emoji as their
/// alt text, mentions as "@name", links as "text (https://...)", entities decoded,
/// whitespace collapsed.
pub fn to_text(html: &str) -> String {
    parse(html, &[]).text
}

// Limit: a hand scanner, not an HTML parser. A '>' inside an attribute value cuts the
// tag short; good enough for chat bodies, swap for html2text if rich messages render wrong.
/// One pass over the body. `drop_attachment_ids`: `<attachment id=..>` tags with these ids
/// leave no marker (the id of a quoted-message reference, shown as a quote instead).
pub fn parse(html: &str, drop_attachment_ids: &[&str]) -> Parsed {
    let mut out = String::with_capacity(html.len());
    let mut parsed = Parsed::default();
    // Where the open <at> / <a> started in `out`, and what it carries.
    let mut at_open: Option<(String, usize)> = None;
    let mut a_open: Option<(Option<String>, usize)> = None;
    let mut rest = html;
    loop {
        let Some(start) = rest.find('<') else {
            out.push_str(&decode_entities(rest));
            break;
        };
        out.push_str(&decode_entities(&rest[..start]));
        let Some(len) = rest[start..].find('>') else {
            // A lone '<' is text, not a tag.
            out.push_str(&decode_entities(&rest[start..]));
            break;
        };
        let tag = &rest[start + 1..start + len];
        rest = &rest[start + len + 1..];
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("img", false) => {
                if let Some(id) = attr(tag, "src").and_then(|s| hosted_id(&s)) {
                    if !parsed.hosted.contains(&id) {
                        parsed.hosted.push(id);
                    }
                    out.push_str(" [image] ");
                } else if attr(tag, "itemtype").is_some_and(|t| t.contains("Emoji")) {
                    // Teams draws some emoji as pictures; the alt text is the emoji itself.
                    out.push_str(&decode_entities(&attr(tag, "alt").unwrap_or_default()));
                } else {
                    out.push_str(" [image] ");
                }
            }
            ("attachment", false) => {
                let id = attr(tag, "id").unwrap_or_default();
                if !drop_attachment_ids.contains(&id.as_str()) {
                    out.push_str(" [attachment] ");
                }
            }
            ("emoji", false) => {
                out.push_str(&decode_entities(&attr(tag, "alt").unwrap_or_default()));
            }
            ("at", false) => {
                at_open = Some((attr(tag, "id").unwrap_or_default(), out.len() + 1));
                out.push('@');
            }
            ("at", true) => {
                if let Some((id, from)) = at_open.take() {
                    parsed.at.push((id, collapse(&out[from..])));
                }
            }
            ("a", false) => {
                let href = attr(tag, "href").map(|h| decode_entities(&h));
                a_open = Some((href, out.len()));
            }
            ("a", true) => {
                if let Some((href, from)) = a_open.take() {
                    if let Some(url) = href.as_deref().and_then(web_url) {
                        let label = collapse(&out[from..]);
                        // A bare pasted link is its own label: do not print it twice.
                        let same = label == url
                            || label.trim_end_matches('/') == url.trim_end_matches('/');
                        if !same {
                            out.push_str(&format!(" ({url})"));
                        }
                    }
                }
            }
            ("br", _) | ("p", true) | ("div", true) | ("li", false) => out.push(' '),
            _ => {}
        }
    }
    parsed.text = collapse(&out);
    parsed
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The hosted content id in `.../hostedContents/{id}/$value`.
fn hosted_id(src: &str) -> Option<String> {
    let after = &src[src.find("/hostedContents/")? + "/hostedContents/".len()..];
    let id = after.split('/').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    // Only a whole attribute name counts: `alt=` must not match inside `data-alt=`.
    let at = tag
        .match_indices(&needle)
        .find(|(i, _)| *i > 0 && tag[..*i].ends_with(char::is_whitespace))?
        .0
        + needle.len();
    let quote = tag[at..].chars().next()?;
    if quote != '"' && quote != char::from(39u8) {
        return None;
    }
    let value = &tag[at + 1..];
    Some(value[..value.find(quote)?].to_string())
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after
            .find(';')
            .filter(|&semi| semi <= 10)
            .and_then(|semi| entity(&after[..semi]).map(|c| (c, semi)));
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some(char::from(39u8)),
        "nbsp" => Some(' '),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, to_text};

    #[test]
    fn strips_tags_and_collapses_whitespace() {
        assert_eq!(
            to_text("<p>Hello&nbsp;<b>Alex</b></p><p>Line 2</p>"),
            "Hello Alex Line 2"
        );
        assert_eq!(to_text("plain text"), "plain text");
        assert_eq!(to_text(""), "");
    }

    #[test]
    fn markers_for_images_attachments_and_emoji() {
        assert_eq!(
            to_text("<p>see <img src='x.png' alt='pic'></p>"),
            "see [image]"
        );
        assert_eq!(
            to_text("<attachment id=\"ee8d\"></attachment>"),
            "[attachment]"
        );
        let smile = char::from_u32(128512).unwrap().to_string();
        assert_eq!(
            to_text("ok <emoji id=\"1f600\" alt=\"&#128512;\" title=\"Grin\"></emoji>"),
            format!("ok {smile}")
        );
    }

    #[test]
    fn emoji_pictures_use_their_alt_text() {
        let smile = char::from_u32(128512).unwrap().to_string();
        let html = format!(
            "hi <img alt=\"{smile}\" src=\"https://cdn.example.invalid/e.png\" itemscope=\"\" itemtype=\"http://schema.skype.com/Emoji\">"
        );
        assert_eq!(to_text(&html), format!("hi {smile}"));
    }

    #[test]
    fn entities() {
        assert_eq!(
            to_text("a &lt; b &amp;&amp; c &#39;q&#39; &#x41; &unknown; &"),
            "a < b && c 'q' A &unknown; &"
        );
    }

    #[test]
    fn lone_angle_bracket_is_text() {
        assert_eq!(to_text("5 < 6"), "5 < 6");
    }

    #[test]
    fn links_keep_their_target_in_brackets() {
        assert_eq!(
            to_text("<a href=\"https://example.invalid/x?a=1&amp;b=2\">the doc</a> is ready"),
            "the doc (https://example.invalid/x?a=1&b=2) is ready"
        );
        // A pasted link is its own label.
        assert_eq!(
            to_text("<a href=\"https://example.invalid/x\">https://example.invalid/x</a>"),
            "https://example.invalid/x"
        );
        // Only web links: nothing else is printed as a target.
        assert_eq!(
            to_text("<a href=\"javascript:alert(1)\">click</a>"),
            "click"
        );
        assert_eq!(to_text("<a href=\"mailto:a@example.com\">mail</a>"), "mail");
        assert_eq!(to_text("<a>no href</a>"), "no href");
    }

    #[test]
    fn mentions_read_as_at_name_and_are_listed() {
        let p = parse(
            "<p>hi <at id=\"0\">Sam Sample</at> and <at id=\"1\">Jordan  Placeholder</at></p>",
            &[],
        );
        assert_eq!(p.text, "hi @Sam Sample and @Jordan Placeholder");
        assert_eq!(
            p.at,
            vec![
                ("0".to_string(), "Sam Sample".to_string()),
                ("1".to_string(), "Jordan Placeholder".to_string())
            ]
        );
    }

    #[test]
    fn hosted_images_are_listed_once_in_body_order() {
        let p = parse(
            "<img src=\"https://graph.example.invalid/v1.0/chats/19:a/messages/1/hostedContents/aWQ9Mg/$value\" alt=\"x\">\
             <img src=\"https://other.example.invalid/pic.png\">\
             <img src=\"https://graph.example.invalid/v1.0/chats/19:a/messages/1/hostedContents/aWQ9Mg/$value\">\
             <img src=\"https://graph.example.invalid/v1.0/chats/19:a/messages/1/hostedContents/aWQ9Mw/$value\">",
            &[],
        );
        assert_eq!(p.hosted, ["aWQ9Mg", "aWQ9Mw"]);
        assert_eq!(p.text, "[image] [image] [image] [image]");
    }

    #[test]
    fn dropped_attachment_ids_leave_no_marker() {
        let html = "<p></p><attachment id=\"1728\"></attachment>test reply<attachment id=\"9\"></attachment>";
        assert_eq!(parse(html, &["1728"]).text, "test reply [attachment]");
        assert_eq!(
            parse(html, &[]).text,
            "[attachment] test reply [attachment]"
        );
    }

    #[test]
    fn attribute_names_match_whole_words() {
        assert_eq!(
            to_text("<emoji data-alt=\"no\" alt=\"yes\"></emoji>"),
            "yes"
        );
    }
}
