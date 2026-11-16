//! Opening links and files with the OS default app. Shared by open-media, open-link, the Teams
//! file fallback and the sign-in browser. `opener` passes the argument straight to the OS (no
//! shell), so a crafted file name or URL cannot become a command line.

use std::path::Path;

use crate::media::is_blocked;
use crate::Result;

/// The trimmed URL when it is http or https with a host and has no whitespace, control or
/// backslash characters. `file:`, `javascript:`, `data:` and everything else give None.
pub fn web_url(s: &str) -> Option<String> {
    let url = s.trim();
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    // `rest` is the same length as the original after the scheme, so slice the original.
    let host = host_of(&url[url.len() - rest.len()..])?;
    (!host.is_empty()).then(|| url.to_string())
}

/// The host of a web URL for the "Open <host>? y/n" line: lowercased, as written (no IDN
/// decoding), without port or credentials. `https://good.example.invalid@example.com/` gives
/// `example.com`, the host the browser really goes to.
pub fn url_host(url: &str) -> Option<String> {
    let url = web_url(url)?;
    let rest = url.split_once("://")?.1;
    host_of(rest).map(|h| h.to_ascii_lowercase())
}

/// Authority part up to the first `/`, `?` or `#`, minus credentials and port.
fn host_of(after_scheme: &str) -> Option<String> {
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host_port.starts_with('[') {
        // IPv6 literal: keep the brackets, drop the port after them.
        &host_port[..host_port.find(']').map_or(host_port.len(), |i| i + 1)]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Checks the URL with `web_url`, then opens the default browser. Works with no browser
/// running (the OS starts one).
pub fn open_url(url: &str) -> Result<()> {
    let url = web_url(url).ok_or("Only http and https links can be opened.")?;
    opener::open_browser(&url).map_err(|e| {
        format!("Could not open the browser ({e}): copy the link and open it yourself.")
    })
}

/// Refuses executable types ("BLOCKED: executable") and files that are not there
/// ("FILE NOT FOUND"), then opens the file with the OS default app, the path as one argument.
pub fn open_file(path: &Path) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    if is_blocked(name.as_deref(), None) {
        return Err("BLOCKED: executable".to_string());
    }
    if !path.is_file() {
        return Err("FILE NOT FOUND".to_string());
    }
    opener::open(path).map_err(|e| {
        format!("Could not open the file ({e}): open the cache folder and find it there.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_clean_http_and_https_urls_pass() {
        for ok in [
            "https://example.invalid",
            "http://example.invalid/path?q=1#frag",
            "  https://example.invalid/a  ",
            "HTTPS://EXAMPLE.INVALID/",
            "https://user:pw@example.com:8443/x",
            "https://[::1]:8080/",
        ] {
            assert!(web_url(ok).is_some(), "{ok:?} should pass");
        }
        for bad in [
            "",
            "example.invalid",
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "data:text/html,<b>x</b>",
            "ftp://example.invalid",
            "https://",
            "https:///path",
            "https://example.invalid/a b",
            "https://example.invalid/\u{7}",
            "https://example.invalid\\@evil.invalid",
            "https://@/path",
            "mailto:a@example.com",
            "//example.invalid",
            "https:example.invalid",
        ] {
            assert!(web_url(bad).is_none(), "{bad:?} must be refused");
        }
        assert_eq!(
            web_url("  https://example.invalid/a  ").as_deref(),
            Some("https://example.invalid/a"),
            "the trimmed URL is returned"
        );
    }

    #[test]
    fn host_is_the_one_the_browser_goes_to() {
        let h = |u: &str| url_host(u);
        assert_eq!(
            h("https://Example.INVALID/path").as_deref(),
            Some("example.invalid")
        );
        assert_eq!(
            h("https://example.invalid:8443/").as_deref(),
            Some("example.invalid")
        );
        assert_eq!(
            h("https://good.example.invalid@example.com/x").as_deref(),
            Some("example.com"),
            "credentials never hide the real host"
        );
        assert_eq!(h("https://[::1]:80/").as_deref(), Some("[::1]"));
        assert_eq!(
            h("https://xn--bcher-kva.example/").as_deref(),
            Some("xn--bcher-kva.example")
        );
        assert_eq!(
            h("https://example.invalid?x=1").as_deref(),
            Some("example.invalid")
        );
        assert_eq!(h("javascript:alert(1)"), None);
    }

    #[test]
    fn open_file_refuses_blocked_and_missing_without_launching_anything() {
        assert_eq!(
            open_file(Path::new("Z:/nowhere/setup.exe")).unwrap_err(),
            "BLOCKED: executable"
        );
        assert_eq!(
            open_file(Path::new("Z:/nowhere/run.PS1")).unwrap_err(),
            "BLOCKED: executable"
        );
        assert_eq!(
            open_file(Path::new("Z:/nowhere/report.pdf")).unwrap_err(),
            "FILE NOT FOUND"
        );
        // A folder is not a file either.
        assert_eq!(
            open_file(&std::env::temp_dir()).unwrap_err(),
            "FILE NOT FOUND"
        );
        assert!(open_url("file:///C:/x.exe").is_err());
    }
}
