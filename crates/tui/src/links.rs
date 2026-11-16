//! Finding web links in message text. Only `http://` and `https://` are ever offered: the scan
//! looks for those two schemes, so `file:`, `javascript:` and `data:` links never appear, and
//! every hit also passes `chatcli_core::open::web_url` (a host, no spaces or control characters).
//! Detection runs on the stored text, not on wrapped lines, so a link that wraps is still one link.

use chatcli_core::open::web_url;

/// A link in a message: its byte range in the text and the cleaned URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Punctuation that ends a sentence rather than a URL.
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', '\'', '"', '\u{2026}', '*', '_',
];

/// Characters that cannot be part of a URL in running text.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\u{201C}' | '\u{201D}')
}

/// Does the text at `at` start with `scheme` (ASCII, any case)?
fn starts_with_ci(text: &str, at: usize, scheme: &str) -> bool {
    text.get(at..at + scheme.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
}

/// Cuts trailing sentence punctuation, and a closing bracket that has no opener inside the URL
/// (`(see https://example.com/page)` keeps the page; `.../wiki/Rust_(language)` keeps its paren).
fn trim_end(url: &str) -> &str {
    let mut end = url.len();
    loop {
        let cur = &url[..end];
        let Some(last) = cur.chars().next_back() else {
            return cur;
        };
        let open = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        };
        let cut = match open {
            Some(o) => cur.matches(last).count() > cur.matches(o).count(),
            None => TRAILING.contains(&last),
        };
        if !cut {
            return cur;
        }
        end -= last.len_utf8();
    }
}

/// Every web link in `text`, in order. A scheme counts only at the start of a word or after
/// punctuation, never glued to letters (`xhttps://...`).
pub fn find_links(text: &str) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let Some((at, _)) = text[from..]
            .char_indices()
            .map(|(i, c)| (from + i, c))
            .find(|&(i, c)| {
                (c == 'h' || c == 'H')
                    && (starts_with_ci(text, i, "http://") || starts_with_ci(text, i, "https://"))
            })
        else {
            break;
        };
        let glued = text[..at]
            .chars()
            .next_back()
            .is_some_and(|p| p.is_alphanumeric());
        let raw_end = text[at..]
            .char_indices()
            .find(|&(_, c)| ends_url(c))
            .map_or(text.len(), |(i, _)| at + i);
        from = raw_end.max(at + 1);
        if glued {
            continue;
        }
        let trimmed = trim_end(&text[at..raw_end]);
        if let Some(url) = web_url(trimmed) {
            out.push(Link {
                start: at,
                end: at + trimmed.len(),
                url,
            });
        }
    }
    out
}

/// The distinct URLs of `text`, in order of first appearance (a link pasted twice is offered
/// once).
pub fn urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in find_links(text) {
        if !out.contains(&l.url) {
            out.push(l.url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls_of(text: &str) -> Vec<String> {
        find_links(text).into_iter().map(|l| l.url).collect()
    }

    #[test]
    fn plain_links_and_ranges() {
        let text = "see https://example.com/a?b=1&c=2#frag now";
        let l = find_links(text);
        assert_eq!(l.len(), 1);
        assert_eq!(
            &text[l[0].start..l[0].end],
            "https://example.com/a?b=1&c=2#frag"
        );
        assert_eq!(l[0].url, "https://example.com/a?b=1&c=2#frag");
        assert_eq!(
            urls_of("HTTP://EXAMPLE.COM/x"),
            vec!["HTTP://EXAMPLE.COM/x"]
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_link() {
        for (text, want) in [
            ("go to https://example.com.", "https://example.com"),
            ("https://example.com/a, then", "https://example.com/a"),
            ("really? https://example.com/a?", "https://example.com/a"),
            ("(see https://example.com/page)", "https://example.com/page"),
            ("[https://example.com/p]", "https://example.com/p"),
            ("\"https://example.com/q\"", "https://example.com/q"),
            ("https://example.com/a!!!", "https://example.com/a"),
            ("*https://example.com/bold*", "https://example.com/bold"),
            ("a (https://example.com/x). Next", "https://example.com/x"),
        ] {
            assert_eq!(urls_of(text), vec![want.to_string()], "{text}");
        }
    }

    #[test]
    fn balanced_parentheses_stay() {
        assert_eq!(
            urls_of("read https://example.com/wiki/Rust_(language)."),
            vec!["https://example.com/wiki/Rust_(language)"]
        );
        assert_eq!(
            urls_of("(https://example.com/wiki/Rust_(language))"),
            vec!["https://example.com/wiki/Rust_(language)"]
        );
    }

    #[test]
    fn query_strings_ports_and_userinfo_survive() {
        assert_eq!(
            urls_of("http://example.com:8080/p?a=b&c=d%20e;f"),
            vec!["http://example.com:8080/p?a=b&c=d%20e;f"]
        );
        assert_eq!(
            urls_of("https://example.com/?next=https://other.example/x"),
            vec!["https://example.com/?next=https://other.example/x"]
        );
    }

    #[test]
    fn unicode_hosts_and_paths_are_found_as_written() {
        let text = "visit https://b\u{fc}cher.example/\u{4e2d}\u{6587} today";
        assert_eq!(
            urls_of(text),
            vec!["https://b\u{fc}cher.example/\u{4e2d}\u{6587}"]
        );
        // A lookalike host stays as written: the confirm line shows it, no decoding.
        assert_eq!(
            urls_of("https://\u{430}pple.example/login"),
            vec!["https://\u{430}pple.example/login"]
        );
    }

    #[test]
    fn other_schemes_are_never_offered() {
        for text in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "data:text/html,<script>1</script>",
            "ftp://example.com/x",
            "mailto:alex@example.com",
            "vbscript:msgbox(1)",
            "http:/missing.example",
            "https://",
            "http:// example.com",
        ] {
            assert!(find_links(text).is_empty(), "{text}");
        }
        // A web link next to a bad one still shows, the bad one never.
        assert_eq!(
            urls_of("javascript:alert(1) https://example.com/ok file:///x"),
            vec!["https://example.com/ok"]
        );
        assert_eq!(
            urls_of("http://example.com/?u=javascript:alert(1)"),
            vec!["http://example.com/?u=javascript:alert(1)"],
            "inside a web URL it is only a query string"
        );
    }

    #[test]
    fn a_scheme_glued_to_letters_is_not_a_link() {
        assert!(find_links("xhttps://example.com").is_empty());
        assert!(find_links("nothttp://example.com").is_empty());
        assert_eq!(urls_of(">https://example.com"), vec!["https://example.com"]);
    }

    #[test]
    fn several_links_and_duplicates() {
        let text = "a https://one.example b http://two.example/x and https://one.example again";
        assert_eq!(find_links(text).len(), 3);
        assert_eq!(
            urls(text),
            vec!["https://one.example", "http://two.example/x"]
        );
        assert!(find_links("no links, just http and https words").is_empty());
        assert!(find_links("").is_empty());
    }

    #[test]
    fn teams_text_with_a_different_target_exposes_the_target() {
        // The Teams HTML step writes `text (https://...)`: the target is the link, not the text.
        assert_eq!(
            urls_of("Quarterly report (https://example.com/files/q3.pdf)"),
            vec!["https://example.com/files/q3.pdf"]
        );
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for text in [
            "https://\u{0}",
            "https://\u{1b}[31m",
            "https://a b",
            "http",
            "h",
            "\u{1F600}https://example.com\u{1F600}",
            &"https://".repeat(1000),
        ] {
            let _ = find_links(text);
        }
    }
}
