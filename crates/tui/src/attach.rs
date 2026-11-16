//! Choosing a file to send (issue 20): reading a typed or dropped path, and completing it.
//!
//! The only file work here is reading folder names and one `is_file` check. The file's own
//! bytes are never read by the TUI: the backend reads it when the message is sent.

use std::path::{Path, PathBuf};

/// A path longer than this is text, not a dropped file.
const DROP_MAX_CHARS: usize = 1024;
/// Folder entries looked at for completion; a folder with more is cut (shown first-come).
const SCAN_MAX: usize = 5_000;
/// Completions kept.
pub const SHOWN_MAX: usize = 200;

/// One folder entry offered by the path prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub dir: bool,
}

/// A path as typed or dropped: surrounding spaces and one pair of matching quotes removed.
/// Windows Terminal quotes a dropped path when it holds a space.
pub fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].trim();
        }
    }
    s
}

/// Text pasted into the message box that is the path of an existing file (a file dragged onto
/// the terminal): the file's path. Anything else, a folder included, is ordinary text. One
/// line only: several dropped files are text.
pub fn dropped_path(pasted: &str) -> Option<PathBuf> {
    let p = unquote(pasted);
    // A network path is text: asking Windows about it opens an SMB session and sends the user's
    // NTLM hash to whatever host the pasted text names.
    if p.is_empty()
        || p.contains(['\n', '\r'])
        || p.chars().count() > DROP_MAX_CHARS
        || is_network_path(p)
    {
        return None;
    }
    let path = PathBuf::from(p);
    path.is_file().then_some(path)
}

/// A UNC path (two backslashes or two slashes, then a host; the `\\?\UNC\` form starts the same
/// way): the file system would contact another machine to answer anything about it. Never
/// touched without a deliberate key press.
pub fn is_network_path(s: &str) -> bool {
    let s = unquote(s);
    s.starts_with("\\\\") || s.starts_with("//")
}

/// The folder part and the name start of what is typed: `C:\Users\Al` gives (`C:\Users\`, `Al`).
pub fn split_input(input: &str) -> (&str, &str) {
    match input.rfind(['/', '\\']) {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    }
}

/// The path the input names, relative ones from `base`.
pub fn resolve(input: &str, base: &Path) -> PathBuf {
    let p = Path::new(unquote(input));
    if p.is_absolute() || p.has_root() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// The separator to put after a folder: the one the input already uses, else the platform's.
pub fn separator(input: &str) -> char {
    if input.contains('/') && !input.contains('\\') {
        '/'
    } else {
        std::path::MAIN_SEPARATOR
    }
}

/// `input` with its last part replaced by the candidate (a folder gets a separator, so the next
/// Tab or Enter goes inside it).
pub fn apply(input: &str, c: &Candidate) -> String {
    let (dir, _) = split_input(input);
    let mut out = format!("{dir}{}", c.name);
    if c.dir {
        out.push(separator(input));
    }
    out
}

/// Entries of the folder `input` points into whose names start with what is typed after the
/// last separator (case does not matter): folders first, then files, each sorted by name.
/// Dot files stay hidden until a dot is typed. A folder that cannot be read gives nothing.
pub fn complete(input: &str, base: &Path) -> Vec<Candidate> {
    if is_network_path(input) {
        return Vec::new();
    }
    scan(input, base)
}

/// `complete` without the network guard: the caller has the user's Tab press for it.
pub fn scan(input: &str, base: &Path) -> Vec<Candidate> {
    let (dir_part, prefix) = split_input(input);
    let dir = if dir_part.is_empty() {
        base.to_path_buf()
    } else {
        resolve(dir_part, base)
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    let mut out: Vec<Candidate> = read
        .flatten()
        .take(SCAN_MAX)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&prefix)
                || (name.starts_with('.') && prefix.is_empty())
            {
                return None;
            }
            // A symlink to a folder counts as a folder: `metadata` follows it.
            let dir = e.metadata().map(|m| m.is_dir()).unwrap_or(false);
            Some(Candidate { name, dir })
        })
        .collect();
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out.truncate(SHOWN_MAX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chatcli-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("My Pictures")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("My Report.pdf"), b"x").unwrap();
        std::fs::write(dir.join("my notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("photo.png"), b"x").unwrap();
        std::fs::write(dir.join(".secret"), b"x").unwrap();
        dir
    }

    fn names(c: &[Candidate]) -> Vec<String> {
        c.iter()
            .map(|c| format!("{}{}", c.name, if c.dir { "/" } else { "" }))
            .collect()
    }

    #[test]
    fn unquote_strips_one_pair_of_quotes_and_spaces() {
        for (given, want) in [
            ("C:\\a\\b.png", "C:\\a\\b.png"),
            ("\"C:\\My Pictures\\b.png\"", "C:\\My Pictures\\b.png"),
            ("  'C:\\a b.png'  ", "C:\\a b.png"),
            ("\"unterminated", "\"unterminated"),
            ("\"mixed'", "\"mixed'"),
            ("\"\"", ""),
            ("\"", "\""),
            ("", ""),
            ("  plain  ", "plain"),
            ("\" spaced \"", "spaced"),
        ] {
            assert_eq!(unquote(given), want, "{given:?}");
        }
    }

    #[test]
    fn only_an_existing_single_file_counts_as_dropped() {
        let dir = tree("drop");
        let file = dir.join("My Report.pdf");
        let quoted = format!("\"{}\"", file.display());
        assert_eq!(dropped_path(&quoted), Some(file.clone()));
        assert_eq!(
            dropped_path(&file.display().to_string()),
            Some(file.clone())
        );
        assert_eq!(dropped_path(&format!("  {}  ", file.display())), Some(file));
        // Text, a missing file, a folder, two files and an empty paste all stay text.
        assert_eq!(dropped_path("see you at ten"), None);
        assert_eq!(
            dropped_path(&dir.join("gone.png").display().to_string()),
            None
        );
        assert_eq!(dropped_path(&dir.display().to_string()), None);
        let two = format!(
            "\"{}\"\n\"{}\"",
            dir.join("photo.png").display(),
            dir.join("my notes.txt").display()
        );
        assert_eq!(dropped_path(&two), None);
        assert_eq!(dropped_path(""), None);
        assert_eq!(dropped_path(&"a".repeat(2000)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pasted UNC path would make Windows open an SMB session (NTLM hash leak): it is text.
    #[test]
    fn network_paths_are_never_probed_until_tab() {
        for p in [
            "\\\\host.example.invalid\\share\\a.png",
            "//host.example.invalid/share/a.png",
            "\\\\?\\UNC\\host.example.invalid\\share\\a.png",
            "\"\\\\host.example.invalid\\share\\a.png\"",
        ] {
            assert!(is_network_path(p), "{p}");
            assert_eq!(dropped_path(p), None, "{p}");
            assert!(complete(p, Path::new(".")).is_empty(), "{p}");
        }
        assert!(!is_network_path("C:\\a\\b.png"));
        assert!(!is_network_path("/home/alex/a.png"));
        assert!(!is_network_path("\\a"));
        // `scan` is the Tab path: it reads whatever folder it is given.
        let dir = tree("unc");
        assert!(!scan(
            &format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR),
            Path::new(".")
        )
        .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_come_first_and_dot_files_stay_hidden() {
        let dir = tree("order");
        assert_eq!(
            names(&complete("", &dir)),
            [
                "docs/",
                "My Pictures/",
                "my notes.txt",
                "My Report.pdf",
                "photo.png"
            ]
        );
        // A dot typed shows the dot files.
        assert_eq!(names(&complete(".", &dir)), [".hidden/", ".secret"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_with_spaces_complete_whatever_the_case() {
        let dir = tree("spaces");
        assert_eq!(names(&complete("my p", &dir)), ["My Pictures/"]);
        assert_eq!(
            names(&complete("MY", &dir)),
            ["My Pictures/", "my notes.txt", "My Report.pdf"]
        );
        let c = complete("my p", &dir);
        assert_eq!(
            apply("my p", &c[0]),
            format!("My Pictures{}", std::path::MAIN_SEPARATOR)
        );
        // Inside a folder: the folder part stays, the last part is replaced.
        let typed = format!("My Pictures{}", std::path::MAIN_SEPARATOR);
        std::fs::write(dir.join("My Pictures").join("holiday photo.jpg"), b"x").unwrap();
        let inside = complete(&typed, &dir);
        assert_eq!(names(&inside), ["holiday photo.jpg"]);
        assert_eq!(
            apply(&typed, &inside[0]),
            format!("{typed}holiday photo.jpg")
        );
        assert_eq!(
            apply(
                "docs/",
                &Candidate {
                    name: "a".into(),
                    dir: true
                }
            ),
            "docs/a/"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_paths_ignore_the_base_and_a_missing_folder_gives_nothing() {
        let dir = tree("abs");
        let other = std::env::temp_dir();
        let typed = format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
        assert_eq!(names(&complete(&typed, &other)).len(), 5);
        assert_eq!(resolve("photo.png", &dir), dir.join("photo.png"));
        assert_eq!(
            resolve(&dir.join("photo.png").display().to_string(), &other),
            dir.join("photo.png")
        );
        assert_eq!(resolve("\"My Pictures\"", &dir), dir.join("My Pictures"));
        assert!(complete("nope/x", &dir).is_empty());
        assert_eq!(split_input("a\\b/c"), ("a\\b/", "c"));
        assert_eq!(split_input("C:\\Users\\Al"), ("C:\\Users\\", "Al"));
        assert_eq!(split_input("plain"), ("", "plain"));
        assert_eq!(separator("a/b"), '/');
        assert_eq!(separator("a\\b"), std::path::MAIN_SEPARATOR);
        assert_eq!(separator("a/b\\c"), std::path::MAIN_SEPARATOR);
        assert_eq!(separator("ab"), std::path::MAIN_SEPARATOR);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
