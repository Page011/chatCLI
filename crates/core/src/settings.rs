//! `settings.toml`: the hand-edited settings file in the data folder.
//!
//! chatcli writes this file only from the settings screen, one key at a time, preserving
//! comments and unknown keys (`save_edits`). A file that does not parse is never written.
//! A missing file is all defaults, which is exactly the behaviour before settings existed. A
//! broken file never stops the app: each section is parsed on its own, a bad section falls back
//! to its defaults and produces one problem naming the line and key, and the other sections
//! still load.
//!
//! Core reports structure and value problems only. The TUI adds semantic ones for `[keys]` and
//! `[theme]` (action names, key strings, colour values), which core keeps as raw text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

const SECTIONS: [&str; 7] = [
    "keys",
    "theme",
    "layout",
    "notifications",
    "media",
    "teams",
    "whatsapp",
];
const LAYOUT_KEYS: [&str; 7] = [
    "sidebar_width",
    "time_format",
    "date_order",
    "density",
    "show_previews",
    "avatars",
    "photos",
];
const NOTIFICATION_KEYS: [&str; 3] = ["enabled", "preview", "sound"];
const MEDIA_KEYS: [&str; 5] = [
    "cache_max_mb",
    "retention_days",
    "max_file_mb",
    "inline_images",
    "image_protocol",
];
const TEAMS_KEYS: [&str; 3] = ["files_read", "files_write", "photos"];
const WHATSAPP_KEYS: [&str; 2] = ["send_read_receipts", "send_typing"];

/// Accepted spellings of each choice setting, in cycling order. The parser and the writer both
/// read these, so a name can never be written that would not load.
pub const TIME_FORMATS: [(&str, TimeFormat); 2] =
    [("24h", TimeFormat::H24), ("12h", TimeFormat::H12)];
pub const DATE_ORDERS: [(&str, DateOrder); 3] = [
    ("dmy", DateOrder::Dmy),
    ("mdy", DateOrder::Mdy),
    ("ymd", DateOrder::Ymd),
];
pub const DENSITIES: [(&str, Density); 2] =
    [("cozy", Density::Cozy), ("compact", Density::Compact)];
pub const AVATARS: [(&str, Avatars); 3] = [
    ("groups", Avatars::Groups),
    ("all", Avatars::All),
    ("off", Avatars::Off),
];
pub const NOTIFY_PREVIEWS: [(&str, NotifyPreview); 3] = [
    ("off", NotifyPreview::Off),
    ("sender", NotifyPreview::Sender),
    ("full", NotifyPreview::Full),
];
pub const IMAGE_PROTOCOLS: [(&str, ImageProtocol); 6] = [
    ("auto", ImageProtocol::Auto),
    ("sixel", ImageProtocol::Sixel),
    ("kitty", ImageProtocol::Kitty),
    ("iterm2", ImageProtocol::Iterm2),
    ("halfblocks", ImageProtocol::Halfblocks),
    ("off", ImageProtocol::Off),
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub keys: KeySettings,
    pub theme: ThemeSettings,
    pub layout: LayoutSettings,
    pub notifications: NotificationSettings,
    pub media: MediaSettings,
    pub teams: TeamsSettings,
    pub whatsapp: WhatsAppSettings,
}

/// Raw: `chatcli_tui` validates action names, key strings and contexts.
#[derive(Debug, Clone, PartialEq)]
pub struct KeySettings {
    /// "default" or "vim".
    pub preset: String,
    /// context -> key string -> action name, exactly as written.
    pub bindings: BTreeMap<String, BTreeMap<String, String>>,
    /// (context, key) -> line in the file, so the TUI's messages can name it.
    pub lines: BTreeMap<(String, String), usize>,
}

impl Default for KeySettings {
    fn default() -> Self {
        KeySettings {
            preset: "default".to_string(),
            bindings: BTreeMap::new(),
            lines: BTreeMap::new(),
        }
    }
}

/// Raw: `chatcli_tui` validates role names and colour values.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSettings {
    pub name: String,
    /// role -> value (every key except `name`). A TOML integer is kept as its digits.
    pub overrides: BTreeMap<String, String>,
    pub lines: BTreeMap<String, usize>,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        ThemeSettings {
            name: "default".to_string(),
            overrides: BTreeMap::new(),
            lines: BTreeMap::new(),
        }
    }
}

/// TOML integer 16..=120 columns, or a string "10%".."70%".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarWidth {
    Columns(u16),
    Percent(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeFormat {
    #[default]
    H24,
    H12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateOrder {
    #[default]
    Dmy,
    Mdy,
    Ymd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Cozy,
    Compact,
}

/// Where messages show an avatar with the sender's name above the text, Discord style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Avatars {
    /// Group chats only: a 1:1 chat keeps the one-line time-and-name layout.
    #[default]
    Groups,
    All,
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutSettings {
    /// None = today's width.
    pub sidebar_width: Option<SidebarWidth>,
    pub time_format: TimeFormat,
    pub date_order: DateOrder,
    pub density: Density,
    pub show_previews: bool,
    pub avatars: Avatars,
    /// Profile photos over the avatars (Sixel/Kitty/iTerm2 terminals only).
    pub photos: bool,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        LayoutSettings {
            sidebar_width: None,
            time_format: TimeFormat::default(),
            date_order: DateOrder::default(),
            density: Density::default(),
            show_previews: true,
            avatars: Avatars::default(),
            photos: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyPreview {
    /// No message text leaves the process.
    #[default]
    Off,
    Sender,
    Full,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationSettings {
    pub enabled: bool,
    pub preview: NotifyPreview,
    pub sound: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        NotificationSettings {
            enabled: true,
            preview: NotifyPreview::default(),
            sound: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageProtocol {
    #[default]
    Auto,
    Sixel,
    Kitty,
    Iterm2,
    Halfblocks,
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSettings {
    /// 1..=100000.
    pub cache_max_mb: u64,
    /// 1..=3650.
    pub retention_days: u32,
    /// 1..=2048, and never above `cache_max_mb`.
    pub max_file_mb: u64,
    pub inline_images: bool,
    pub image_protocol: ImageProtocol,
}

impl Default for MediaSettings {
    fn default() -> Self {
        MediaSettings {
            cache_max_mb: 200,
            retention_days: 30,
            max_file_mb: 25,
            inline_images: true,
            image_protocol: ImageProtocol::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TeamsSettings {
    /// Ask for Files.Read.All so Teams file attachments can be downloaded.
    pub files_read: bool,
    /// Ask for Files.ReadWrite so files can be sent.
    pub files_write: bool,
    /// Ask for User.ReadBasic.All so profile photos can be shown.
    pub photos: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WhatsAppSettings {
    /// Send read receipts when a chat is marked read.
    pub send_read_receipts: bool,
    /// Send our own typing indicator. Showing other people's is always on.
    pub send_typing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsProblem {
    pub severity: Severity,
    pub line: Option<usize>,
    /// Section path as written (`layout`, `keys.list`); "" for a file-wide problem.
    pub section: String,
    pub key: Option<String>,
    pub message: String,
}

impl SettingsProblem {
    /// One line for the notice bar or the CLI, for example
    /// `settings.toml line 12: [layout] density: use "cozy" or "compact". Using the defaults for [layout].`
    pub fn text(&self) -> String {
        let mut parts = vec![match self.line {
            Some(n) => format!("settings.toml line {n}:"),
            None => "settings.toml:".to_string(),
        }];
        match (self.section.is_empty(), &self.key) {
            (false, Some(key)) => parts.push(format!("[{}] {key}:", self.section)),
            (false, None) => parts.push(format!("[{}]:", self.section)),
            (true, _) => {}
        }
        parts.push(self.message.clone());
        parts.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadReport {
    pub path: PathBuf,
    pub settings: Settings,
    pub problems: Vec<SettingsProblem>,
}

/// Reads `settings.toml` from the data folder. Never fails: trouble becomes a problem.
pub fn load() -> LoadReport {
    match crate::paths::settings_file() {
        Ok(path) => load_from(&path),
        Err(e) => LoadReport {
            path: PathBuf::new(),
            settings: Settings::default(),
            problems: vec![file_problem(format!(
                "cannot find the data folder ({e}). Using the defaults."
            ))],
        },
    }
}

/// A missing file is the defaults with no problems.
pub fn load_from(path: &Path) -> LoadReport {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadReport {
            path: path.to_path_buf(),
            settings: Settings::default(),
            problems: Vec::new(),
        },
        Err(e) => LoadReport {
            path: path.to_path_buf(),
            settings: Settings::default(),
            problems: vec![file_problem(format!(
                "cannot read the file ({e}). Check it is UTF-8 text you can open. Using the defaults."
            ))],
        },
    }
}

fn file_problem(message: String) -> SettingsProblem {
    SettingsProblem {
        severity: Severity::Error,
        line: None,
        section: String::new(),
        key: None,
        message,
    }
}

/// A value problem found inside one section. `path` is set when it sits in a sub-table
/// (`keys.list`) rather than the section itself.
struct Bad {
    path: Option<String>,
    key: String,
    message: String,
}

fn bad(key: &str, message: impl Into<String>) -> Bad {
    Bad {
        path: None,
        key: key.to_string(),
        message: message.into(),
    }
}

/// Parses the whole file. Pure.
///
/// The text is cut into groups at top-level header lines (`[name]`, `[name.sub]`): every line
/// belongs to the group of the last header above it, lines before any header form the root
/// group. Each group is parsed by `toml` on its own, from a copy of the file where the other
/// groups' lines are blanked, so error line numbers are already the real ones. A syntax error
/// therefore costs only its own section.
///
/// Known limit: a multi-line array whose continuation line starts with `[` looks like a header.
/// No section in this schema uses a multi-line array.
pub fn parse(text: &str, path: &Path) -> LoadReport {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut groups: Vec<String> = vec![String::new()];
    let mut group_of: Vec<usize> = Vec::with_capacity(lines.len());
    let mut current = 0;
    for line in &lines {
        if let Some(name) = first_segment(line) {
            current = match groups.iter().position(|g| *g == name) {
                Some(i) => i,
                None => {
                    groups.push(name);
                    groups.len() - 1
                }
            };
        }
        group_of.push(current);
    }

    let mut problems = Vec::new();
    let mut tables: BTreeMap<String, Table> = BTreeMap::new();
    for (g, name) in groups.iter().enumerate() {
        let group_text = lines
            .iter()
            .zip(&group_of)
            .map(|(l, &gi)| if gi == g { *l } else { "" })
            .collect::<Vec<_>>()
            .join("\n");
        match toml::from_str::<Table>(&group_text) {
            Ok(table) => {
                tables.insert(name.clone(), table);
            }
            Err(e) => problems.push(SettingsProblem {
                severity: Severity::Error,
                line: error_line(&group_text, &e),
                section: name.clone(),
                key: None,
                message: if name.is_empty() {
                    format!(
                        "{}. Fix the syntax; settings above the first section are ignored.",
                        e.message()
                    )
                } else {
                    format!("{}. Using the defaults for [{name}].", e.message())
                },
            }),
        }
    }

    let table_of = |name: &str| -> Option<&Table> {
        tables
            .get(name)
            .and_then(|g| g.get(name))
            .and_then(Value::as_table)
    };
    let settings = Settings {
        keys: load_section(text, "keys", table_of("keys"), &mut problems, keys_section),
        theme: load_section(
            text,
            "theme",
            table_of("theme"),
            &mut problems,
            theme_section,
        ),
        layout: load_section(
            text,
            "layout",
            table_of("layout"),
            &mut problems,
            layout_section,
        ),
        notifications: load_section(
            text,
            "notifications",
            table_of("notifications"),
            &mut problems,
            notifications_section,
        ),
        media: load_section(
            text,
            "media",
            table_of("media"),
            &mut problems,
            media_section,
        ),
        teams: load_section(
            text,
            "teams",
            table_of("teams"),
            &mut problems,
            teams_section,
        ),
        whatsapp: load_section(
            text,
            "whatsapp",
            table_of("whatsapp"),
            &mut problems,
            whatsapp_section,
        ),
    };

    // One warning for everything chatcli does not know: typos are the common cause.
    let mut unknown: Vec<(String, Option<String>)> = Vec::new();
    if let Some(root) = tables.get("") {
        unknown.extend(root.keys().map(|k| (String::new(), Some(k.clone()))));
    }
    for name in groups.iter().filter(|g| !g.is_empty()) {
        if !SECTIONS.contains(&name.as_str()) {
            unknown.push((name.clone(), None));
        }
    }
    for (name, known) in [
        ("layout", &LAYOUT_KEYS[..]),
        ("notifications", &NOTIFICATION_KEYS[..]),
        ("media", &MEDIA_KEYS[..]),
        ("teams", &TEAMS_KEYS[..]),
        ("whatsapp", &WHATSAPP_KEYS[..]),
    ] {
        if let Some(t) = table_of(name) {
            unknown.extend(
                t.keys()
                    .filter(|k| !known.contains(&k.as_str()))
                    .map(|k| (name.to_string(), Some(k.clone()))),
            );
        }
    }
    if let Some(t) = table_of("keys") {
        // Besides `preset`, [keys] holds one table per context. A plain value cannot be a binding.
        unknown.extend(
            t.iter()
                .filter(|(k, v)| *k != "preset" && !v.is_table())
                .map(|(k, _)| ("keys".to_string(), Some(k.clone()))),
        );
    }
    if !unknown.is_empty() {
        problems.push(unknown_problem(text, &unknown));
    }

    LoadReport {
        path: path.to_path_buf(),
        settings,
        problems,
    }
}

fn unknown_problem(text: &str, unknown: &[(String, Option<String>)]) -> SettingsProblem {
    let names: Vec<String> = unknown
        .iter()
        .map(|(section, key)| match (section.is_empty(), key) {
            (true, Some(key)) => key.clone(),
            (false, Some(key)) => format!("[{section}] {key}"),
            (_, None) => format!("[{section}]"),
        })
        .collect();
    let (section, key) = &unknown[0];
    let line = match key {
        Some(key) => line_of(text, section, key),
        None => section_line(text, section),
    };
    SettingsProblem {
        severity: Severity::Warning,
        line,
        section: String::new(),
        key: None,
        message: format!(
            "unknown setting{} ignored: {}. Check the spelling.",
            if names.len() == 1 { "" } else { "s" },
            names.join(", ")
        ),
    }
}

/// Runs one section's reader. A missing section is the defaults; a bad value is the defaults
/// plus one error naming the line and key.
fn load_section<T: Default>(
    text: &str,
    name: &str,
    table: Option<&Table>,
    problems: &mut Vec<SettingsProblem>,
    read: impl FnOnce(&Table, &str) -> Result<T, Bad>,
) -> T {
    let Some(table) = table else {
        return T::default();
    };
    match read(table, text) {
        Ok(v) => v,
        Err(b) => {
            let section = b.path.unwrap_or_else(|| name.to_string());
            let line = line_of(text, &section, &b.key).or_else(|| section_line(text, &section));
            problems.push(SettingsProblem {
                severity: Severity::Error,
                line,
                section,
                key: Some(b.key),
                message: format!("{}. Using the defaults for [{name}].", b.message),
            });
            T::default()
        }
    }
}

fn keys_section(t: &Table, text: &str) -> Result<KeySettings, Bad> {
    let mut out = KeySettings::default();
    for (name, value) in t {
        match value {
            Value::String(s) if name == "preset" => out.preset = s.clone(),
            _ if name == "preset" => {
                return Err(bad(
                    "preset",
                    "use a preset name in quotes, such as \"default\" or \"vim\"",
                ))
            }
            Value::Table(context) => {
                let mut map = BTreeMap::new();
                for (key, action) in context {
                    let Value::String(action) = action else {
                        return Err(Bad {
                            path: Some(format!("keys.{name}")),
                            key: key.clone(),
                            message: "use an action name in quotes".to_string(),
                        });
                    };
                    map.insert(key.clone(), action.clone());
                    if let Some(line) = line_of(text, &format!("keys.{name}"), key) {
                        out.lines.insert((name.clone(), key.clone()), line);
                    }
                }
                out.bindings.insert(name.clone(), map);
            }
            // A plain value that is not `preset`: reported once as an unknown setting.
            _ => {}
        }
    }
    Ok(out)
}

fn theme_section(t: &Table, text: &str) -> Result<ThemeSettings, Bad> {
    let mut out = ThemeSettings::default();
    for (key, value) in t {
        let as_text = match value {
            Value::String(s) => s.clone(),
            Value::Integer(i) => i.to_string(),
            _ => {
                return Err(bad(
                    key,
                    "use a name in quotes, a number 0-255, or \"#rrggbb\"",
                ))
            }
        };
        if key == "name" {
            out.name = as_text;
        } else {
            out.overrides.insert(key.clone(), as_text);
            if let Some(line) = line_of(text, "theme", key) {
                out.lines.insert(key.clone(), line);
            }
        }
    }
    Ok(out)
}

fn layout_section(t: &Table, _text: &str) -> Result<LayoutSettings, Bad> {
    let d = LayoutSettings::default();
    Ok(LayoutSettings {
        sidebar_width: sidebar_width(t)?,
        time_format: choice(t, "time_format", d.time_format, &TIME_FORMATS)?,
        date_order: choice(t, "date_order", d.date_order, &DATE_ORDERS)?,
        density: choice(t, "density", d.density, &DENSITIES)?,
        show_previews: flag(t, "show_previews", d.show_previews)?,
        avatars: choice(t, "avatars", d.avatars, &AVATARS)?,
        photos: flag(t, "photos", d.photos)?,
    })
}

fn sidebar_width(t: &Table) -> Result<Option<SidebarWidth>, Bad> {
    let hint = "use 16 to 120 columns, or a percentage from \"10%\" to \"70%\"";
    match t.get("sidebar_width") {
        None => Ok(None),
        Some(Value::Integer(n)) if (16..=120).contains(n) => {
            Ok(Some(SidebarWidth::Columns(*n as u16)))
        }
        Some(Value::String(s)) => s
            .strip_suffix('%')
            .and_then(|n| n.trim().parse::<u8>().ok())
            .filter(|n| (10..=70).contains(n))
            .map(|n| Some(SidebarWidth::Percent(n)))
            .ok_or_else(|| bad("sidebar_width", hint)),
        Some(_) => Err(bad("sidebar_width", hint)),
    }
}

fn notifications_section(t: &Table, _text: &str) -> Result<NotificationSettings, Bad> {
    let d = NotificationSettings::default();
    Ok(NotificationSettings {
        enabled: flag(t, "enabled", d.enabled)?,
        preview: choice(t, "preview", d.preview, &NOTIFY_PREVIEWS)?,
        sound: flag(t, "sound", d.sound)?,
    })
}

fn media_section(t: &Table, _text: &str) -> Result<MediaSettings, Bad> {
    let d = MediaSettings::default();
    let out = MediaSettings {
        cache_max_mb: number(t, "cache_max_mb", d.cache_max_mb, 1, 100_000)?,
        retention_days: number(t, "retention_days", u64::from(d.retention_days), 1, 3650)? as u32,
        max_file_mb: number(t, "max_file_mb", d.max_file_mb, 1, 2048)?,
        inline_images: flag(t, "inline_images", d.inline_images)?,
        image_protocol: choice(t, "image_protocol", d.image_protocol, &IMAGE_PROTOCOLS)?,
    };
    if out.max_file_mb > out.cache_max_mb {
        return Err(bad(
            "max_file_mb",
            format!(
                "must not be above cache_max_mb ({}): raise cache_max_mb or lower max_file_mb",
                out.cache_max_mb
            ),
        ));
    }
    Ok(out)
}

fn teams_section(t: &Table, _text: &str) -> Result<TeamsSettings, Bad> {
    Ok(TeamsSettings {
        files_read: flag(t, "files_read", false)?,
        files_write: flag(t, "files_write", false)?,
        photos: flag(t, "photos", false)?,
    })
}

fn whatsapp_section(t: &Table, _text: &str) -> Result<WhatsAppSettings, Bad> {
    Ok(WhatsAppSettings {
        send_read_receipts: flag(t, "send_read_receipts", false)?,
        send_typing: flag(t, "send_typing", false)?,
    })
}

fn flag(t: &Table, key: &str, default: bool) -> Result<bool, Bad> {
    match t.get(key) {
        None => Ok(default),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(_) => Err(bad(key, "use true or false")),
    }
}

fn number(t: &Table, key: &str, default: u64, low: u64, high: u64) -> Result<u64, Bad> {
    match t.get(key) {
        None => Ok(default),
        Some(Value::Integer(n)) if *n >= low as i64 && *n <= high as i64 => Ok(*n as u64),
        Some(_) => Err(bad(key, format!("use a whole number from {low} to {high}"))),
    }
}

fn choice<T: Copy>(t: &Table, key: &str, default: T, options: &[(&str, T)]) -> Result<T, Bad> {
    match t.get(key) {
        None => Ok(default),
        Some(Value::String(s)) => options
            .iter()
            .find(|(name, _)| name == s)
            .map(|(_, v)| *v)
            .ok_or_else(|| bad(key, options_hint(options))),
        Some(_) => Err(bad(key, options_hint(options))),
    }
}

/// `use "a", "b" or "c"`.
fn options_hint<T>(options: &[(&str, T)]) -> String {
    let quoted: Vec<String> = options.iter().map(|(n, _)| format!("\"{n}\"")).collect();
    match quoted.split_last() {
        Some((last, [])) => format!("use {last}"),
        Some((last, rest)) => format!("use {} or {last}", rest.join(", ")),
        None => "use one of the documented values".to_string(),
    }
}

/// The section name of a top-level header line: `[name]`, `[ name ]`, `[name.sub]`.
/// `[[array]]` and quoted names do not match, and stay with the group above.
fn first_segment(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let after = rest[end..].trim_start();
    (after.starts_with(']') || after.starts_with('.')).then(|| rest[..end].to_string())
}

/// The full dotted path of a header line (`keys.list`), whitespace and quotes removed.
fn header_path(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?;
    if rest.starts_with('[') {
        return None;
    }
    let inner = &rest[..rest.find(']')?];
    Some(
        inner
            .split('.')
            .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// The key of a `key = value` line: bare or quoted. None for comments, headers and the rest.
fn key_of(line: &str) -> Option<String> {
    let line = line.trim_start();
    let (key, rest) = match line.chars().next()? {
        q @ ('"' | '\'') => {
            let body = &line[1..];
            let end = body.find(q)?;
            (body[..end].to_string(), &body[end + 1..])
        }
        _ => {
            let end = line
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(line.len());
            (line[..end].to_string(), &line[end..])
        }
    };
    (!key.is_empty() && rest.trim_start().starts_with('=')).then_some(key)
}

/// 1-based line of `key = ...` inside the table whose header path is `section` (`layout`,
/// `keys.list`). None when it is not there (for example an inherited default).
pub fn line_of(text: &str, section: &str, key: &str) -> Option<usize> {
    let mut current = String::new();
    for (i, raw) in text.lines().enumerate() {
        if let Some(path) = header_path(raw) {
            current = path;
        } else if current == section && key_of(raw).as_deref() == Some(key) {
            return Some(i + 1);
        }
    }
    None
}

/// 1-based line of the first header at or below `section`.
fn section_line(text: &str, section: &str) -> Option<usize> {
    text.lines()
        .position(|raw| {
            header_path(raw).is_some_and(|p| {
                p == section || p.strip_prefix(section).is_some_and(|r| r.starts_with('.'))
            })
        })
        .map(|i| i + 1)
}

/// 1-based line of a toml error, from its byte span.
fn error_line(text: &str, e: &toml::de::Error) -> Option<usize> {
    let start = e.span()?.start.min(text.len());
    Some(
        text.as_bytes()[..start]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1,
    )
}

// --- writing ----------------------------------------------------------------------------

/// A value the settings screen writes. Always one of the three TOML scalars the schema uses.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Str(String),
}

impl SettingValue {
    pub fn str(s: &str) -> SettingValue {
        SettingValue::Str(s.to_string())
    }
}

/// One key to set or remove in `settings.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingEdit {
    /// Dotted table path: `layout`, `theme`, `keys`, `keys.list`.
    pub table: String,
    /// The key as written for removals; the `to_setting()` form for new keys.
    pub key: String,
    /// None = remove the key (back to the default).
    pub value: Option<SettingValue>,
}

impl SettingEdit {
    pub fn set(table: &str, key: &str, value: SettingValue) -> SettingEdit {
        SettingEdit {
            table: table.to_string(),
            key: key.to_string(),
            value: Some(value),
        }
    }

    pub fn remove(table: &str, key: &str) -> SettingEdit {
        SettingEdit {
            table: table.to_string(),
            key: key.to_string(),
            value: None,
        }
    }
}

/// The written spelling of `value` in `options`.
fn spelling<T: Copy + PartialEq>(options: &[(&'static str, T)], value: T) -> &'static str {
    options
        .iter()
        .find(|(_, v)| *v == value)
        .map_or(options[0].0, |(name, _)| *name)
}

impl TimeFormat {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&TIME_FORMATS, self))
    }
}

impl DateOrder {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&DATE_ORDERS, self))
    }
}

impl Density {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&DENSITIES, self))
    }
}

impl Avatars {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&AVATARS, self))
    }
}

impl NotifyPreview {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&NOTIFY_PREVIEWS, self))
    }
}

impl ImageProtocol {
    pub fn to_setting(self) -> SettingValue {
        SettingValue::str(spelling(&IMAGE_PROTOCOLS, self))
    }
}

impl SidebarWidth {
    /// Columns are a TOML integer, a percentage is the string "30%".
    pub fn to_setting(self) -> SettingValue {
        match self {
            SidebarWidth::Columns(n) => SettingValue::Int(i64::from(n)),
            SidebarWidth::Percent(p) => SettingValue::Str(format!("{p}%")),
        }
    }
}

/// Applies `edits` to the file at `path` and saves it atomically. Only the named keys change:
/// comments, blank lines, unknown keys and sections with value problems stay as the user wrote
/// them, and a hand edit made since chatcli started is kept (the file is read again here).
///
/// Never loses the file: the new text is written to a unique temp file and renamed over the
/// old one; any failure leaves the old file as it was and no temp file behind. A file that does
/// not parse as TOML is never written (`Err`): rewriting it would mangle what the user typed.
/// Errors start with `NOT SAVED` and are one sentence for the notice line.
pub fn save_edits(path: &Path, edits: &[SettingEdit]) -> crate::Result<()> {
    // Saves in this process run one at a time, so two of them never both read the old text
    // and drop each other's change. Another process editing the file can still race: the last
    // rename wins, and the file is whole either way.
    static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = SAVING.lock().unwrap_or_else(|e| e.into_inner());

    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("NOT SAVED - cannot read settings.toml: {e}")),
    };
    match apply_edits(&original, edits)? {
        Some(out) => {
            crate::config::write_atomic(path, &out).map_err(|e| format!("NOT SAVED - {e}"))
        }
        None => Ok(()),
    }
}

/// Pure half of `save_edits`: the new file text, or None when nothing changes.
fn apply_edits(original: &str, edits: &[SettingEdit]) -> crate::Result<Option<String>> {
    use toml_edit::DocumentMut;

    let (bom, body) = match original.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", original),
    };
    let mut doc: DocumentMut = body.parse().map_err(|e: toml_edit::TomlError| {
        let line = e.span().map(|s| {
            let at = s.start.min(body.len());
            body.as_bytes()[..at]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        });
        match line {
            Some(n) => format!(
                "NOT SAVED - settings.toml line {n} has a syntax error: fix it in a text editor"
            ),
            None => {
                "NOT SAVED - settings.toml has a syntax error: fix it in a text editor".to_string()
            }
        }
    })?;
    for edit in edits {
        let path: Vec<&str> = edit.table.split('.').filter(|s| !s.is_empty()).collect();
        apply_in(doc.as_table_mut(), &path, edit, &mut None)
            .map_err(|e| format!("NOT SAVED - {e}"))?;
    }
    let mut out = doc.to_string();
    if out == body {
        return Ok(None);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // Belt and braces: whatever toml_edit produced must still be valid TOML.
    if toml::from_str::<Table>(&out).is_err() {
        return Err("NOT SAVED - internal error, file left as it was".to_string());
    }
    Ok(Some(format!("{bom}{out}")))
}

/// The comment lines of `prefix` (a key's leading decor), if it has any.
fn comment_of(prefix: Option<&toml_edit::RawString>) -> Option<String> {
    prefix
        .and_then(|p| p.as_str())
        .filter(|s| s.contains('#'))
        .map(str::to_string)
}

/// Removes `key`. The comment lines above it are not lost: they move to the key after it, or,
/// when it was the last, go to `orphan` for the caller (the table header takes them).
fn remove_keeping_comment(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    orphan: &mut Option<String>,
) -> bool {
    let comment = table
        .key(key)
        .and_then(|k| comment_of(k.leaf_decor().prefix()));
    let next = table
        .iter()
        .map(|(k, _)| k.to_string())
        .skip_while(|k| k != key)
        .nth(1);
    let removed = table.remove(key).is_some();
    if let (true, Some(comment)) = (removed, comment) {
        match next.and_then(|n| table.key_mut(&n)) {
            Some(mut k) => {
                let old = k
                    .leaf_decor()
                    .prefix()
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string();
                k.leaf_decor_mut().set_prefix(format!("{comment}{old}"));
            }
            None => *orphan = Some(comment),
        }
    }
    removed
}

/// Walks `path` below `table` (creating missing tables for a set) and applies `edit` there.
/// Returns whether anything was removed, so only a table this edit emptied is pruned.
fn apply_in(
    table: &mut dyn toml_edit::TableLike,
    path: &[&str],
    edit: &SettingEdit,
    orphan: &mut Option<String>,
) -> Result<bool, String> {
    use toml_edit::{Item, Table, Value};

    let Some((seg, rest)) = path.split_first() else {
        return Ok(match &edit.value {
            None => remove_keeping_comment(table, &edit.key, orphan),
            Some(v) => {
                let mut new = match v {
                    SettingValue::Bool(b) => Value::from(*b),
                    SettingValue::Int(i) => Value::from(*i),
                    SettingValue::Str(s) => Value::from(s.as_str()),
                };
                match table.get_mut(&edit.key) {
                    // Assign in place: the key keeps its spelling and position, the value its
                    // trailing comment.
                    Some(old) => {
                        if let Some(decor) = old.as_value().map(|v| v.decor().clone()) {
                            *new.decor_mut() = decor;
                        }
                        *old = Item::Value(new);
                    }
                    None => {
                        table.insert(&edit.key, Item::Value(new));
                    }
                }
                false
            }
        });
    };
    if !table.contains_key(seg) {
        if edit.value.is_none() {
            return Ok(false);
        }
        let mut made = Table::new();
        // `[keys.list]` alone: no empty `[keys]` header above it.
        made.set_implicit(!rest.is_empty());
        table.insert(seg, Item::Table(made));
    }
    let item = table.get_mut(seg).expect("table was just checked or made");
    let child = item
        .as_table_like_mut()
        .ok_or_else(|| format!("[{}] is not a table: fix it in a text editor", edit.table))?;
    let removed = apply_in(child, rest, edit, orphan)?;
    // The last key's comment goes above the header, which then stays even when empty.
    if let (true, Some(comment)) = (removed && rest.is_empty(), orphan.take()) {
        if let Some(t) = item.as_table_mut() {
            let old = t
                .decor()
                .prefix()
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            t.decor_mut().set_prefix(format!("{old}{comment}"));
        }
    }
    // A table this removal emptied goes too, unless a comment hangs on its header.
    let prune = removed
        && item.as_table().is_some_and(|t| {
            let plain = |r: Option<&toml_edit::RawString>| {
                r.and_then(|r| r.as_str()).is_none_or(|s| !s.contains('#'))
            };
            t.is_empty() && plain(t.decor().prefix()) && plain(t.decor().suffix())
        });
    if prune {
        table.remove(seg);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> LoadReport {
        parse(text, Path::new("settings.toml"))
    }

    fn errors(r: &LoadReport) -> Vec<&SettingsProblem> {
        r.problems
            .iter()
            .filter(|p| p.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn empty_and_missing_file_are_the_defaults() {
        let r = load("");
        assert_eq!(r.settings, Settings::default());
        assert!(r.problems.is_empty());
        let r = load("# only a comment\n\n");
        assert_eq!(r.settings, Settings::default());
        assert!(r.problems.is_empty());
        let r = load_from(Path::new("Z:/chatcli-test/does/not/exist.toml"));
        assert_eq!(r.settings, Settings::default());
        assert!(r.problems.is_empty());
    }

    #[test]
    fn defaults_are_todays_behaviour() {
        let s = Settings::default();
        assert_eq!(s.keys.preset, "default");
        assert_eq!(s.theme.name, "default");
        assert_eq!(s.layout.sidebar_width, None);
        assert!(s.layout.show_previews);
        assert!(s.notifications.enabled && s.notifications.sound);
        assert_eq!(s.notifications.preview, NotifyPreview::Off);
        assert_eq!(
            (
                s.media.cache_max_mb,
                s.media.retention_days,
                s.media.max_file_mb
            ),
            (200, 30, 25)
        );
        assert!(!s.teams.files_read && !s.teams.files_write && !s.teams.photos);
        assert!(s.layout.photos);
        assert!(!s.whatsapp.send_read_receipts && !s.whatsapp.send_typing);
    }

    #[test]
    fn a_full_file_loads_every_section() {
        let r = load(
            r##"
[keys]
preset = "vim"

[keys.list]
"ctrl+n" = "list_down"
x = "unbound"

[theme]
name = "high-contrast"
accent = "red"
dim = 8
chip_live = "#00ff00"

[layout]
sidebar_width = "30%"
time_format = "12h"
date_order = "ymd"
density = "compact"
show_previews = false
photos = false

[notifications]
enabled = false
preview = "sender"
sound = false

[media]
cache_max_mb = 500
retention_days = 7
max_file_mb = 50
inline_images = false
image_protocol = "kitty"

[teams]
files_read = true
files_write = true
photos = true

[whatsapp]
send_read_receipts = true
send_typing = true
"##,
        );
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        let s = r.settings;
        assert_eq!(s.keys.preset, "vim");
        assert_eq!(s.keys.bindings["list"]["ctrl+n"], "list_down");
        assert_eq!(s.keys.bindings["list"]["x"], "unbound");
        assert_eq!(s.keys.lines[&("list".to_string(), "ctrl+n".to_string())], 6);
        assert_eq!(s.theme.name, "high-contrast");
        assert_eq!(s.theme.overrides["dim"], "8");
        assert_eq!(s.theme.overrides["chip_live"], "#00ff00");
        assert_eq!(s.theme.lines["accent"], 11);
        assert_eq!(s.layout.sidebar_width, Some(SidebarWidth::Percent(30)));
        assert_eq!(s.layout.time_format, TimeFormat::H12);
        assert_eq!(s.layout.date_order, DateOrder::Ymd);
        assert_eq!(s.layout.density, Density::Compact);
        assert!(!s.layout.show_previews && !s.layout.photos);
        assert!(!s.notifications.enabled && !s.notifications.sound);
        assert_eq!(s.notifications.preview, NotifyPreview::Sender);
        assert_eq!(s.media.cache_max_mb, 500);
        assert_eq!(s.media.retention_days, 7);
        assert_eq!(s.media.max_file_mb, 50);
        assert!(!s.media.inline_images);
        assert_eq!(s.media.image_protocol, ImageProtocol::Kitty);
        assert!(s.teams.files_read && s.teams.files_write && s.teams.photos);
        assert!(s.whatsapp.send_read_receipts && s.whatsapp.send_typing);
    }

    #[test]
    fn sidebar_width_takes_columns_or_percent_in_range() {
        let w = |v: &str| {
            load(&format!("[layout]\nsidebar_width = {v}\n"))
                .settings
                .layout
                .sidebar_width
        };
        assert_eq!(w("16"), Some(SidebarWidth::Columns(16)));
        assert_eq!(w("120"), Some(SidebarWidth::Columns(120)));
        assert_eq!(w("\"10%\""), Some(SidebarWidth::Percent(10)));
        assert_eq!(w("\"70%\""), Some(SidebarWidth::Percent(70)));
        for bad in [
            "15", "121", "\"9%\"", "\"71%\"", "\"30\"", "\"x%\"", "true", "2.5",
        ] {
            let r = load(&format!("[layout]\nsidebar_width = {bad}\n"));
            assert_eq!(errors(&r).len(), 1, "{bad} must be refused");
            assert_eq!(r.settings.layout, LayoutSettings::default());
        }
    }

    #[test]
    fn a_syntax_error_names_file_and_line_and_other_sections_still_load() {
        let r = load("[layout]\ndensity = \"compact\"\n\n[media]\ncache_max_mb = = 5\n\n[teams]\nfiles_read = true\n");
        let e = errors(&r);
        assert_eq!(e.len(), 1, "{:?}", r.problems);
        assert_eq!(e[0].line, Some(5));
        assert_eq!(e[0].section, "media");
        assert!(
            e[0].text().starts_with("settings.toml line 5: [media]:"),
            "{}",
            e[0].text()
        );
        assert_eq!(r.settings.layout.density, Density::Compact);
        assert!(r.settings.teams.files_read);
        assert_eq!(r.settings.media, MediaSettings::default());
    }

    #[test]
    fn a_syntax_error_in_the_root_group_is_reported_with_its_line() {
        let r = load("oops = \n[layout]\ndensity = \"compact\"\n");
        let e = errors(&r);
        assert_eq!(e.len(), 1, "{:?}", r.problems);
        assert_eq!(e[0].line, Some(1));
        assert_eq!(r.settings.layout.density, Density::Compact);
    }

    #[test]
    fn syntax_errors_after_a_split_section_keep_real_line_numbers() {
        // [keys] and [keys.list] are one group around [layout]: the error line stays 9.
        let r = load("[keys]\npreset = \"vim\"\n\n[layout]\nshow_previews = false\n\n[keys.list]\n\"a\" = \"x\"\n\"b\" = \n");
        let e = errors(&r);
        assert_eq!(e.len(), 1, "{:?}", r.problems);
        assert_eq!(e[0].line, Some(9));
        assert_eq!(e[0].section, "keys");
        assert!(!r.settings.layout.show_previews, "layout still loads");
        assert_eq!(r.settings.keys, KeySettings::default());
    }

    #[test]
    fn a_bad_value_keeps_the_other_sections_and_names_line_and_key() {
        let r = load("[layout]\ntime_format = \"12h\"\ndensity = \"tiny\"\n\n[notifications]\nsound = false\n");
        let e = errors(&r);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].line, e[0].key.as_deref()), (Some(3), Some("density")));
        assert_eq!(
            e[0].text(),
            "settings.toml line 3: [layout] density: use \"cozy\" or \"compact\". Using the defaults for [layout]."
        );
        assert_eq!(
            r.settings.layout,
            LayoutSettings::default(),
            "the whole section falls back"
        );
        assert!(
            !r.settings.notifications.sound,
            "other sections keep their values"
        );
    }

    #[test]
    fn range_and_type_checks() {
        for text in [
            "[media]\ncache_max_mb = 0\n",
            "[media]\ncache_max_mb = 100001\n",
            "[media]\nretention_days = 0\n",
            "[media]\nretention_days = 3651\n",
            "[media]\nmax_file_mb = 2049\ncache_max_mb = 5000\n",
            "[media]\nmax_file_mb = 0\n",
            "[media]\ninline_images = \"yes\"\n",
            "[media]\nimage_protocol = \"png\"\n",
            "[media]\ncache_max_mb = \"big\"\n",
            "[layout]\nshow_previews = 1\n",
            "[notifications]\npreview = \"all\"\n",
            "[teams]\nfiles_read = \"true\"\n",
            "[whatsapp]\nsend_typing = 0\n",
        ] {
            let r = load(text);
            assert_eq!(errors(&r).len(), 1, "{text:?} -> {:?}", r.problems);
        }
        // Edges inside the range load fine.
        let r = load("[media]\ncache_max_mb = 100000\nretention_days = 3650\nmax_file_mb = 2048\n");
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.settings.media.max_file_mb, 2048);
    }

    #[test]
    fn max_file_above_cache_is_an_error_and_media_uses_defaults() {
        let r =
            load("[media]\ncache_max_mb = 10\nmax_file_mb = 25\n[layout]\ndensity = \"compact\"\n");
        let e = errors(&r);
        assert_eq!(e.len(), 1, "{:?}", r.problems);
        assert_eq!(
            (e[0].line, e[0].key.as_deref()),
            (Some(3), Some("max_file_mb"))
        );
        assert!(
            e[0].message.contains("cache_max_mb (10)"),
            "{}",
            e[0].message
        );
        assert_eq!(r.settings.media, MediaSettings::default());
        assert_eq!(r.settings.layout.density, Density::Compact);
    }

    #[test]
    fn unknown_things_make_one_warning_and_the_values_still_load() {
        let r = load(
            "stray = 1\n[layout]\ndensty = \"compact\"\ndensity = \"compact\"\n[colors]\nx = 1\n[teams]\nfiles_reed = true\n[keys]\npreset = \"vim\"\nbogus = 2\n",
        );
        assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
        let w = &r.problems[0];
        assert_eq!(w.severity, Severity::Warning);
        assert_eq!(w.line, Some(1));
        for name in [
            "stray",
            "[layout] densty",
            "[colors]",
            "[teams] files_reed",
            "[keys] bogus",
        ] {
            assert!(w.message.contains(name), "{name} missing in {}", w.message);
        }
        assert!(w.message.starts_with("unknown settings ignored:"));
        assert_eq!(
            r.settings.layout.density,
            Density::Compact,
            "known keys still load"
        );
        assert_eq!(r.settings.keys.preset, "vim");
        let one = load("[layout]\ndensty = 1\n");
        assert!(one.problems[0]
            .message
            .starts_with("unknown setting ignored: [layout] densty"));
        assert_eq!(one.problems[0].line, Some(2));
        assert!(one.problems[0]
            .text()
            .starts_with("settings.toml line 2: unknown setting"));
    }

    #[test]
    fn a_wrong_typed_key_value_is_an_error_naming_the_context_line() {
        let r = load("[keys]\npreset = \"default\"\n\n[keys.list]\n\"ctrl+n\" = 5\n");
        let e = errors(&r);
        assert_eq!(e.len(), 1, "{:?}", r.problems);
        assert_eq!(e[0].section, "keys.list");
        assert_eq!(e[0].line, Some(5));
        assert_eq!(r.settings.keys, KeySettings::default());
        let r = load("[keys]\npreset = 3\n");
        assert_eq!(errors(&r)[0].key.as_deref(), Some("preset"));
        let r = load("[theme]\naccent = true\n");
        assert_eq!(errors(&r).len(), 1);
    }

    #[test]
    fn header_spacing_and_split_groups() {
        let r = load("[ layout ]\ndensity = \"compact\"\n\n[ keys . list ]\n\"a\" = \"quit\"\n[layout.extra]\nx = 1\n");
        // [layout.extra] is a sub-table named `extra` of layout: unknown key, not an error.
        assert!(errors(&r).is_empty(), "{:?}", r.problems);
        assert_eq!(r.settings.layout.density, Density::Compact);
        assert_eq!(r.settings.keys.bindings["list"]["a"], "quit");
        assert!(r.problems[0].message.contains("[layout] extra"));
    }

    #[test]
    fn a_leading_byte_order_mark_and_crlf_are_fine() {
        let r = load("\u{FEFF}[layout]\r\ndensity = \"compact\"\r\n");
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.settings.layout.density, Density::Compact);
    }

    #[test]
    fn line_of_finds_keys_per_table() {
        let text = "root = 1\n[layout]\n# density = \"x\"\ndensity = \"cozy\"\n[keys.list]\n\"ctrl+n\" = \"a\"\n  'x' = \"b\"\n[keys.panel]\n\"ctrl+n\" = \"c\"\n";
        assert_eq!(line_of(text, "layout", "density"), Some(4));
        assert_eq!(line_of(text, "keys.list", "ctrl+n"), Some(6));
        assert_eq!(line_of(text, "keys.list", "x"), Some(7));
        assert_eq!(line_of(text, "keys.panel", "ctrl+n"), Some(9));
        assert_eq!(line_of(text, "layout", "missing"), None);
        assert_eq!(line_of(text, "media", "density"), None);
        assert_eq!(line_of(text, "", "root"), Some(1));
        assert_eq!(section_line(text, "keys"), Some(5));
    }

    #[test]
    fn problem_text_forms() {
        let p = SettingsProblem {
            severity: Severity::Error,
            line: None,
            section: "media".into(),
            key: None,
            message: "broken".into(),
        };
        assert_eq!(p.text(), "settings.toml: [media]: broken");
        let p = SettingsProblem {
            severity: Severity::Warning,
            line: Some(2),
            section: String::new(),
            key: None,
            message: "unknown setting ignored: x. Check the spelling.".into(),
        };
        assert_eq!(
            p.text(),
            "settings.toml line 2: unknown setting ignored: x. Check the spelling."
        );
    }

    #[test]
    fn load_from_reads_a_real_file_and_reports_unreadable_ones() {
        let dir =
            std::env::temp_dir().join(format!("chatcli-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "[layout]\ndensity = \"compact\"\n").unwrap();
        let r = load_from(&path);
        assert_eq!(r.path, path);
        assert_eq!(r.settings.layout.density, Density::Compact);
        // A folder where the file should be is not a missing file.
        let r = load_from(&dir);
        assert_eq!(errors(&r).len(), 1);
        assert_eq!(r.settings, Settings::default());
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(errors(&load_from(&path)).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_hint_reads_naturally() {
        assert_eq!(options_hint(&[("a", 1)]), "use \"a\"");
        assert_eq!(options_hint(&[("a", 1), ("b", 2)]), "use \"a\" or \"b\"");
        assert_eq!(
            options_hint(&[("a", 1), ("b", 2), ("c", 3)]),
            "use \"a\", \"b\" or \"c\""
        );
    }

    // --- save_edits -------------------------------------------------------------------------

    fn set(table: &str, key: &str, v: SettingValue) -> SettingEdit {
        SettingEdit::set(table, key, v)
    }

    /// Runs `edits` over `text` and returns the new text (the input when nothing changes).
    fn edited(text: &str, edits: &[SettingEdit]) -> String {
        apply_edits(text, edits)
            .unwrap()
            .unwrap_or_else(|| text.to_string())
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chatcli-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temps_in(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count()
    }

    const COMMENTED: &str = "# my settings\n\n[layout]\n# how wide\nsidebar_width = 30 # wide\ndensity = \"cozy\"\nmystery = 1\n\n[extra]\nnote = \"kept\"\n";

    #[test]
    fn a_round_trip_keeps_comments_and_unknown_keys() {
        let out = edited(
            COMMENTED,
            &[
                set("layout", "density", SettingValue::str("compact")),
                set("layout", "sidebar_width", SettingValue::Int(40)),
            ],
        );
        // Only the two values moved; the trailing comment stays on its line.
        assert_eq!(
            out,
            COMMENTED
                .replace("\"cozy\"", "\"compact\"")
                .replace("= 30 # wide", "= 40 # wide")
        );
        let r = load(&out);
        assert_eq!(r.settings.layout.density, Density::Compact);
        assert_eq!(
            r.settings.layout.sidebar_width,
            Some(SidebarWidth::Columns(40))
        );
    }

    #[test]
    fn the_photos_keys_round_trip_and_keep_comments() {
        let text = "[layout]\n# my note\ndensity = \"cozy\"\n\n[teams]\nfiles_read = true\n";
        let out = edited(
            text,
            &[
                set("layout", "photos", SettingValue::Bool(false)),
                set("teams", "photos", SettingValue::Bool(true)),
            ],
        );
        assert!(out.contains("# my note"), "{out}");
        let r = load(&out);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert!(!r.settings.layout.photos && r.settings.teams.photos);
        assert!(r.settings.teams.files_read);
    }

    #[test]
    fn a_new_key_goes_into_its_table_and_a_new_table_is_created() {
        let out = edited(
            COMMENTED,
            &[set("layout", "show_previews", SettingValue::Bool(false))],
        );
        assert!(out.contains("show_previews = false"), "{out}");
        assert!(out.starts_with("# my settings\n"));
        assert!(!load(&out).settings.layout.show_previews);

        let out = edited(
            "",
            &[set("layout", "density", SettingValue::str("compact"))],
        );
        assert_eq!(out, "[layout]\ndensity = \"compact\"\n");
        let out = edited(
            "[teams]\nfiles_read = true\n",
            &[set("whatsapp", "send_typing", SettingValue::Bool(true))],
        );
        let r = load(&out);
        assert!(r.settings.teams.files_read && r.settings.whatsapp.send_typing);
    }

    #[test]
    fn a_key_table_has_no_empty_parent_and_quotes_the_key() {
        let out = edited(
            "",
            &[set("keys.list", "ctrl+n", SettingValue::str("refresh"))],
        );
        assert_eq!(out, "[keys.list]\n\"ctrl+n\" = \"refresh\"\n");
        assert!(!out.contains("[keys]"));
        let r = load(&out);
        assert_eq!(r.settings.keys.bindings["list"]["ctrl+n"], "refresh");
        // A second key joins the same table.
        let out = edited(&out, &[set("keys.list", "r", SettingValue::str("unbound"))]);
        assert_eq!(out.matches("[keys.list]").count(), 1, "{out}");
        assert_eq!(load(&out).settings.keys.bindings["list"].len(), 2);
    }

    #[test]
    fn removing_a_key_prunes_an_emptied_table_unless_a_comment_hangs_on_it() {
        let text = "[layout]\ndensity = \"compact\"\n\n[teams]\nfiles_read = true\n";
        let out = edited(text, &[SettingEdit::remove("layout", "density")]);
        assert!(!out.contains("[layout]"), "{out}");
        assert!(out.contains("files_read = true"));
        // Removing what is not there changes nothing, not even an empty table.
        assert_eq!(
            apply_edits("[layout]\n", &[SettingEdit::remove("layout", "density")]).unwrap(),
            None
        );

        let text = "# keep this table\n[layout]\ndensity = \"compact\"\n";
        let out = edited(text, &[SettingEdit::remove("layout", "density")]);
        assert!(
            out.contains("# keep this table") && out.contains("[layout]"),
            "{out}"
        );
        let text = "[layout] # mine\ndensity = \"compact\"\n";
        assert!(edited(text, &[SettingEdit::remove("layout", "density")]).contains("[layout]"));

        // Both [keys.list] and its parent go when the last binding is removed.
        let text = "[keys.list]\n\"f5\" = \"refresh\"\n";
        let out = edited(text, &[SettingEdit::remove("keys.list", "f5")]);
        assert_eq!(out.trim(), "");
    }

    #[test]
    fn removing_a_key_keeps_the_comment_above_it() {
        // Next key takes the comment.
        let text = "[layout]
# laptop: compact fits more chats
density = \"compact\"
time_format = \"12h\"
";
        let out = edited(text, &[SettingEdit::remove("layout", "density")]);
        assert!(out.contains("# laptop: compact fits more chats"), "{out}");
        assert!(out.contains("time_format"), "{out}");
        assert!(!out.contains("density"), "{out}");
        // The last key: the comment goes above the header, which stays.
        let text = "[layout]
# laptop: compact fits more chats
density = \"compact\"
";
        let out = edited(text, &[SettingEdit::remove("layout", "density")]);
        assert!(out.contains("# laptop: compact fits more chats"), "{out}");
        assert!(
            out.contains("[layout]") && !out.contains("density"),
            "{out}"
        );
        assert!(toml::from_str::<Table>(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_bom_and_crlf_survive() {
        let text = "\u{FEFF}[layout]\r\ndensity = \"cozy\"\r\n";
        let out = edited(
            text,
            &[set("layout", "density", SettingValue::str("compact"))],
        );
        assert!(out.starts_with('\u{FEFF}'), "{out:?}");
        let r = load(&out);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.settings.layout.density, Density::Compact);
        let out = edited(
            text,
            &[set("media", "cache_max_mb", SettingValue::Int(500))],
        );
        assert_eq!(load(&out).settings.media.cache_max_mb, 500);
    }

    #[test]
    fn a_broken_file_is_refused_with_its_line_and_nothing_is_written() {
        let dir = temp_dir("broken");
        let path = dir.join("settings.toml");
        let text = "[layout]\ndensity = \"cozy\"\n[media\ncache_max_mb = 5\n";
        std::fs::write(&path, text).unwrap();
        let err = save_edits(
            &path,
            &[set("layout", "density", SettingValue::str("compact"))],
        )
        .unwrap_err();
        assert!(err.starts_with("NOT SAVED - settings.toml line 3"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(temps_in(&dir), 0);
        // A key whose path is not a table is refused too.
        std::fs::write(&path, "layout = 3\n").unwrap();
        let err = save_edits(
            &path,
            &[set("layout", "density", SettingValue::str("compact"))],
        )
        .unwrap_err();
        assert!(err.contains("[layout] is not a table"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout = 3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_value_problem_in_another_section_is_left_as_written() {
        let dir = temp_dir("other");
        let path = dir.join("settings.toml");
        let text = "[layout]\ndensity = \"tiny\"\n\n[media]\ncache_max_mb = 300\n";
        std::fs::write(&path, text).unwrap();
        save_edits(
            &path,
            &[set("media", "cache_max_mb", SettingValue::Int(400))],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text.replace("300", "400")
        );
        let r = load_from(&path);
        assert_eq!(r.settings.media.cache_max_mb, 400);
        assert_eq!(errors(&r).len(), 1, "density is still the user's problem");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_created_and_an_unchanged_save_writes_nothing() {
        let dir = temp_dir("create");
        let path = dir.join("settings.toml");
        save_edits(
            &path,
            &[set("layout", "show_previews", SettingValue::Bool(false))],
        )
        .unwrap();
        assert!(!load_from(&path).settings.layout.show_previews);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        save_edits(
            &path,
            &[set("layout", "show_previews", SettingValue::Bool(false))],
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // Removing the only key from a file we created leaves an empty file, still loadable.
        save_edits(&path, &[SettingEdit::remove("layout", "show_previews")]).unwrap();
        assert!(load_from(&path).problems.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rename_keeps_the_old_file_and_leaves_no_temp_file() {
        let dir = temp_dir("rename");
        // The target is a folder holding a file, so the rename cannot replace it.
        let path = dir.join("settings.toml");
        std::fs::create_dir_all(path.join("inner")).unwrap();
        // The folder cannot be read as text either: that is a read failure, also not saved.
        let err = save_edits(
            &path,
            &[set("layout", "density", SettingValue::str("compact"))],
        )
        .unwrap_err();
        assert!(err.starts_with("NOT SAVED"), "{err}");
        assert_eq!(temps_in(&dir), 0);
        assert!(path.join("inner").is_dir());
        // The rename itself failing (file readable, target replaceable only by a folder swap):
        // write_atomic over a folder, the same step save_edits ends with.
        let err = crate::config::write_atomic(&path, "x = 1\n").unwrap_err();
        assert!(err.starts_with("cannot replace"), "{err}");
        assert_eq!(temps_in(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real failure: an old settings file exists and its replacement cannot be written (a
    /// read-only file, or one an editor holds). The old bytes stay and no temp file is left.
    #[cfg(windows)]
    #[test]
    fn a_read_only_settings_file_stays_as_it_was() {
        let dir = temp_dir("readonly");
        let path = dir.join("settings.toml");
        std::fs::write(
            &path,
            "# mine
[layout]
density = \"cozy\"
",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        let err = save_edits(
            &path,
            &[set("layout", "density", SettingValue::str("compact"))],
        )
        .unwrap_err();
        assert!(err.starts_with("NOT SAVED"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine
[layout]
density = \"cozy\"
"
        );
        assert_eq!(temps_in(&dir), 0);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_never_corrupt_the_file_and_lose_no_key() {
        let dir = temp_dir("race");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "# mine\n[layout]\ndensity = \"cozy\"\n").unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..10 {
                        let key = format!("k{i}_{n}");
                        save_edits(&path, &[set("extra", &key, SettingValue::Int(n))])?;
                    }
                    Ok::<(), String>(())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every save succeeds");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n"));
        let table: Table = toml::from_str(&text).unwrap();
        assert_eq!(
            table["extra"].as_table().unwrap().len(),
            80,
            "an edit was lost"
        );
        assert_eq!(temps_in(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_setting_the_screen_changes_round_trips_through_the_parser() {
        let mut edits = Vec::new();
        for (_, v) in TIME_FORMATS {
            edits.push(("layout", "time_format", v.to_setting()));
        }
        for (_, v) in DATE_ORDERS {
            edits.push(("layout", "date_order", v.to_setting()));
        }
        for (_, v) in DENSITIES {
            edits.push(("layout", "density", v.to_setting()));
        }
        for (_, v) in NOTIFY_PREVIEWS {
            edits.push(("notifications", "preview", v.to_setting()));
        }
        for (_, v) in IMAGE_PROTOCOLS {
            edits.push(("media", "image_protocol", v.to_setting()));
        }
        for w in [
            SidebarWidth::Columns(16),
            SidebarWidth::Columns(120),
            SidebarWidth::Percent(10),
            SidebarWidth::Percent(70),
        ] {
            edits.push(("layout", "sidebar_width", w.to_setting()));
        }
        for (table, key, value) in edits {
            let out = edited("", &[set(table, key, value.clone())]);
            let r = load(&out);
            assert!(
                r.problems.is_empty(),
                "{table}.{key} = {value:?}: {:?}",
                r.problems
            );
        }
        // Spot-check the values read back, not just that they load.
        let one = |t: &str, k: &str, v: SettingValue| load(&edited("", &[set(t, k, v)])).settings;
        assert_eq!(
            one("layout", "date_order", DateOrder::Ymd.to_setting())
                .layout
                .date_order,
            DateOrder::Ymd
        );
        assert_eq!(
            one(
                "layout",
                "sidebar_width",
                SidebarWidth::Percent(35).to_setting()
            )
            .layout
            .sidebar_width,
            Some(SidebarWidth::Percent(35))
        );
        assert_eq!(
            one(
                "media",
                "image_protocol",
                ImageProtocol::Iterm2.to_setting()
            )
            .media
            .image_protocol,
            ImageProtocol::Iterm2
        );
        // Colours and the key preset are plain strings.
        let s = one("theme", "name", SettingValue::str("light"));
        assert_eq!(s.theme.name, "light");
        let s = one("keys", "preset", SettingValue::str("vim"));
        assert_eq!(s.keys.preset, "vim");
        let out = edited("", &[set("theme", "unread", SettingValue::str("#12ab3f"))]);
        assert_eq!(load(&out).settings.theme.overrides["unread"], "#12ab3f");
    }
}
