//! The command registry: every `/command` (command bar, message box) and `:command` (vim
//! box) is one entry here. Pure: `App::run_cmd` does the work, `ui` draws the lists.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmdId {
    Help,
    Settings,
    Keys,
    Theme,
    Layout,
    Notifications,
    Search,
    Refresh,
    SignIn,
    SignOut,
    About,
    Tour,
    Quit,
}

pub(crate) struct Cmd {
    pub id: CmdId,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// One line, shown next to the name in the command bar, the popup and the help.
    pub doc: &'static str,
}

const fn cmd(
    id: CmdId,
    name: &'static str,
    aliases: &'static [&'static str],
    doc: &'static str,
) -> Cmd {
    Cmd {
        id,
        name,
        aliases,
        doc,
    }
}

/// In the order the bar lists them.
pub(crate) const COMMANDS: &[Cmd] = &[
    cmd(CmdId::Help, "help", &["h", "?"], "keys and commands"),
    cmd(
        CmdId::Settings,
        "settings",
        &["prefs", "options", "config"],
        "theme, layout, keys and more",
    ),
    cmd(CmdId::Keys, "keys", &["bindings"], "change key bindings"),
    cmd(CmdId::Theme, "theme", &["colours", "colors"], "colours"),
    cmd(
        CmdId::Layout,
        "layout",
        &[],
        "sidebar, density, time format",
    ),
    cmd(
        CmdId::Notifications,
        "notifications",
        &["notify"],
        "desktop notifications",
    ),
    cmd(
        CmdId::Search,
        "search",
        &["find"],
        "search every stored message",
    ),
    cmd(CmdId::Refresh, "refresh", &[], "reload the chat list"),
    cmd(
        CmdId::SignIn,
        "signin",
        &["login"],
        "sign in to this network",
    ),
    cmd(
        CmdId::SignOut,
        "signout",
        &["logout"],
        "sign out, after y/n",
    ),
    cmd(
        CmdId::About,
        "about",
        &["version"],
        "version, data folder, log",
    ),
    cmd(CmdId::Tour, "tour", &["demo"], "watch chatcli show itself"),
    cmd(CmdId::Quit, "quit", &["q", "exit"], "quit chatcli"),
];

/// Heading of the message-box popup that lists commands (`Popup::title`).
pub(crate) const COMMANDS_TITLE: &str = "Commands";

/// The one-line pointer in the empty message pane and the sign-in panel.
pub(crate) const COMMANDS_HINT: &str = "Type / for commands: /help /settings /keys";

fn bare(word: &str) -> &str {
    word.trim_start_matches('/')
}

/// The command a word names: exact name or alias, ASCII-case-insensitive, leading `/` ignored.
pub(crate) fn find(word: &str) -> Option<&'static Cmd> {
    let w = bare(word);
    COMMANDS.iter().find(|c| {
        c.name.eq_ignore_ascii_case(w) || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(w))
    })
}

/// Commands that match what was typed, best first: name prefix, alias prefix, name contains,
/// doc contains. An empty query lists every command in table order.
pub(crate) fn matching(query: &str) -> Vec<&'static Cmd> {
    let q = bare(query).to_ascii_lowercase();
    let mut hits: Vec<(u8, &'static Cmd)> = COMMANDS
        .iter()
        .filter_map(|c| {
            let rank = if q.is_empty() || c.name.starts_with(&q) {
                0
            } else if c.aliases.iter().any(|a| a.starts_with(&q)) {
                1
            } else if c.name.contains(&q) {
                2
            } else if c
                .doc
                .to_ascii_lowercase()
                .split(|ch: char| !ch.is_alphanumeric())
                .any(|word| word.starts_with(&q))
            {
                3
            } else {
                return None;
            };
            Some((rank, c))
        })
        .collect();
    // Stable: table order inside a rank.
    hits.sort_by_key(|h| h.0);
    hits.into_iter().map(|h| h.1).collect()
}

/// What the user typed in the message box.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Typed<'a> {
    /// An ordinary message.
    Text,
    /// `//rest`: send `/rest` as text.
    Escaped(&'a str),
    /// `/word args`: a command (or an unknown word). `args` is trimmed.
    Command(&'a str, &'a str),
}

/// Reads a trimmed draft: `//x` is text, a lone leading `/` is a command word.
pub(crate) fn parse_typed(text: &str) -> Typed<'_> {
    let Some(body) = text.strip_prefix('/') else {
        return Typed::Text;
    };
    if body.starts_with('/') {
        return Typed::Escaped(body);
    }
    match body.split_once(char::is_whitespace) {
        Some((word, args)) => Typed::Command(word, args.trim()),
        None => Typed::Command(body, ""),
    }
}

/// The command word being typed at the start of the draft: the text after the `/`, while the
/// text before the cursor has no space and does not start with `//`. `None` otherwise.
pub(crate) fn slash_query(before: &str) -> Option<&str> {
    let body = before.strip_prefix('/')?;
    (!body.starts_with('/') && !body.contains(char::is_whitespace)).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_are_unique_and_lower_case() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            for w in std::iter::once(&c.name).chain(c.aliases) {
                assert_eq!(*w, w.to_ascii_lowercase(), "{w}");
                assert!(!w.contains(char::is_whitespace), "{w}");
                assert!(seen.insert(*w), "{w} used twice");
            }
            assert!(!c.doc.is_empty());
        }
    }

    #[test]
    fn find_takes_name_alias_case_and_slash() {
        assert_eq!(find("settings").map(|c| c.id), Some(CmdId::Settings));
        assert_eq!(find("/Settings").map(|c| c.id), Some(CmdId::Settings));
        assert_eq!(find("PREFS").map(|c| c.id), Some(CmdId::Settings));
        assert_eq!(find("?").map(|c| c.id), Some(CmdId::Help));
        assert_eq!(find("q").map(|c| c.id), Some(CmdId::Quit));
        assert!(find("sett").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn matching_lists_everything_for_an_empty_query_in_order() {
        let all: Vec<_> = matching("").iter().map(|c| c.id).collect();
        let table: Vec<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(all, table);
        assert_eq!(matching("/").len(), COMMANDS.len());
    }

    #[test]
    fn matching_ranks_name_prefix_alias_name_then_doc() {
        let ids = |q: &str| -> Vec<CmdId> { matching(q).iter().map(|c| c.id).collect() };
        // Name prefixes first (table order), then the rest.
        assert_eq!(&ids("se")[..2], &[CmdId::Settings, CmdId::Search]);
        // Alias prefix: "log" is an alias of signin (login) and signout (logout); about
        // only has it in its doc, so it comes last.
        assert_eq!(ids("log"), [CmdId::SignIn, CmdId::SignOut, CmdId::About]);
        // Name contains (layout, signout, about) comes in table order.
        assert_eq!(
            &ids("out")[..3],
            &[CmdId::Layout, CmdId::SignOut, CmdId::About]
        );
        // Doc only: "bindings" is an alias of keys, "sidebar" is only in layout's doc.
        assert_eq!(ids("sidebar"), [CmdId::Layout]);
        assert!(ids("zzz").is_empty());
    }

    #[test]
    fn typed_text_is_text_a_command_or_an_escape() {
        assert_eq!(parse_typed("hello /x"), Typed::Text);
        assert_eq!(parse_typed("//hi"), Typed::Escaped("/hi"));
        assert_eq!(parse_typed("/help"), Typed::Command("help", ""));
        assert_eq!(parse_typed("/keys   vim "), Typed::Command("keys", "vim"));
        assert_eq!(parse_typed("/"), Typed::Command("", ""));
        assert_eq!(slash_query("/se"), Some("se"));
        assert_eq!(slash_query("/"), Some(""));
        assert_eq!(slash_query("//se"), None);
        assert_eq!(slash_query("/keys v"), None);
        assert_eq!(slash_query("hi /se"), None);
    }

    #[test]
    fn the_hint_names_real_commands() {
        for word in COMMANDS_HINT
            .split_whitespace()
            .filter(|w| w.len() > 1 && w.starts_with('/'))
        {
            assert!(find(word).is_some(), "{word}");
        }
    }
}
