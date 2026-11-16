//! Colour themes: named roles instead of colours scattered through the drawing code.
//!
//! The built-in themes use the 16 ANSI colours only, so the terminal's own palette decides how
//! they look. `default` is exactly the screen as it was before themes existed. A `[theme]`
//! override changes one role's colour and keeps that role's attributes (bold, reverse). Index
//! (16-255) and `#rrggbb` values are fixed colours that do not follow the terminal palette: the
//! one exception to the ANSI-16 rule, and only when the user asks for it.
//!
//! `NO_COLOR` removes every colour; bold, dim and reverse stay, and the roles that only carried
//! meaning through colour get an attribute instead, so status is never colour alone.

use chatcli_core::settings::{SettingsProblem, Severity, ThemeSettings};
use ratatui::style::{Color, Modifier, Style};

/// Every role a `[theme]` table may override, in help order.
pub const ROLES: [&str; 14] = [
    "text",
    "dim",
    "hint",
    "accent",
    "own_name",
    "unread",
    "error",
    "warn",
    "ok",
    "selected",
    "chip_live",
    "chip_attention",
    "network_teams",
    "network_whatsapp",
];

/// Built-in theme names.
pub const THEMES: [&str; 4] = ["default", "high-contrast", "light", "dark"];

/// One style per role; the doc on each field says what it colours.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Message bodies, chat titles, the draft.
    pub text: Style,
    /// Secondary text: times, previews, separators, tags.
    pub dim: Style,
    /// Key hints and empty-state notes (these must stay readable, so default is the DIM attribute
    /// and not a grey that some palettes draw as the background).
    pub hint: Style,
    /// Chips that say work is under way (SIGNING IN, SCAN QR).
    pub accent: Style,
    /// Your own name in the conversation.
    pub own_name: Style,
    /// Unread counts and the titles of chats with unread messages.
    pub unread: Style,
    /// Errors and anything urgent: red in every built-in theme.
    pub error: Style,
    /// Warning text, such as why a network is NOT LIVE.
    pub warn: Style,
    /// Positive progress text, such as an upload percentage.
    pub ok: Style,
    /// The selected chat, tab or message: tint or reverse plus bold, never a stripe on one edge.
    pub selected: Style,
    /// The LIVE chip.
    pub chip_live: Style,
    /// Chips that need the user (SIGN-IN NEEDED, NOT LIVE).
    pub chip_attention: Style,
    /// The Teams tab name.
    pub network_teams: Style,
    /// The WhatsApp tab name.
    pub network_whatsapp: Style,
}

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn reversed() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

impl Default for Theme {
    /// Exactly the colours the screen had before themes existed.
    fn default() -> Theme {
        Theme {
            text: Style::new(),
            dim: fg(Color::DarkGray),
            hint: Style::new().add_modifier(Modifier::DIM),
            accent: fg(Color::Cyan),
            own_name: fg(Color::Cyan).patch(bold()),
            unread: bold(),
            error: fg(Color::Red),
            // Plain and bold today: the other themes colour them.
            warn: Style::new(),
            ok: bold(),
            selected: reversed(),
            chip_live: fg(Color::Green),
            chip_attention: fg(Color::Yellow),
            network_teams: Style::new(),
            network_whatsapp: Style::new(),
        }
    }
}

impl Theme {
    /// A built-in theme by name (case does not matter), `None` for an unknown name.
    pub fn builtin(name: &str) -> Option<Theme> {
        let base = Theme::default();
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "default" => base,
            // No grey and no dim text anywhere: everything readable on any palette. Colours only
            // where they add to a text signal, and always with bold.
            "high-contrast" => Theme {
                dim: Style::new(),
                hint: Style::new(),
                accent: bold(),
                own_name: bold(),
                error: fg(Color::Red).patch(bold()),
                warn: bold(),
                ok: bold(),
                chip_live: bold(),
                chip_attention: reversed(),
                ..base
            },
            // For a light terminal: no yellow or cyan, which wash out on white.
            "light" => Theme {
                accent: fg(Color::Blue),
                own_name: fg(Color::Blue).patch(bold()),
                warn: fg(Color::Magenta),
                ok: fg(Color::Green).patch(bold()),
                chip_attention: fg(Color::Magenta),
                ..base
            },
            // For a dark terminal: a lighter grey than DarkGray, which some palettes draw as black.
            "dark" => Theme {
                dim: fg(Color::Gray),
                warn: fg(Color::Yellow),
                ok: fg(Color::Green).patch(bold()),
                ..base
            },
            _ => return None,
        })
    }

    fn role_mut(&mut self, role: &str) -> Option<&mut Style> {
        Some(match role {
            "text" => &mut self.text,
            "dim" => &mut self.dim,
            "hint" => &mut self.hint,
            "accent" => &mut self.accent,
            "own_name" => &mut self.own_name,
            "unread" => &mut self.unread,
            "error" => &mut self.error,
            "warn" => &mut self.warn,
            "ok" => &mut self.ok,
            "selected" => &mut self.selected,
            "chip_live" => &mut self.chip_live,
            "chip_attention" => &mut self.chip_attention,
            "network_teams" => &mut self.network_teams,
            "network_whatsapp" => &mut self.network_whatsapp,
            _ => return None,
        })
    }

    /// The style of one role by name (a read-only twin of `role_mut`).
    pub fn role(&self, role: &str) -> Option<Style> {
        self.clone().role_mut(role).map(|s| *s)
    }

    fn roles_mut(&mut self) -> [&mut Style; 14] {
        [
            &mut self.text,
            &mut self.dim,
            &mut self.hint,
            &mut self.accent,
            &mut self.own_name,
            &mut self.unread,
            &mut self.error,
            &mut self.warn,
            &mut self.ok,
            &mut self.selected,
            &mut self.chip_live,
            &mut self.chip_attention,
            &mut self.network_teams,
            &mut self.network_whatsapp,
        ]
    }

    /// `NO_COLOR`: every foreground and background goes. Roles that were only a colour keep a
    /// signal as an attribute: secondary text dims, errors and warnings go bold.
    pub fn without_colour(mut self) -> Theme {
        for s in self.roles_mut() {
            s.fg = None;
            s.bg = None;
        }
        self.dim = self.dim.add_modifier(Modifier::DIM);
        self.error = self.error.add_modifier(Modifier::BOLD);
        self.warn = self.warn.add_modifier(Modifier::BOLD);
        self.chip_attention = self.chip_attention.add_modifier(Modifier::BOLD);
        self
    }

    /// The theme `[theme]` asks for, and every problem with it. A bad name falls back to
    /// `default`; a bad role or colour is skipped, so the theme's own value stays for it.
    pub fn from_settings(s: &ThemeSettings, no_colour: bool) -> (Theme, Vec<SettingsProblem>) {
        let mut problems = Vec::new();
        let problem = |key: Option<&str>, message: String| SettingsProblem {
            severity: Severity::Error,
            line: key.and_then(|k| s.lines.get(k).copied()),
            section: "theme".to_string(),
            key: key.map(str::to_string),
            message,
        };
        let mut theme = Theme::builtin(&s.name).unwrap_or_else(|| {
            problems.push(problem(
                Some("name"),
                format!(
                    "unknown theme \"{}\": use {}. Using \"default\".",
                    s.name,
                    THEMES.join(", ")
                ),
            ));
            Theme::default()
        });
        for (role, value) in &s.overrides {
            let Some(slot) = theme.role_mut(role) else {
                let hint = crate::keymap::closest(role, ROLES.iter().copied())
                    .map_or(format!("use one of {}", ROLES.join(", ")), |c| {
                        format!("did you mean \"{c}\"?")
                    });
                problems.push(problem(
                    Some(role),
                    format!("unknown colour role: {hint} Skipping it."),
                ));
                continue;
            };
            match parse_colour(value) {
                Ok(c) => *slot = slot.fg(c),
                Err(e) => problems.push(problem(
                    Some(role),
                    format!("{e} Using the theme's own value for \"{role}\"."),
                )),
            }
        }
        if no_colour {
            theme = theme.without_colour();
        }
        (theme, problems)
    }
}

/// Problems with the `[theme]` section: what `chatcli settings check` prints for it.
pub fn validate_theme(s: &ThemeSettings) -> Vec<SettingsProblem> {
    Theme::from_settings(s, false).1
}

/// An ANSI name (`red`, `dark_gray`, `light_blue`), an index `0-255`, or `#rrggbb`.
pub fn parse_colour(value: &str) -> Result<Color, String> {
    let v = value.trim().to_ascii_lowercase();
    let bad = || {
        format!(
            "\"{value}\" is not a colour: use a name like red or dark_gray, a number 0-255, or #rrggbb."
        )
    };
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() != 6 || !hex.is_ascii() {
            return Err(bad());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
        return Ok(Color::Rgb(byte(0)?, byte(2)?, byte(4)?));
    }
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse::<u8>().map(Color::Indexed).map_err(|_| bad());
    }
    Ok(match v.replace(['-', ' '], "_").as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "dark_gray" | "darkgray" | "dark_grey" | "darkgrey" => Color::DarkGray,
        "light_red" => Color::LightRed,
        "light_green" => Color::LightGreen,
        "light_yellow" => Color::LightYellow,
        "light_blue" => Color::LightBlue,
        "light_magenta" => Color::LightMagenta,
        "light_cyan" => Color::LightCyan,
        _ => return Err(bad()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str, overrides: &[(&str, &str)]) -> ThemeSettings {
        let mut s = ThemeSettings {
            name: name.to_string(),
            ..ThemeSettings::default()
        };
        for (i, (k, v)) in overrides.iter().enumerate() {
            s.overrides.insert((*k).to_string(), (*v).to_string());
            s.lines.insert((*k).to_string(), i + 10);
        }
        s
    }

    #[test]
    fn colour_values_parse() {
        let cases = [
            ("red", Color::Red),
            ("Dark_Gray", Color::DarkGray),
            ("dark-gray", Color::DarkGray),
            ("gray", Color::Gray),
            ("white", Color::White),
            ("light_blue", Color::LightBlue),
            ("8", Color::Indexed(8)),
            ("255", Color::Indexed(255)),
            ("#00ff7f", Color::Rgb(0, 255, 127)),
            ("#ABCDEF", Color::Rgb(0xab, 0xcd, 0xef)),
        ];
        for (text, want) in cases {
            assert_eq!(parse_colour(text), Ok(want), "{text}");
        }
        for text in [
            "",
            "256",
            "-1",
            "#fff",
            "#gggggg",
            "#12345",
            "#12345\u{e9}",
            "reddish",
            "1.5",
        ] {
            assert!(parse_colour(text).is_err(), "{text}");
        }
    }

    #[test]
    fn default_theme_has_no_problems_and_every_role_is_reachable() {
        let (t, p) = Theme::from_settings(&ThemeSettings::default(), false);
        assert_eq!(t, Theme::default());
        assert!(p.is_empty());
        let mut t = Theme::default();
        for role in ROLES {
            assert!(t.role_mut(role).is_some(), "{role}");
        }
        assert_eq!(t.roles_mut().len(), ROLES.len());
    }

    #[test]
    fn built_ins_exist_and_keep_errors_red() {
        for name in THEMES {
            let t = Theme::builtin(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(t.error.fg, Some(Color::Red), "{name}: errors stay red");
            assert!(
                t.selected.add_modifier.contains(Modifier::REVERSED),
                "{name}"
            );
            // ANSI 16 only: no index or RGB colour in a built-in.
            for s in Theme::builtin(name).unwrap().roles_mut() {
                assert!(
                    !matches!(s.fg, Some(Color::Rgb(..) | Color::Indexed(_))),
                    "{name}"
                );
            }
        }
        assert!(Theme::builtin("neon").is_none());
        assert_eq!(
            Theme::builtin(" High-Contrast "),
            Theme::builtin("high-contrast")
        );
    }

    #[test]
    fn an_override_changes_only_that_role_and_keeps_its_attributes() {
        let (t, p) = Theme::from_settings(&settings("default", &[("own_name", "green")]), false);
        assert!(p.is_empty());
        let want = Theme {
            own_name: fg(Color::Green).patch(bold()),
            ..Theme::default()
        };
        assert_eq!(t, want, "colour replaced, bold kept, nothing else touched");
        // On a reversed role the override is the tint.
        let (t, _) = Theme::from_settings(&settings("default", &[("selected", "#112233")]), false);
        assert_eq!(t.selected.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert!(t.selected.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_bad_value_names_the_role_and_keeps_the_themes_own_value() {
        let s = settings("high-contrast", &[("dim", "banana"), ("error", "#zzzzzz")]);
        let (t, p) = Theme::from_settings(&s, false);
        let own = Theme::builtin("high-contrast").unwrap();
        assert_eq!((t.dim, t.error), (own.dim, own.error));
        assert_eq!(p.len(), 2);
        let texts: Vec<String> = p.iter().map(SettingsProblem::text).collect();
        assert!(texts
            .iter()
            .any(|t| t.contains("[theme] dim:") && t.contains("\"dim\"")));
        assert!(texts.iter().all(|t| t.contains("settings.toml line 1")));
        assert!(p.iter().all(|x| x.severity == Severity::Error));
    }

    #[test]
    fn unknown_theme_and_role_are_reported_with_a_suggestion() {
        let (t, p) = Theme::from_settings(&settings("neon", &[("acent", "red")]), false);
        assert_eq!(t, Theme::default(), "unknown name falls back to default");
        assert_eq!(p.len(), 2);
        assert!(p[0].message.contains("unknown theme \"neon\""));
        assert!(
            p[1].message.contains("did you mean \"accent\"?"),
            "{}",
            p[1].message
        );
    }

    #[test]
    fn no_colour_removes_every_colour_and_keeps_a_signal() {
        for name in THEMES {
            let (t, _) = Theme::from_settings(&settings(name, &[("accent", "#ff0000")]), true);
            let mut t = t;
            for s in t.roles_mut() {
                assert!(s.fg.is_none() && s.bg.is_none(), "{name}");
            }
            assert!(t.dim.add_modifier.contains(Modifier::DIM), "{name}");
            assert!(t.error.add_modifier.contains(Modifier::BOLD), "{name}");
            assert!(
                t.selected.add_modifier.contains(Modifier::REVERSED),
                "{name}"
            );
            assert!(t.unread.add_modifier.contains(Modifier::BOLD), "{name}");
        }
    }
}
