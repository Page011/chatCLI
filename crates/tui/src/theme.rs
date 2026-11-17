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

/// Palettes: how panels are drawn (outlines and background shades), on top of the colour theme.
pub const PALETTES: [&str; 4] = ["lines", "blocks", "minimal", "rounded"];

/// Panels a palette styles, as named in `[theme]` keys (`sidebar_outline`, `popup_background`).
pub const PANELS: [&str; 6] = [
    "sidebar", "messages", "composer", "popup", "header", "footer",
];

/// Outline names for `<panel>_outline`.
pub const OUTLINES: [&str; 6] = ["none", "rule", "single", "rounded", "double", "thick"];

/// The panels a palette styles. `Header` and `Footer` are one-line bars: background only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Sidebar,
    Messages,
    Composer,
    Popup,
    Header,
    Footer,
}

impl Panel {
    fn index(self) -> usize {
        self as usize
    }

    /// Whether `<panel>_outline` means anything (the bars have no outline).
    pub fn has_outline(name: &str) -> bool {
        !matches!(name, "header" | "footer")
    }

    pub fn label(name: &str) -> &'static str {
        match name {
            "sidebar" => "Sidebar",
            "messages" => "Messages",
            "composer" => "Message box",
            "popup" => "Popups",
            "header" => "Top bar",
            "footer" => "Footer",
            _ => "?",
        }
    }
}

/// How a panel's edge is drawn. Every outline keeps the same one-cell frame, so text sits in
/// the same place in every palette; `None` fills the frame with the panel's background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outline {
    None,
    /// A line along the top only, with the title in it.
    Rule,
    Single,
    Rounded,
    Double,
    Thick,
}

impl Outline {
    pub fn from_name(name: &str) -> Option<Outline> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "none" => Outline::None,
            "rule" => Outline::Rule,
            "single" => Outline::Single,
            "rounded" => Outline::Rounded,
            "double" => Outline::Double,
            "thick" => Outline::Thick,
            _ => return None,
        })
    }

    pub fn set(self) -> ratatui::symbols::border::Set<'static> {
        use ratatui::symbols::border;
        match self {
            Outline::None => border::EMPTY,
            Outline::Rule => border::Set {
                top_left: "─",
                top_right: "─",
                horizontal_top: "─",
                ..border::EMPTY
            },
            Outline::Single => border::PLAIN,
            Outline::Rounded => border::ROUNDED,
            Outline::Double => border::DOUBLE,
            Outline::Thick => border::THICK,
        }
    }
}

/// One panel's look, unfocused and focused.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelLook {
    pub outline: Outline,
    pub focus_outline: Outline,
    pub bg: Option<Color>,
    pub focus_bg: Option<Color>,
    /// The focused panel's outline takes the accent colour.
    pub focus_tint: bool,
}

impl PanelLook {
    const LINES: PanelLook = PanelLook {
        outline: Outline::Single,
        focus_outline: Outline::Double,
        bg: None,
        focus_bg: None,
        focus_tint: false,
    };
    const BAR: PanelLook = PanelLook {
        outline: Outline::None,
        focus_outline: Outline::None,
        bg: None,
        focus_bg: None,
        focus_tint: false,
    };

    fn plain(outline: Outline, focus_tint: bool) -> PanelLook {
        PanelLook {
            outline,
            focus_outline: outline,
            focus_tint,
            ..PanelLook::LINES
        }
    }

    /// A frameless panel with one fixed shade (`None` keeps the terminal's own background).
    /// Focus changes nothing: the selected row already shows where focus is.
    fn shaded(bg: Option<u8>) -> PanelLook {
        let bg = bg.map(Color::Indexed);
        PanelLook {
            outline: Outline::None,
            focus_outline: Outline::None,
            bg,
            focus_bg: bg,
            focus_tint: false,
        }
    }
}

/// The looks of every panel for a palette, in `Panel` order. Shades are from the 256-colour grey
/// ramp (232 near black to 255 near white), which every current terminal draws the same.
fn palette_looks(name: &str, light: bool) -> Option<[PanelLook; 6]> {
    let bar = PanelLook::BAR;
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "lines" => same(PanelLook::LINES),
        "blocks" => {
            // Fixed shades, none changing with focus. The messages panel keeps the terminal's own
            // background (a transparent terminal shows through there); terminals have no
            // per-cell transparency, so the rest are solid: sidebar and top bar in the middle,
            // popups next, the message box the lightest on dark.
            let (s, c, p, h) = if light {
                (254, 252, 253, 254)
            } else {
                (234, 236, 235, 234)
            };
            [
                PanelLook::shaded(Some(s)),
                PanelLook::shaded(None),
                PanelLook::shaded(Some(c)),
                PanelLook::shaded(Some(p)),
                PanelLook {
                    bg: Some(Color::Indexed(h)),
                    focus_bg: Some(Color::Indexed(h)),
                    ..bar
                },
                bar,
            ]
        }
        "minimal" => same(PanelLook::plain(Outline::Rule, true)),
        "rounded" => same(PanelLook::plain(Outline::Rounded, true)),
        _ => return None,
    })
}

/// One look for the four framed panels; the two bars stay plain.
fn same(p: PanelLook) -> [PanelLook; 6] {
    [p, p, p, p, PanelLook::BAR, PanelLook::BAR]
}

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
    /// Panel outlines and backgrounds, in `Panel` order (the palette plus per-panel keys).
    pub panels: [PanelLook; 6],
    /// Name colours in group chats: each person keeps one, picked from their id. Empty under
    /// NO_COLOR, where names are bold only.
    pub people: &'static [Color],
    /// The WhatsApp message status marks (`mark_sent`, `mark_delivered`, `mark_read`).
    pub marks: Marks,
}

/// The status column's marks: one character each, one column wide. They differ in shape, so
/// colour is never the only signal (a user who sets two the same is told).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marks {
    /// Typed here, not yet taken by the network (any network).
    pub sending: char,
    /// Sent, not on everyone's phone yet.
    pub sent: char,
    /// On everyone's phone, not read.
    pub delivered: char,
    /// The newest message everyone has read.
    pub read: char,
}

impl Default for Marks {
    fn default() -> Marks {
        Marks {
            sending: '◷',
            sent: '○',
            delivered: '●',
            read: '✓',
        }
    }
}

/// The `[theme]` keys of the marks, in the order a message goes through them.
pub const MARK_KEYS: [&str; 4] = ["mark_sending", "mark_sent", "mark_delivered", "mark_read"];

impl Marks {
    pub fn get(&self, key: &str) -> Option<char> {
        Some(match key {
            "mark_sending" => self.sending,
            "mark_sent" => self.sent,
            "mark_delivered" => self.delivered,
            "mark_read" => self.read,
            _ => return None,
        })
    }

    fn slot(&mut self, key: &str) -> Option<&mut char> {
        Some(match key {
            "mark_sending" => &mut self.sending,
            "mark_sent" => &mut self.sent,
            "mark_delivered" => &mut self.delivered,
            "mark_read" => &mut self.read,
            _ => return None,
        })
    }

    /// Every mark, in `MARK_KEYS` order.
    fn all(&self) -> [char; 4] {
        [self.sending, self.sent, self.delivered, self.read]
    }
}

/// A mark as typed: exactly one character that takes one terminal column (the status column
/// is one wide; an emoji takes two).
pub fn mark_char(value: &str) -> Result<char, String> {
    let v = value.trim();
    let mut chars = v.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() && crate::text::sw(v) == 1 => Ok(c),
        _ => Err(format!(
            "\"{v}\" is not one character one column wide (try ◷ ○ ● ✓ ◌ ◉ ✔ ·)."
        )),
    }
}

/// Name colours for a dark terminal: no red (errors) and no cyan of either shade (your own
/// name).
const PEOPLE_DARK: [Color; 7] = [
    Color::Green,
    Color::Yellow,
    Color::LightBlue,
    Color::Magenta,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
];

/// Name colours for a light terminal: no yellow, which washes out on white.
const PEOPLE_LIGHT: [Color; 6] = [
    Color::Blue,
    Color::Green,
    Color::Magenta,
    Color::LightBlue,
    Color::LightMagenta,
    Color::DarkGray,
];

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
            panels: palette_looks("lines", false).expect("lines is built in"),
            people: &PEOPLE_DARK,
            marks: Marks::default(),
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
                people: &PEOPLE_LIGHT,
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

    pub fn panel(&self, p: Panel) -> PanelLook {
        self.panels[p.index()]
    }

    /// One person's name colour, bold, the same every time for the same `key` (their id).
    pub fn person(&self, key: &str) -> Style {
        // FNV-1a: stable across runs and builds, unlike the std hasher.
        let h = key.bytes().fold(0x811c_9dc5_u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        });
        match self.people.len() {
            0 => bold(),
            n => fg(self.people[h as usize % n]).patch(bold()),
        }
    }

    /// `NO_COLOR`: every foreground and background goes. Roles that were only a colour keep a
    /// signal as an attribute: secondary text dims, errors and warnings go bold. A panel that
    /// was told apart by its shade alone gets a line instead.
    pub fn without_colour(mut self) -> Theme {
        for s in self.roles_mut() {
            s.fg = None;
            s.bg = None;
        }
        for p in &mut self.panels[..4] {
            if p.bg.is_some() && p.outline == Outline::None {
                p.outline = Outline::Single;
                p.focus_outline = Outline::Double;
            }
            p.bg = None;
            p.focus_bg = None;
        }
        for p in &mut self.panels[4..] {
            p.bg = None;
            p.focus_bg = None;
        }
        self.people = &[];
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
        let light = s.name.trim().eq_ignore_ascii_case("light");
        if let Some(name) = s.overrides.get("palette") {
            match palette_looks(name, light) {
                Some(looks) => theme.panels = looks,
                None => problems.push(problem(
                    Some("palette"),
                    format!(
                        "unknown palette \"{name}\": use {}. Using \"lines\".",
                        PALETTES.join(", ")
                    ),
                )),
            }
        }
        for (key, value) in &s.overrides {
            if key == "palette" {
                continue;
            }
            if let Some(slot) = theme.marks.slot(key) {
                match mark_char(value) {
                    Ok(c) => *slot = c,
                    Err(e) => {
                        problems.push(problem(Some(key), format!("{e} Keeping \"{}\".", *slot)))
                    }
                }
                continue;
            }
            if let Some((panel, part)) = panel_key(key) {
                let look = &mut theme.panels[panel];
                match part {
                    "outline" => match Outline::from_name(value) {
                        Some(o) => {
                            look.outline = o;
                            // Lines' double-when-focused stays for a single outline.
                            look.focus_outline =
                                if o == Outline::Single && look.focus_outline == Outline::Double {
                                    Outline::Double
                                } else {
                                    o
                                };
                        }
                        None => problems.push(problem(
                            Some(key),
                            format!(
                                "unknown outline \"{value}\": use {}. Keeping the palette's.",
                                OUTLINES.join(", ")
                            ),
                        )),
                    },
                    _ => match background(value) {
                        Ok(bg) => {
                            look.bg = bg;
                            look.focus_bg = bg;
                        }
                        Err(e) => problems.push(problem(
                            Some(key),
                            format!("{e} Keeping the palette's background."),
                        )),
                    },
                }
                continue;
            }
            let role = key;
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
        let marks = theme.marks.all();
        if (1..marks.len()).any(|i| marks[..i].contains(&marks[i])) {
            let shown: String = marks.iter().flat_map(|c| [*c, ' ']).collect();
            problems.push(SettingsProblem {
                severity: Severity::Warning,
                ..problem(
                    MARK_KEYS.iter().copied().find(|k| s.overrides.contains_key(*k)),
                    format!(
                        "two message marks are the same ({}): sending, sent, delivered and read must differ to be told apart.",
                        shown.trim_end()
                    ),
                )
            });
        }
        if no_colour {
            // Blocks has no fill on the messages panel, so without_colour would leave it
            // borderless: give it lines here, unless the user set its outline.
            let blocks = s
                .overrides
                .get("palette")
                .is_some_and(|n| n.trim().eq_ignore_ascii_case("blocks"));
            let look = &mut theme.panels[1];
            if blocks
                && look.outline == Outline::None
                && !s.overrides.contains_key("messages_outline")
            {
                look.outline = Outline::Single;
                look.focus_outline = Outline::Double;
            }
            theme = theme.without_colour();
        }
        (theme, problems)
    }
}

/// `sidebar_outline` -> (0, "outline"). Bars take a background only.
fn panel_key(key: &str) -> Option<(usize, &str)> {
    let (panel, part) = key.rsplit_once('_')?;
    let i = PANELS.iter().position(|p| *p == panel)?;
    match part {
        "background" => Some((i, "background")),
        "outline" if Panel::has_outline(panel) => Some((i, "outline")),
        _ => None,
    }
}

/// A panel background: `none` (the terminal's own) or any colour `parse_colour` takes.
pub fn background(value: &str) -> Result<Option<Color>, String> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    parse_colour(value).map(Some)
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
    fn message_marks_default_override_and_refuse_wide_or_long_values() {
        let (t, p) = Theme::from_settings(&settings("default", &[]), false);
        assert!(p.is_empty());
        assert_eq!(t.marks.all(), ['◷', '○', '●', '✓']);
        let (t, p) = Theme::from_settings(
            &settings("default", &[("mark_read", "✔"), ("mark_sent", "😀")]),
            false,
        );
        assert_eq!(t.marks.read, '✔');
        assert_eq!(t.marks.sent, '○', "a bad mark keeps the default");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].key.as_deref(), Some("mark_sent"));
        assert!(p[0].message.contains("Keeping \"○\""), "{}", p[0].message);
        // Two marks alike: kept, but the user is warned they cannot be told apart.
        let (t, p) = Theme::from_settings(&settings("default", &[("mark_read", "●")]), false);
        assert_eq!(t.marks.read, '●');
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].severity, Severity::Warning);
        assert_eq!(p[0].key.as_deref(), Some("mark_read"));
        // NO_COLOR keeps them: they are shapes.
        let (t, _) = Theme::from_settings(&settings("default", &[("mark_read", "✔")]), true);
        assert_eq!(t.marks.read, '✔');
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

    #[test]
    fn palettes_set_panel_looks() {
        let (t, p) = Theme::from_settings(&settings("default", &[("palette", "blocks")]), false);
        assert!(p.is_empty(), "{p:?}");
        let side = t.panel(Panel::Sidebar);
        assert_eq!(side.outline, Outline::None);
        assert_eq!(side.bg, Some(Color::Indexed(234)));
        let shades = |t: &Theme| {
            [
                Panel::Sidebar,
                Panel::Messages,
                Panel::Composer,
                Panel::Popup,
                Panel::Header,
            ]
            .map(|p| t.panel(p).bg)
        };
        let idx = |n| Some(Color::Indexed(n));
        assert_eq!(
            shades(&t),
            [idx(234), None, idx(236), idx(235), idx(234)],
            "chat window on the terminal background, message box the lightest"
        );
        for panel in [
            Panel::Sidebar,
            Panel::Messages,
            Panel::Composer,
            Panel::Popup,
            Panel::Header,
        ] {
            let look = t.panel(panel);
            assert_eq!(look.focus_bg, look.bg, "focus changes no shade");
            assert!(!look.focus_tint);
            assert_eq!(look.focus_outline, look.outline);
        }
        let (light, _) = Theme::from_settings(&settings("light", &[("palette", "blocks")]), false);
        assert_eq!(
            shades(&light),
            [idx(254), None, idx(252), idx(253), idx(254)]
        );
        let (lines, _) = Theme::from_settings(&settings("default", &[("palette", "lines")]), false);
        assert_eq!(
            lines,
            Theme::default(),
            "lines is the look without a palette"
        );
        for name in PALETTES {
            assert!(palette_looks(name, false).is_some(), "{name}");
        }
    }

    #[test]
    fn a_bad_palette_or_panel_value_is_one_problem_and_keeps_the_rest() {
        let s = settings(
            "default",
            &[
                ("palette", "neon"),
                ("sidebar_outline", "wiggly"),
                ("popup_background", "banana"),
                ("header_outline", "single"),
                ("messages_outline", "rounded"),
            ],
        );
        let (t, p) = Theme::from_settings(&s, false);
        let keys: Vec<_> = p.iter().map(|x| x.key.clone().unwrap()).collect();
        assert_eq!(
            keys,
            [
                "palette",
                "header_outline",
                "popup_background",
                "sidebar_outline"
            ],
            "{p:?}"
        );
        assert!(
            p.iter().all(|x| x.line.is_some()),
            "every problem names its line"
        );
        assert_eq!(t.panel(Panel::Sidebar), PanelLook::LINES);
        assert_eq!(t.panel(Panel::Messages).outline, Outline::Rounded);
    }

    #[test]
    fn panel_overrides_apply_on_top_of_the_palette() {
        let s = settings(
            "default",
            &[
                ("palette", "blocks"),
                ("sidebar_outline", "single"),
                ("sidebar_background", "none"),
                ("header_background", "#102030"),
            ],
        );
        let (t, p) = Theme::from_settings(&s, false);
        assert!(p.is_empty(), "{p:?}");
        let side = t.panel(Panel::Sidebar);
        assert_eq!(
            (side.outline, side.bg, side.focus_bg),
            (Outline::Single, None, None)
        );
        assert_eq!(
            t.panel(Panel::Header).bg,
            Some(Color::Rgb(0x10, 0x20, 0x30))
        );
        assert_eq!(t.panel(Panel::Messages).bg, None);
        assert_eq!(t.panel(Panel::Composer).bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn no_colour_turns_shaded_panels_into_lines() {
        let (t, _) = Theme::from_settings(&settings("default", &[("palette", "blocks")]), true);
        for p in [
            Panel::Sidebar,
            Panel::Messages,
            Panel::Composer,
            Panel::Popup,
        ] {
            let look = t.panel(p);
            assert_eq!(
                (look.outline, look.bg, look.focus_bg),
                (Outline::Single, None, None)
            );
        }
        assert_eq!(t.panel(Panel::Header).bg, None);
    }

    #[test]
    fn no_colour_keeps_an_outline_the_user_set_to_none() {
        let s = settings(
            "default",
            &[("palette", "lines"), ("messages_outline", "none")],
        );
        let (t, _) = Theme::from_settings(&s, true);
        assert_eq!(t.panel(Panel::Messages).outline, Outline::None);
        let s = settings(
            "default",
            &[("palette", "blocks"), ("messages_outline", "none")],
        );
        let (t, _) = Theme::from_settings(&s, true);
        assert_eq!(t.panel(Panel::Messages).outline, Outline::None);
    }
}
