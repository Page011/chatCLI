//! The command registry: every command of the `:` command bar and of `:command` at the start of
//! the message box is one entry here. Pure: `App::run_cmd` does the work, `ui` draws the lists.

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
    ReadAll,
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
        CmdId::ReadAll,
        "readall",
        &["markallread"],
        "mark every chat read, after y/n",
    ),
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
pub(crate) const COMMANDS_HINT: &str = "Press : for commands: help, settings, keys";

fn bare(word: &str) -> &str {
    word.trim_start_matches(':')
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

/// The command word being typed at the start of the draft: the text after the `:`, while the
/// text before the cursor has no space and no second colon. `None` otherwise.
pub(crate) fn colon_query(before: &str) -> Option<&str> {
    let body = before.strip_prefix(':')?;
    (!body.contains(|c: char| c.is_whitespace() || c == ':')).then_some(body)
}

/// The commands `:word` at the start of the message box offers, best first. Stricter than the
/// command bar, so an emoticon stays text: the word must be lower case, and it must be a whole
/// name or alias (`:q`) or at least two letters of the start of one (`:he`). `:P` and `:D` are
/// never commands. A lone `:` lists them all.
pub(crate) fn typed_matches(word: &str) -> Vec<&'static Cmd> {
    if word.chars().any(|c| c.is_uppercase()) {
        return Vec::new();
    }
    let whole = |w: &str| w == word;
    let start = |w: &str| word.chars().count() >= 2 && w.starts_with(word);
    let mut hits: Vec<(u8, &'static Cmd)> = COMMANDS
        .iter()
        .filter_map(|c| {
            let names = || std::iter::once(c.name).chain(c.aliases.iter().copied());
            if word.is_empty() || whole(c.name) || start(c.name) {
                Some((0, c))
            } else if names().any(|w| whole(w) || start(w)) {
                Some((1, c))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by_key(|h| h.0);
    hits.into_iter().map(|h| h.1).collect()
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
    fn matching_lists_everything_for_an_empty_query_in_order() {
        let all: Vec<_> = matching("").iter().map(|c| c.id).collect();
        let table: Vec<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(all, table);
        assert_eq!(matching(":").len(), COMMANDS.len());
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
    fn colon_query_is_the_word_after_a_leading_colon() {
        assert_eq!(colon_query(":se"), Some("se"));
        assert_eq!(colon_query(":"), Some(""));
        assert_eq!(colon_query("::se"), None);
        assert_eq!(colon_query(":thumbsup:"), None);
        assert_eq!(colon_query(":keys v"), None);
        assert_eq!(colon_query("hi :se"), None);
        assert_eq!(colon_query("se"), None);
    }

    #[test]
    fn typed_matches_leave_emoticons_alone() {
        let ids = |w: &str| -> Vec<CmdId> { typed_matches(w).iter().map(|c| c.id).collect() };
        assert_eq!(ids("").len(), COMMANDS.len());
        assert_eq!(ids("he"), [CmdId::Help]);
        assert_eq!(ids("se"), [CmdId::Settings, CmdId::Search]);
        // Whole aliases, one letter or more.
        assert_eq!(ids("q"), [CmdId::Quit]);
        assert_eq!(ids("h"), [CmdId::Help]);
        assert_eq!(ids("lo"), [CmdId::SignIn, CmdId::SignOut]);
        // Emoticons and their lower-case twins: no command.
        for face in ["P", "D", "p", "d", "o", "x", "3", "(", ")", "/", "Q", "He"] {
            assert!(ids(face).is_empty(), "{face}");
        }
        // Only the start of a name or alias, never the middle or the doc.
        assert!(ids("out").is_empty());
        assert!(ids("sidebar").is_empty());
    }

    #[test]
    fn the_hint_names_real_commands() {
        let (_, names) = COMMANDS_HINT.split_once("commands: ").expect("the list");
        for word in names.split(", ") {
            assert_eq!(typed_matches(word).first().map(|c| c.name), Some(word));
        }
    }
}
