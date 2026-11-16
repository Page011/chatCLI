//! Teams message bodies are HTML. The terminal shows one line of plain text.

/// Plain text on one line: tags dropped, images/attachments as markers, emoji as their
/// alt text, entities decoded, whitespace collapsed.
// Limit: a hand scanner, not an HTML parser. A '>' inside an attribute value cuts the
// tag short; good enough for chat bodies, swap for html2text if rich messages render wrong.
pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
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
        out.push_str(&tag_text(&rest[start + 1..start + len]));
        rest = &rest[start + len + 1..];
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tag_text(tag: &str) -> String {
    let closing = tag.starts_with('/');
    let name = tag
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match (name.as_str(), closing) {
        ("img", false) => " [image] ".into(),
        ("attachment", false) => " [attachment] ".into(),
        ("emoji", false) => attr(tag, "alt")
            .map(|a| decode_entities(&a))
            .unwrap_or_default(),
        ("br", _) | ("p", true) | ("div", true) | ("li", false) => " ".into(),
        _ => String::new(),
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
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
    use super::to_text;

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
}
