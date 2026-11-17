//! The settings screen's model: sections, rows and what one change does. Pure: no drawing, no
//! files. `change` returns the new settings plus the `settings.toml` edits that go with them;
//! `App` applies the settings at once and queues the edits for the run loop to save.

use chatcli_core::settings::{
    Avatars, DateOrder, Density, ImageProtocol, NotifyPreview, SettingEdit, SettingValue, Settings,
    SidebarWidth, TimeFormat, AVATARS, DATE_ORDERS, DENSITIES, IMAGE_PROTOCOLS, NOTIFY_PREVIEWS,
    TIME_FORMATS,
};

use crate::input::TextInput;
use crate::keymap::{actions_for, default_keys, parse_key, Action, Context, KeyChord, Keymap};
use crate::theme::{
    background, mark_char, parse_colour, Marks, Outline, MARK_KEYS, PALETTES, ROLES, THEMES,
};

/// A page of the settings screen, opened by `/settings`, `/theme`, `/keys`, `/layout`,
/// `/notifications` and `/about`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Appearance,
    Layout,
    Notifications,
    Media,
    Teams,
    WhatsApp,
    Keys,
    About,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Appearance,
        Section::Layout,
        Section::Notifications,
        Section::Media,
        Section::Teams,
        Section::WhatsApp,
        Section::Keys,
        Section::About,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Layout => "Layout",
            Section::Notifications => "Notifications",
            Section::Media => "Media",
            Section::Teams => "Teams",
            Section::WhatsApp => "WhatsApp",
            Section::Keys => "Keys",
            Section::About => "About",
        }
    }

    /// The section `step` places away, wrapping at both ends.
    pub fn step(self, step: isize) -> Section {
        let i = Section::ALL.iter().position(|s| *s == self).unwrap_or(0) as isize;
        Section::ALL[(i + step).rem_euclid(Section::ALL.len() as isize) as usize]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsPage {
    pub section: Section,
    /// The cursor is in the right-hand rows, not the section list.
    pub in_rows: bool,
    /// Index into `rows(..)`.
    pub row: usize,
    /// First row drawn.
    pub top: usize,
    pub edit: Option<PageEdit>,
    /// Red line for the selected row: a range error or a failed save.
    pub error: Option<String>,
    /// The Keys page shows only the actions that match this (`filter_key_rows`).
    pub filter: TextInput,
    /// Keys go to `filter` (after `/`) until Enter or Esc.
    pub filtering: bool,
}

/// A value being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageEdit {
    Text(TextInput),
    /// Waiting for the key that will run `action` in `ctx`. The key is read before the keymap.
    Capture {
        ctx: Context,
        action: Action,
    },
    /// `key` already runs `other` in `ctx`: Enter swaps, Esc cancels.
    Conflict {
        ctx: Context,
        action: Action,
        key: KeyChord,
        other: Action,
    },
    /// Waiting for a key to add to `action` in `ctx`, keeping its other keys.
    Add {
        ctx: Context,
        action: Action,
    },
    /// `key` is free in `ctx` but overlaps a binding live at the same time (`note` says which):
    /// Enter adds it anyway, Esc cancels.
    AddOverlap {
        ctx: Context,
        action: Action,
        key: KeyChord,
        note: String,
    },
    /// `key` is free for `action` in `ctx` but would stop something else (`note` says what): Enter
    /// binds it anyway, Esc cancels.
    CaptureOverlap {
        ctx: Context,
        action: Action,
        key: KeyChord,
        note: String,
    },
    /// Choosing which of the action's keys to delete: `index` into `Keymap::keys_in`.
    Pick {
        ctx: Context,
        action: Action,
        index: usize,
    },
}

impl PageEdit {
    /// True while the page reads raw key presses for a key binding (not typed text).
    pub(crate) fn is_key_edit(&self) -> bool {
        !matches!(self, PageEdit::Text(_))
    }
}

/// The footer line while a key is being captured or a swap is asked, else None.
pub(crate) fn capture_line(edit: Option<&PageEdit>) -> Option<String> {
    match edit? {
        PageEdit::Text(_) => None,
        PageEdit::Capture { action, .. } => Some(format!(
            "Press the new key for {} (any key is taken, Esc too)",
            action.label()
        )),
        PageEdit::Conflict {
            key, other, action, ..
        } => Some(format!(
            "{} is already {}. Enter: {} gets {}'s old key (or none), Esc cancels",
            key.display(),
            other.label(),
            other.label(),
            action.label()
        )),
        PageEdit::Add { action, .. } => Some(format!(
            "Press a key to add to {} (any key is taken, Esc too)",
            action.label()
        )),
        PageEdit::AddOverlap { note, .. } => {
            Some(format!("{note}. Enter adds it anyway, Esc cancels"))
        }
        PageEdit::CaptureOverlap { note, .. } => {
            Some(format!("{note}. Enter binds it anyway, Esc cancels"))
        }
        // Short enough for 40 columns: the row being edited already names the action.
        PageEdit::Pick { .. } => Some("Left/Right pick, d delete, Esc cancel".to_string()),
    }
}

impl SettingsPage {
    pub fn new(section: Section) -> SettingsPage {
        SettingsPage {
            section,
            in_rows: false,
            row: 0,
            top: 0,
            edit: None,
            error: None,
            filter: TextInput::default(),
            filtering: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowId {
    ThemeName,
    /// `[theme] palette`: lines, blocks, minimal, rounded.
    Palette,
    /// `[theme] <panel>_outline`.
    Outline(&'static str),
    /// One of `theme::ROLES`.
    Role(&'static str),
    /// `[theme] mark_sent`, `mark_delivered` or `mark_read`: one character.
    Mark(&'static str),
    SidebarWidth,
    TimeFormat,
    DateOrder,
    Density,
    ShowPreviews,
    Avatars,
    Photos,
    NotifEnabled,
    NotifPreview,
    NotifSound,
    CacheMaxMb,
    RetentionDays,
    MaxFileMb,
    InlineImages,
    ImageProtocol,
    FilesRead,
    FilesWrite,
    TeamsPhotos,
    SendReadReceipts,
    SendTyping,
    DownloadImages,
    /// A read-only line (About, the problems header, the Keys note).
    Info(&'static str),
    /// A read-only settings.toml problem (About), by position.
    Problem(usize),
    /// Keys page: a context's name, not selectable.
    KeyHeading(Context),
    /// Keys page: one action in one context. Enter captures its key.
    KeyAction(Context, Action),
    /// Keys page: the last row, asks before removing every key you set.
    ResetAllKeys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Read-only.
    Info,
    /// Left/Right/Space/Enter change it.
    Cycle,
    /// Left/Right step it, Enter types a value.
    Edit,
    /// A group title (the Keys page): bold, not selectable.
    Heading,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub id: RowId,
    pub label: String,
    pub value: String,
    /// One line, shown for the selected row.
    pub hint: &'static str,
    /// The file overrides the default.
    pub custom: bool,
    pub kind: Kind,
}

impl Row {
    pub fn selectable(&self) -> bool {
        !matches!(self.kind, Kind::Info | Kind::Heading)
    }

    /// Red text: the problems header and the problem lines.
    pub fn is_error(&self) -> bool {
        matches!(self.id, RowId::Problem(_) | RowId::Info("problems"))
    }
}

const SIDEBAR_STEPS: [Option<SidebarWidth>; 6] = [
    None,
    Some(SidebarWidth::Percent(20)),
    Some(SidebarWidth::Percent(25)),
    Some(SidebarWidth::Percent(30)),
    Some(SidebarWidth::Percent(35)),
    Some(SidebarWidth::Percent(40)),
];

/// Colour choices for Left/Right: `default` removes the override.
const COLOURS: [&str; 17] = [
    "default",
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "gray",
    "dark_gray",
    "light_red",
    "light_green",
    "light_yellow",
    "light_blue",
    "light_magenta",
    "light_cyan",
    "white",
];

pub(crate) const MSG_CACHE: &str = "NOT A VALID SIZE - use a whole number from 1 to 100000";
pub(crate) const MSG_DAYS: &str = "NOT A VALID NUMBER OF DAYS - use a whole number from 1 to 3650";
pub(crate) const MSG_FILE: &str = "NOT A VALID SIZE - use a whole number from 1 to 2048";
pub(crate) const MSG_WIDTH: &str = "NOT A VALID WIDTH - use auto, 16 to 120 columns, or 10% to 70%";

/// Plain words for a theme role (the file's word is the role name itself).
pub(crate) fn role_label(role: &str) -> &'static str {
    match role {
        "text" => "Text",
        "dim" => "Dim text",
        "hint" => "Hints",
        "accent" => "Accent",
        "own_name" => "Your name",
        "unread" => "Unread count",
        "error" => "Errors",
        "warn" => "Warnings",
        "ok" => "OK and progress",
        "selected" => "Selected row",
        "chip_live" => "Status: live",
        "chip_attention" => "Status: attention",
        "network_teams" => "Teams tab",
        "network_whatsapp" => "WhatsApp tab",
        "sidebar_background" => "Sidebar fill",
        "messages_background" => "Messages fill",
        "composer_background" => "Message box fill",
        "popup_background" => "Popups fill",
        "header_background" => "Top bar fill",
        "footer_background" => "Footer fill",
        "sidebar_outline" => "Sidebar outline",
        "messages_outline" => "Messages outline",
        "composer_outline" => "Message box outline",
        "popup_outline" => "Popups outline",
        _ => "?",
    }
}

/// `[theme]` keys for each panel's outline (the bars have none) and background, in screen order.
const PANEL_ROWS: [(&str, Option<&str>); 6] = [
    ("sidebar_background", Some("sidebar_outline")),
    ("messages_background", Some("messages_outline")),
    ("composer_background", Some("composer_outline")),
    ("popup_background", Some("popup_outline")),
    ("header_background", None),
    ("footer_background", None),
];

/// Message mark choices for Left/Right (any one-column character can be typed).
const MARK_CHOICES: [char; 10] = ['○', '●', '✓', '◌', '◉', '✔', '·', '*', '◷', ' '];

/// Labels of the `MARK_KEYS` rows, in the same order.
const MARK_LABELS: [&str; 4] = ["Sending mark", "Sent mark", "Delivered mark", "Read mark"];

/// Outline choices for Left/Right: `default` removes the override (the palette decides).
const OUTLINE_CHOICES: [&str; 7] = [
    "default", "none", "rule", "single", "rounded", "double", "thick",
];

fn is_background(key: &str) -> bool {
    key.ends_with("_background")
}

/// Left/Right choices for a colour row; a background can also be `none`.
fn colour_choices(key: &str) -> Vec<&'static str> {
    let mut out = COLOURS.to_vec();
    if is_background(key) {
        out.insert(1, "none");
    }
    out
}

fn row(id: RowId, label: &str, value: String, hint: &'static str, custom: bool) -> Row {
    Row {
        id,
        label: label.to_string(),
        value,
        hint,
        custom,
        kind: match id {
            RowId::Info(_) | RowId::Problem(_) => Kind::Info,
            RowId::KeyHeading(_) => Kind::Heading,
            RowId::Role(_)
            | RowId::Mark(_)
            | RowId::SidebarWidth
            | RowId::CacheMaxMb
            | RowId::RetentionDays
            | RowId::MaxFileMb => Kind::Edit,
            _ => Kind::Cycle,
        },
    }
}

/// Top line of Appearance while NO_COLOR is set.
pub(crate) fn no_colour_row() -> Row {
    row(
        RowId::Info("no_colour"),
        "",
        "NO_COLOR SET - colours are off; saved choices apply without it".to_string(),
        "",
        false,
    )
}

fn name_of<T: Copy + PartialEq>(options: &[(&'static str, T)], v: T) -> String {
    options
        .iter()
        .find(|(_, o)| *o == v)
        .map_or("?", |(n, _)| *n)
        .to_string()
}

fn on_off(b: bool) -> String {
    if b { "on" } else { "off" }.to_string()
}

fn width_text(w: Option<SidebarWidth>) -> String {
    match w {
        None => "auto".to_string(),
        Some(SidebarWidth::Columns(n)) => n.to_string(),
        Some(SidebarWidth::Percent(p)) => format!("{p}%"),
    }
}

/// The text a typed edit of `id` starts from.
pub(crate) fn edit_start(s: &Settings, id: RowId) -> String {
    match id {
        RowId::Role(r) => s.theme.overrides.get(r).cloned().unwrap_or_default(),
        RowId::Mark(k) => s.theme.overrides.get(k).cloned().unwrap_or_default(),
        RowId::SidebarWidth => width_text(s.layout.sidebar_width),
        RowId::CacheMaxMb => s.media.cache_max_mb.to_string(),
        RowId::RetentionDays => s.media.retention_days.to_string(),
        RowId::MaxFileMb => s.media.max_file_mb.to_string(),
        _ => String::new(),
    }
}

/// The user's `[keys.<ctx>]` names `action`, or unbinds one of its built-in keys.
fn key_is_custom(s: &Settings, ctx: Context, action: Action) -> bool {
    let Some(table) = s.keys.bindings.get(ctx.name()) else {
        return false;
    };
    let own = default_keys(ctx, action);
    table.iter().any(|(k, v)| {
        v == action.name()
            || (v == Action::Unbound.name() && parse_key(k).is_ok_and(|c| own.contains(&c)))
    })
}

/// The keys of `action` in `ctx` as the page shows them: `-` when none, ` *` when yours.
fn key_value(keys: &Keymap, ctx: Context, action: Action, custom: bool) -> String {
    let mut parts: Vec<String> = keys
        .keys_in(ctx, action)
        .iter()
        .map(KeyChord::display)
        .collect();
    parts.extend(keys.sequences_in(ctx, action));
    let mut value = if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    };
    if custom {
        value.push_str(" *");
    }
    value
}

/// The Keys page: a legend, then every context with its actions, then Reset all.
fn key_rows(out: &mut Vec<Row>, s: &Settings, keys: &Keymap) {
    out.push(row(
        RowId::Info("keys"),
        "",
        "* marks a key you changed".to_string(),
        "",
        false,
    ));
    for ctx in Context::ALL {
        // A context whose keys all come from `common` (the sign-in panel, the filter, the
        // popups) has no rows of its own: no heading either.
        let actions = actions_for(ctx, &s.keys);
        if actions.is_empty() {
            continue;
        }
        out.push(row(
            RowId::KeyHeading(ctx),
            ctx.title(),
            String::new(),
            "",
            false,
        ));
        for a in actions {
            let custom = key_is_custom(s, ctx, a);
            out.push(row(
                RowId::KeyAction(ctx, a),
                &a.label(),
                key_value(keys, ctx, a, custom),
                a.doc(),
                custom,
            ));
        }
    }
    out.push(row(
        RowId::ResetAllKeys,
        "Reset all keys",
        String::new(),
        "Enter removes every key you set, after y/n",
        false,
    ));
}

/// The Keys page cut down to the actions that match `query` (ignoring case) in their label, their
/// `settings.toml` name, their keys or the context's title. A heading stays only above a
/// match; the legend becomes the filter line, and Reset all goes. An empty query changes nothing.
pub(crate) fn filter_key_rows(rows: Vec<Row>, query: &str) -> Vec<Row> {
    use crate::list::matches;
    if query.is_empty() {
        return rows;
    }
    let hit = |r: &Row| match r.id {
        RowId::KeyAction(ctx, a) => [r.label.as_str(), a.name(), r.value.as_str(), ctx.title()]
            .iter()
            .any(|t| matches(t, query)),
        _ => false,
    };
    let found = rows.iter().filter(|r| hit(r)).count();
    let mut out = Vec::new();
    let mut heading: Option<Row> = None;
    for r in rows {
        match r.id {
            RowId::Info("keys") => out.push(row(
                RowId::Info("filter"),
                "",
                match found {
                    0 => format!("No keys match \"{query}\" - Esc clears"),
                    _ => format!("Keys matching \"{query}\" - Esc clears"),
                },
                "",
                false,
            )),
            RowId::KeyHeading(_) => heading = Some(r),
            RowId::KeyAction(..) if hit(&r) => {
                out.extend(heading.take());
                out.push(r);
            }
            RowId::KeyAction(..) | RowId::ResetAllKeys => {}
            _ => out.push(r),
        }
    }
    out
}

/// The rows of `section`. `keys` is the live keymap (the Keys page). `about` and `problems` feed the About page and the header above every
/// other page.
pub(crate) fn rows(
    section: Section,
    s: &Settings,
    keys: &Keymap,
    about: &[(&'static str, String)],
    problems: &[String],
) -> Vec<Row> {
    let d = Settings::default();
    let mut out = Vec::new();
    if !problems.is_empty() && section != Section::About {
        out.push(row(
            RowId::Info("problems"),
            "",
            format!("SETTINGS FILE PROBLEMS: {} - see About", problems.len()),
            "",
            false,
        ));
    }
    match section {
        Section::Appearance => {
            out.push(row(
                RowId::ThemeName,
                "Theme",
                s.theme.name.clone(),
                "default, high-contrast, light, dark",
                s.theme.name != d.theme.name,
            ));
            let palette = s.theme.overrides.get("palette");
            out.push(row(
                RowId::Palette,
                "Palette",
                palette.cloned().unwrap_or_else(|| "lines".to_string()),
                "lines, blocks, minimal, rounded",
                palette.is_some(),
            ));
            let marks = Marks::default();
            for (key, label) in MARK_KEYS.into_iter().zip(MARK_LABELS) {
                let v = s.theme.overrides.get(key);
                let mark = v
                    .and_then(|v| mark_char(v).ok())
                    .or_else(|| marks.get(key))
                    .unwrap_or(' ');
                out.push(row(
                    RowId::Mark(key),
                    label,
                    // A blank value would look like a missing one.
                    if mark == ' ' {
                        "none (space)".to_string()
                    } else {
                        mark.to_string()
                    },
                    "one character, e.g. ◷ ○ ● ✓ ◌ ◉ ✔ ·, or a space for none",
                    v.is_some(),
                ));
            }
            for (bg, outline) in PANEL_ROWS {
                if let Some(key) = outline {
                    let v = s.theme.overrides.get(key);
                    out.push(row(
                        RowId::Outline(key),
                        role_label(key),
                        v.cloned().unwrap_or_else(|| "default".to_string()),
                        "default, none, rule, single, rounded, double, thick",
                        v.is_some(),
                    ));
                }
                let v = s.theme.overrides.get(bg);
                out.push(row(
                    RowId::Role(bg),
                    role_label(bg),
                    v.cloned().unwrap_or_else(|| "default".to_string()),
                    "default, none, a name or #rrggbb",
                    v.is_some(),
                ));
            }
            for r in ROLES {
                let custom = s.theme.overrides.get(r);
                out.push(row(
                    RowId::Role(r),
                    role_label(r),
                    custom.cloned().unwrap_or_else(|| "default".to_string()),
                    "name, default or #rrggbb",
                    custom.is_some(),
                ));
            }
        }
        Section::Layout => {
            let l = &s.layout;
            out.push(row(
                RowId::SidebarWidth,
                "Sidebar width",
                width_text(l.sidebar_width),
                "auto, 16-120 columns or 10%-70%",
                l.sidebar_width != d.layout.sidebar_width,
            ));
            out.push(row(
                RowId::TimeFormat,
                "Time format",
                name_of(&TIME_FORMATS, l.time_format),
                "24h or 12h",
                l.time_format != d.layout.time_format,
            ));
            out.push(row(
                RowId::DateOrder,
                "Date order",
                name_of(&DATE_ORDERS, l.date_order),
                "dmy, mdy or ymd",
                l.date_order != d.layout.date_order,
            ));
            out.push(row(
                RowId::Density,
                "Density",
                name_of(&DENSITIES, l.density),
                "cozy or compact",
                l.density != d.layout.density,
            ));
            out.push(row(
                RowId::ShowPreviews,
                "Show previews",
                on_off(l.show_previews),
                "last message under each chat",
                l.show_previews != d.layout.show_previews,
            ));
            out.push(row(
                RowId::Avatars,
                "Avatars",
                name_of(&AVATARS, l.avatars),
                "groups, all or off",
                l.avatars != d.layout.avatars,
            ));
            out.push(row(
                RowId::Photos,
                "Photos",
                on_off(l.photos),
                "in avatars, Sixel/Kitty/iTerm2 only",
                l.photos != d.layout.photos,
            ));
        }
        Section::Notifications => {
            let n = &s.notifications;
            out.push(row(
                RowId::NotifEnabled,
                "Enabled",
                on_off(n.enabled),
                "desktop toasts for new messages",
                n.enabled != d.notifications.enabled,
            ));
            out.push(row(
                RowId::NotifPreview,
                "Preview",
                name_of(&NOTIFY_PREVIEWS, n.preview),
                "off or sender: no message text in toasts",
                n.preview != d.notifications.preview,
            ));
            out.push(row(
                RowId::NotifSound,
                "Sound",
                on_off(n.sound),
                "play a sound",
                n.sound != d.notifications.sound,
            ));
        }
        Section::Media => {
            let m = &s.media;
            let dm = &d.media;
            out.push(row(
                RowId::CacheMaxMb,
                "Cache size",
                format!("{} MB", m.cache_max_mb),
                "1-100000 MB, used at next download or start",
                m.cache_max_mb != dm.cache_max_mb,
            ));
            out.push(row(
                RowId::RetentionDays,
                "Keep files",
                format!("{} days", m.retention_days),
                "1-3650 days, used at next download or start",
                m.retention_days != dm.retention_days,
            ));
            out.push(row(
                RowId::MaxFileMb,
                "Largest file",
                format!("{} MB", m.max_file_mb),
                "1-2048 MB, not above cache size",
                m.max_file_mb != dm.max_file_mb,
            ));
            out.push(row(
                RowId::InlineImages,
                "Inline images",
                on_off(m.inline_images),
                "thumbnails; NEXT START for terminal graphics",
                m.inline_images != dm.inline_images,
            ));
            out.push(row(
                RowId::ImageProtocol,
                "Image protocol",
                name_of(&IMAGE_PROTOCOLS, m.image_protocol),
                "NEXT START: graphics are detected at start",
                m.image_protocol != dm.image_protocol,
            ));
        }
        Section::Teams => {
            out.push(row(
                RowId::FilesRead,
                "File downloads",
                on_off(s.teams.files_read),
                "needs a new sign-in: /signout then /signin",
                s.teams.files_read != d.teams.files_read,
            ));
            out.push(row(
                RowId::FilesWrite,
                "Sending files",
                on_off(s.teams.files_write),
                "needs a new sign-in: /signout then /signin",
                s.teams.files_write != d.teams.files_write,
            ));
            out.push(row(
                RowId::TeamsPhotos,
                "Profile photos",
                on_off(s.teams.photos),
                "needs a new sign-in: /signout then /signin",
                s.teams.photos != d.teams.photos,
            ));
        }
        Section::WhatsApp => {
            out.push(row(
                RowId::SendReadReceipts,
                "Send read receipts",
                on_off(s.whatsapp.send_read_receipts),
                "on: others see when you read",
                s.whatsapp.send_read_receipts != d.whatsapp.send_read_receipts,
            ));
            out.push(row(
                RowId::SendTyping,
                "Send typing",
                on_off(s.whatsapp.send_typing),
                "on: others see when you type",
                s.whatsapp.send_typing != d.whatsapp.send_typing,
            ));
            out.push(row(
                RowId::DownloadImages,
                "Download images",
                on_off(s.whatsapp.download_images),
                "on: pictures load when a chat opens",
                s.whatsapp.download_images != d.whatsapp.download_images,
            ));
        }
        Section::Keys => key_rows(&mut out, s, keys),
        Section::About => {
            for (label, value) in about {
                out.push(row(RowId::Info(label), label, value.clone(), "", false));
            }
            for (i, p) in problems.iter().enumerate() {
                out.push(row(RowId::Problem(i), "", p.clone(), "", false));
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Prev,
    Next,
    Toggle,
    /// Back to the default: the key leaves the file.
    Reset,
    Text(String),
}

type Done = Result<(Settings, Vec<SettingEdit>), String>;

fn one(s: Settings, edit: SettingEdit) -> Done {
    Ok((s, vec![edit]))
}

/// Steps through `options` (wrapping) for a cycle change; `None` for Reset and Text.
fn cycle<T: Copy + PartialEq>(options: &[(&'static str, T)], cur: T, c: &Change) -> Option<T> {
    let i = options.iter().position(|(_, v)| *v == cur).unwrap_or(0);
    let n = options.len();
    let j = match c {
        Change::Prev => (i + n - 1) % n,
        Change::Next | Change::Toggle => (i + 1) % n,
        _ => return None,
    };
    Some(options[j].1)
}

/// A choice setting: cycle, or reset to `default`.
fn choice<T: Copy + PartialEq>(
    options: &[(&'static str, T)],
    cur: T,
    default: T,
    c: &Change,
) -> Result<T, String> {
    match c {
        Change::Reset => Ok(default),
        Change::Text(_) => Err("use Left and Right to choose".to_string()),
        _ => Ok(cycle(options, cur, c).unwrap_or(cur)),
    }
}

fn flip(cur: bool, default: bool, c: &Change) -> Result<bool, String> {
    match c {
        Change::Reset => Ok(default),
        Change::Text(_) => Err("use Left and Right to switch".to_string()),
        _ => Ok(!cur),
    }
}

/// A whole number in `min..=max`: stepped by `step` (clamped to the range) or typed.
fn number(cur: u64, step: u64, min: u64, max: u64, c: &Change, msg: &str) -> Result<u64, String> {
    match c {
        Change::Prev => Ok(cur.saturating_sub(step).clamp(min, max)),
        Change::Next | Change::Toggle => Ok(cur.saturating_add(step).clamp(min, max)),
        Change::Text(t) => {
            let t = t.trim();
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return Err(msg.to_string());
            }
            t.parse::<u64>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .ok_or_else(|| msg.to_string())
        }
        Change::Reset => unreachable!("reset is handled before the number"),
    }
}

fn parse_width(t: &str) -> Result<Option<SidebarWidth>, String> {
    let t = t.trim().to_ascii_lowercase();
    if t == "auto" {
        return Ok(None);
    }
    let bad = || MSG_WIDTH.to_string();
    if let Some(p) = t.strip_suffix('%') {
        let p: u8 = p.trim().parse().map_err(|_| bad())?;
        return (10..=70)
            .contains(&p)
            .then_some(Some(SidebarWidth::Percent(p)))
            .ok_or_else(bad);
    }
    let n: u16 = t.parse().map_err(|_| bad())?;
    (16..=120)
        .contains(&n)
        .then_some(Some(SidebarWidth::Columns(n)))
        .ok_or_else(bad)
}

fn set_or_remove(table: &str, key: &str, v: Option<SettingValue>) -> SettingEdit {
    match v {
        Some(v) => SettingEdit::set(table, key, v),
        None => SettingEdit::remove(table, key),
    }
}

/// Applies one change to `s`: the new settings plus the file edits, or the inline error text.
/// Pure; `Reset` removes the key from the file so the default applies.
pub(crate) fn change(s: &Settings, id: RowId, c: Change) -> Done {
    let d = Settings::default();
    let reset = c == Change::Reset;
    let mut n = s.clone();
    match id {
        RowId::Info(_) | RowId::Problem(_) | RowId::KeyHeading(_) => {
            Err("this line is read-only".to_string())
        }
        RowId::KeyAction(..) => Err("Enter replaces a key, a adds one, d deletes one".to_string()),
        RowId::ResetAllKeys => Err("press Enter to reset every key".to_string()),
        RowId::ThemeName => {
            let name = match &c {
                Change::Reset => d.theme.name.clone(),
                Change::Text(t) => THEMES
                    .iter()
                    .find(|n| n.eq_ignore_ascii_case(t.trim()))
                    .map(|n| n.to_string())
                    .ok_or_else(|| format!("unknown theme: use {}", THEMES.join(", ")))?,
                _ => {
                    let i = THEMES
                        .iter()
                        .position(|t| t.eq_ignore_ascii_case(&s.theme.name))
                        .unwrap_or(0);
                    let k = THEMES.len();
                    let j = if c == Change::Prev { i + k - 1 } else { i + 1 } % k;
                    THEMES[j].to_string()
                }
            };
            n.theme.name = name.clone();
            let edit = if reset {
                SettingEdit::remove("theme", "name")
            } else {
                SettingEdit::set("theme", "name", SettingValue::Str(name))
            };
            one(n, edit)
        }
        RowId::Palette => {
            let cur = s
                .theme
                .overrides
                .get("palette")
                .map(|v| v.trim().to_ascii_lowercase());
            let name = match &c {
                Change::Reset => None,
                Change::Text(t) => Some(
                    PALETTES
                        .iter()
                        .find(|n| n.eq_ignore_ascii_case(t.trim()))
                        .ok_or_else(|| format!("unknown palette: use {}", PALETTES.join(", ")))?
                        .to_string(),
                ),
                _ => {
                    let i = cur
                        .as_deref()
                        .and_then(|v| PALETTES.iter().position(|p| *p == v))
                        .unwrap_or(0);
                    let k = PALETTES.len();
                    Some(
                        PALETTES[if c == Change::Prev { i + k - 1 } else { i + 1 } % k].to_string(),
                    )
                }
            };
            // `lines` is the default look: saved as no key at all.
            let name = name.filter(|n| n != "lines");
            match &name {
                Some(v) => n.theme.overrides.insert("palette".to_string(), v.clone()),
                None => n.theme.overrides.remove("palette"),
            };
            one(
                n,
                set_or_remove("theme", "palette", name.map(SettingValue::Str)),
            )
        }
        RowId::Mark(key) => {
            let default = Marks::default().get(key).unwrap_or(' ');
            let cur = s
                .theme
                .overrides
                .get(key)
                .and_then(|v| mark_char(v).ok())
                .unwrap_or(default);
            let mark = match &c {
                Change::Reset => default,
                // An empty box is the default; a space is a space (no mark).
                Change::Text(t) if t.is_empty() || t.trim().eq_ignore_ascii_case("default") => {
                    default
                }
                Change::Text(t) => mark_char(t)?,
                _ => {
                    let k = MARK_CHOICES.len();
                    let i = MARK_CHOICES.iter().position(|m| *m == cur).unwrap_or(0);
                    MARK_CHOICES[if c == Change::Prev { i + k - 1 } else { i + 1 } % k]
                }
            };
            // The default mark is saved as no key at all.
            let value = (mark != default).then(|| mark.to_string());
            match &value {
                Some(v) => n.theme.overrides.insert(key.to_string(), v.clone()),
                None => n.theme.overrides.remove(key),
            };
            one(n, set_or_remove("theme", key, value.map(SettingValue::Str)))
        }
        RowId::Outline(key) => {
            let cur = s
                .theme
                .overrides
                .get(key)
                .map_or("default".to_string(), |v| v.trim().to_ascii_lowercase());
            let value = match &c {
                Change::Reset => None,
                Change::Text(t) => {
                    let t = t.trim().to_ascii_lowercase();
                    if t.is_empty() || t == "default" {
                        None
                    } else {
                        Outline::from_name(&t).ok_or_else(|| {
                            format!("unknown outline: use {}", OUTLINE_CHOICES.join(", "))
                        })?;
                        Some(t)
                    }
                }
                _ => {
                    let k = OUTLINE_CHOICES.len();
                    let i = OUTLINE_CHOICES.iter().position(|o| *o == cur).unwrap_or(0);
                    let j = if c == Change::Prev { i + k - 1 } else { i + 1 } % k;
                    (j != 0).then(|| OUTLINE_CHOICES[j].to_string())
                }
            };
            match &value {
                Some(v) => n.theme.overrides.insert(key.to_string(), v.clone()),
                None => n.theme.overrides.remove(key),
            };
            one(n, set_or_remove("theme", key, value.map(SettingValue::Str)))
        }
        RowId::Role(role) => {
            let value: Option<String> = match &c {
                Change::Reset => None,
                Change::Text(t) => {
                    let t = t.trim();
                    if t.is_empty() || t.eq_ignore_ascii_case("default") {
                        None
                    } else {
                        if is_background(role) {
                            background(t)?;
                        } else {
                            parse_colour(t)?;
                        }
                        Some(t.to_string())
                    }
                }
                _ => {
                    let cur = s
                        .theme
                        .overrides
                        .get(role)
                        .map_or("default".to_string(), |v| v.trim().to_ascii_lowercase());
                    let choices = colour_choices(role);
                    let k = choices.len();
                    let i = choices.iter().position(|n| *n == cur).unwrap_or(0);
                    let j = if c == Change::Prev { i + k - 1 } else { i + 1 } % k;
                    (j != 0).then(|| choices[j].to_string())
                }
            };
            match &value {
                Some(v) => n.theme.overrides.insert(role.to_string(), v.clone()),
                None => n.theme.overrides.remove(role),
            };
            let edit = set_or_remove("theme", role, value.map(SettingValue::Str));
            one(n, edit)
        }
        RowId::SidebarWidth => {
            let w = match &c {
                Change::Reset => None,
                Change::Text(t) => parse_width(t)?,
                _ => {
                    let k = SIDEBAR_STEPS.len();
                    let i = SIDEBAR_STEPS
                        .iter()
                        .position(|w| *w == s.layout.sidebar_width)
                        .unwrap_or(0);
                    SIDEBAR_STEPS[if c == Change::Prev { i + k - 1 } else { i + 1 } % k]
                }
            };
            n.layout.sidebar_width = w;
            let edit = set_or_remove("layout", "sidebar_width", w.map(SidebarWidth::to_setting));
            one(n, edit)
        }
        RowId::TimeFormat => {
            let v = choice(
                &TIME_FORMATS,
                s.layout.time_format,
                TimeFormat::default(),
                &c,
            )?;
            n.layout.time_format = v;
            one(n, mk("layout", "time_format", reset, v.to_setting()))
        }
        RowId::DateOrder => {
            let v = choice(&DATE_ORDERS, s.layout.date_order, DateOrder::default(), &c)?;
            n.layout.date_order = v;
            one(n, mk("layout", "date_order", reset, v.to_setting()))
        }
        RowId::Density => {
            let v = choice(&DENSITIES, s.layout.density, Density::default(), &c)?;
            n.layout.density = v;
            one(n, mk("layout", "density", reset, v.to_setting()))
        }
        RowId::Avatars => {
            let v = choice(&AVATARS, s.layout.avatars, Avatars::default(), &c)?;
            n.layout.avatars = v;
            one(n, mk("layout", "avatars", reset, v.to_setting()))
        }
        RowId::Photos => {
            let v = flip(s.layout.photos, d.layout.photos, &c)?;
            n.layout.photos = v;
            one(n, mk("layout", "photos", reset, SettingValue::Bool(v)))
        }
        RowId::ShowPreviews => {
            let v = flip(s.layout.show_previews, d.layout.show_previews, &c)?;
            n.layout.show_previews = v;
            one(
                n,
                mk("layout", "show_previews", reset, SettingValue::Bool(v)),
            )
        }
        RowId::NotifEnabled => {
            let v = flip(s.notifications.enabled, d.notifications.enabled, &c)?;
            n.notifications.enabled = v;
            one(
                n,
                mk("notifications", "enabled", reset, SettingValue::Bool(v)),
            )
        }
        RowId::NotifPreview => {
            let v = choice(
                &NOTIFY_PREVIEWS,
                s.notifications.preview,
                NotifyPreview::default(),
                &c,
            )?;
            n.notifications.preview = v;
            one(n, mk("notifications", "preview", reset, v.to_setting()))
        }
        RowId::NotifSound => {
            let v = flip(s.notifications.sound, d.notifications.sound, &c)?;
            n.notifications.sound = v;
            one(
                n,
                mk("notifications", "sound", reset, SettingValue::Bool(v)),
            )
        }
        RowId::CacheMaxMb => {
            let v = if reset {
                d.media.cache_max_mb
            } else {
                number(s.media.cache_max_mb, 50, 1, 100_000, &c, MSG_CACHE)?
            };
            if v < s.media.max_file_mb {
                return Err(format!(
                    "must not be below the largest file ({} MB)",
                    s.media.max_file_mb
                ));
            }
            n.media.cache_max_mb = v;
            one(
                n,
                mk("media", "cache_max_mb", reset, SettingValue::Int(v as i64)),
            )
        }
        RowId::RetentionDays => {
            let v = if reset {
                u64::from(d.media.retention_days)
            } else {
                number(u64::from(s.media.retention_days), 1, 1, 3650, &c, MSG_DAYS)?
            };
            n.media.retention_days = v as u32;
            one(
                n,
                mk(
                    "media",
                    "retention_days",
                    reset,
                    SettingValue::Int(v as i64),
                ),
            )
        }
        RowId::MaxFileMb => {
            let v = if reset {
                d.media.max_file_mb
            } else {
                number(s.media.max_file_mb, 5, 1, 2048, &c, MSG_FILE)?
            };
            if v > s.media.cache_max_mb {
                return Err(format!(
                    "must not be above the cache size ({} MB)",
                    s.media.cache_max_mb
                ));
            }
            n.media.max_file_mb = v;
            one(
                n,
                mk("media", "max_file_mb", reset, SettingValue::Int(v as i64)),
            )
        }
        RowId::InlineImages => {
            let v = flip(s.media.inline_images, d.media.inline_images, &c)?;
            n.media.inline_images = v;
            one(
                n,
                mk("media", "inline_images", reset, SettingValue::Bool(v)),
            )
        }
        RowId::ImageProtocol => {
            let v = choice(
                &IMAGE_PROTOCOLS,
                s.media.image_protocol,
                ImageProtocol::default(),
                &c,
            )?;
            n.media.image_protocol = v;
            one(n, mk("media", "image_protocol", reset, v.to_setting()))
        }
        RowId::FilesRead => {
            let v = flip(s.teams.files_read, d.teams.files_read, &c)?;
            n.teams.files_read = v;
            one(n, mk("teams", "files_read", reset, SettingValue::Bool(v)))
        }
        RowId::FilesWrite => {
            let v = flip(s.teams.files_write, d.teams.files_write, &c)?;
            n.teams.files_write = v;
            one(n, mk("teams", "files_write", reset, SettingValue::Bool(v)))
        }
        RowId::TeamsPhotos => {
            let v = flip(s.teams.photos, d.teams.photos, &c)?;
            n.teams.photos = v;
            one(n, mk("teams", "photos", reset, SettingValue::Bool(v)))
        }
        RowId::SendReadReceipts => {
            let v = flip(
                s.whatsapp.send_read_receipts,
                d.whatsapp.send_read_receipts,
                &c,
            )?;
            n.whatsapp.send_read_receipts = v;
            one(
                n,
                mk(
                    "whatsapp",
                    "send_read_receipts",
                    reset,
                    SettingValue::Bool(v),
                ),
            )
        }
        RowId::SendTyping => {
            let v = flip(s.whatsapp.send_typing, d.whatsapp.send_typing, &c)?;
            n.whatsapp.send_typing = v;
            one(
                n,
                mk("whatsapp", "send_typing", reset, SettingValue::Bool(v)),
            )
        }
        RowId::DownloadImages => {
            let v = flip(s.whatsapp.download_images, d.whatsapp.download_images, &c)?;
            n.whatsapp.download_images = v;
            one(
                n,
                mk("whatsapp", "download_images", reset, SettingValue::Bool(v)),
            )
        }
    }
}

/// Reset removes the key (the default applies); anything else writes the value.
fn mk(table: &str, key: &str, reset: bool, v: SettingValue) -> SettingEdit {
    if reset {
        SettingEdit::remove(table, key)
    } else {
        SettingEdit::set(table, key, v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &Settings, id: RowId, c: Change) -> (Settings, Vec<SettingEdit>) {
        change(s, id, c).expect("change ok")
    }

    fn err(s: &Settings, id: RowId, c: Change) -> String {
        change(s, id, c).expect_err("change refused")
    }

    fn all_ids() -> Vec<RowId> {
        let s = Settings::default();
        let mut ids = Vec::new();
        for sec in Section::ALL {
            for r in rows(sec, &s, &Keymap::default(), &[], &[]) {
                // Key rows change by capture, not by `change`: their own tests below.
                if r.selectable() && !matches!(r.id, RowId::KeyAction(..) | RowId::ResetAllKeys) {
                    ids.push(r.id);
                }
            }
        }
        ids
    }

    #[test]
    fn every_selectable_row_survives_every_change() {
        let ids = all_ids();
        assert_eq!(
            ids.len(),
            18 + 3 + 1 + 5 + 6 + ROLES.len() + MARK_KEYS.len()
        );
        for id in ids {
            for c in [Change::Prev, Change::Next, Change::Toggle, Change::Reset] {
                let (_, edits) = run(&Settings::default(), id, c.clone());
                assert_eq!(edits.len(), 1, "{id:?} {c:?}");
            }
        }
    }

    #[test]
    fn cycles_wrap_both_ways() {
        let s = Settings::default();
        let (n, _) = run(&s, RowId::TimeFormat, Change::Prev);
        assert_eq!(n.layout.time_format, TimeFormat::H12);
        let (n, _) = run(&n, RowId::TimeFormat, Change::Next);
        assert_eq!(n.layout.time_format, TimeFormat::H24);
        let (n, _) = run(&s, RowId::ThemeName, Change::Prev);
        assert_eq!(n.theme.name, "dark");
        let (n, e) = run(&n, RowId::ThemeName, Change::Next);
        assert_eq!(n.theme.name, "default");
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "theme",
                "name",
                SettingValue::str("default")
            )]
        );
        let (n, _) = run(&s, RowId::SidebarWidth, Change::Prev);
        assert_eq!(n.layout.sidebar_width, Some(SidebarWidth::Percent(40)));
        let (n, _) = run(&n, RowId::SidebarWidth, Change::Next);
        assert_eq!(n.layout.sidebar_width, None);
        let (n, _) = run(&s, RowId::ImageProtocol, Change::Prev);
        assert_eq!(n.media.image_protocol, ImageProtocol::Off);
    }

    #[test]
    fn toggles_flip_and_write_a_bool() {
        let s = Settings::default();
        let (n, e) = run(&s, RowId::SendTyping, Change::Toggle);
        assert!(!n.whatsapp.send_typing);
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "whatsapp",
                "send_typing",
                SettingValue::Bool(false)
            )]
        );
        let (n, _) = run(&n, RowId::SendTyping, Change::Next);
        assert!(n.whatsapp.send_typing);
        let (n, _) = run(&s, RowId::DownloadImages, Change::Toggle);
        assert!(!n.whatsapp.download_images);
        let (n, _) = run(&s, RowId::NotifEnabled, Change::Toggle);
        assert!(!n.notifications.enabled);
    }

    #[test]
    fn numbers_step_clamp_and_report_exact_range_errors() {
        let s = Settings::default();
        let (n, _) = run(&s, RowId::CacheMaxMb, Change::Next);
        assert_eq!(n.media.cache_max_mb, 250);
        let (n, _) = run(&s, RowId::RetentionDays, Change::Prev);
        assert_eq!(n.media.retention_days, 29);
        assert_eq!(
            err(&s, RowId::CacheMaxMb, Change::Text("0".into())),
            MSG_CACHE
        );
        assert_eq!(
            err(&s, RowId::CacheMaxMb, Change::Text("100001".into())),
            MSG_CACHE
        );
        assert_eq!(
            err(&s, RowId::CacheMaxMb, Change::Text("12x".into())),
            MSG_CACHE
        );
        assert_eq!(
            err(&s, RowId::CacheMaxMb, Change::Text("".into())),
            MSG_CACHE
        );
        assert_eq!(
            err(&s, RowId::RetentionDays, Change::Text("3651".into())),
            MSG_DAYS
        );
        assert_eq!(
            err(&s, RowId::MaxFileMb, Change::Text("2049".into())),
            MSG_FILE
        );
        let (n, e) = run(&s, RowId::CacheMaxMb, Change::Text(" 500 ".into()));
        assert_eq!(n.media.cache_max_mb, 500);
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "media",
                "cache_max_mb",
                SettingValue::Int(500)
            )]
        );
    }

    #[test]
    fn largest_file_may_not_pass_the_cache_and_the_cache_may_not_drop_under_it() {
        let s = Settings::default();
        assert_eq!(
            err(&s, RowId::MaxFileMb, Change::Text("300".into())),
            "must not be above the cache size (200 MB)"
        );
        let (n, _) = run(&s, RowId::MaxFileMb, Change::Text("200".into()));
        assert_eq!(n.media.max_file_mb, 200);
        assert_eq!(
            err(&n, RowId::CacheMaxMb, Change::Text("100".into())),
            "must not be below the largest file (200 MB)"
        );
        assert_eq!(
            err(&n, RowId::CacheMaxMb, Change::Prev),
            "must not be below the largest file (200 MB)"
        );
    }

    #[test]
    fn sidebar_width_accepts_auto_columns_and_percent() {
        let s = Settings::default();
        let w = |t: &str| change(&s, RowId::SidebarWidth, Change::Text(t.into()));
        assert_eq!(w("auto").unwrap().0.layout.sidebar_width, None);
        assert_eq!(
            w("30").unwrap().0.layout.sidebar_width,
            Some(SidebarWidth::Columns(30))
        );
        assert_eq!(
            w("30%").unwrap().0.layout.sidebar_width,
            Some(SidebarWidth::Percent(30))
        );
        assert_eq!(
            w("30%").unwrap().1,
            vec![SettingEdit::set(
                "layout",
                "sidebar_width",
                SettingValue::str("30%")
            )]
        );
        assert_eq!(
            w("30").unwrap().1,
            vec![SettingEdit::set(
                "layout",
                "sidebar_width",
                SettingValue::Int(30)
            )]
        );
        assert_eq!(
            w("auto").unwrap().1,
            vec![SettingEdit::remove("layout", "sidebar_width")]
        );
        for bad in ["15", "121", "9%", "71%", "wide", ""] {
            assert_eq!(w(bad).unwrap_err(), MSG_WIDTH, "{bad}");
        }
    }

    #[test]
    fn message_marks_take_one_narrow_character_cycle_and_reset() {
        let s = Settings::default();
        let id = RowId::Mark("mark_sent");
        let (n, e) = run(&s, id, Change::Text(" ◌ ".into()));
        assert_eq!(n.theme.overrides["mark_sent"], "◌");
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "theme",
                "mark_sent",
                SettingValue::str("◌")
            )]
        );
        // Two characters, an emoji (two columns) and a control character are refused.
        for bad in ["ab", "😀", "\u{7}"] {
            let msg = err(&s, id, Change::Text(bad.into()));
            assert!(
                msg.contains("one character one column wide"),
                "{bad}: {msg}"
            );
        }
        // Right from the default ○ goes to ●; the default itself is saved as no key.
        let (n, e) = run(&s, id, Change::Next);
        assert_eq!(n.theme.overrides["mark_sent"], "●");
        assert_eq!(e[0].key, "mark_sent");
        let (n, e) = run(&n, id, Change::Prev);
        assert!(!n.theme.overrides.contains_key("mark_sent"));
        assert_eq!(e, vec![SettingEdit::remove("theme", "mark_sent")]);
        // A space is "no mark", shown as words so the row does not look empty; an empty box is
        // still the default.
        let (n, e) = run(&s, id, Change::Text(" ".into()));
        assert_eq!(n.theme.overrides["mark_sent"], " ");
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "theme",
                "mark_sent",
                SettingValue::str(" ")
            )]
        );
        let shown = rows(Section::Appearance, &n, &Keymap::default(), &[], &[])
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(shown.value, "none (space)");
        let (n, e) = run(&n, id, Change::Text(String::new()));
        assert!(!n.theme.overrides.contains_key("mark_sent"));
        assert_eq!(e, vec![SettingEdit::remove("theme", "mark_sent")]);
        let (n, _) = run(&s, id, Change::Text("*".into()));
        let (n, e) = run(&n, id, Change::Reset);
        assert!(!n.theme.overrides.contains_key("mark_sent"));
        assert_eq!(e, vec![SettingEdit::remove("theme", "mark_sent")]);
    }

    #[test]
    fn colours_take_names_hex_and_default() {
        let s = Settings::default();
        let (n, e) = run(&s, RowId::Role("accent"), Change::Text("#12ab3F".into()));
        assert_eq!(n.theme.overrides["accent"], "#12ab3F");
        assert_eq!(
            e,
            vec![SettingEdit::set(
                "theme",
                "accent",
                SettingValue::str("#12ab3F")
            )]
        );
        let bad = err(&s, RowId::Role("accent"), Change::Text("#12".into()));
        assert!(bad.contains("is not a colour"), "{bad}");
        let (n, e) = run(&n, RowId::Role("accent"), Change::Text("default".into()));
        assert!(n.theme.overrides.is_empty());
        assert_eq!(e, vec![SettingEdit::remove("theme", "accent")]);
        let (n, _) = run(&s, RowId::Role("error"), Change::Next);
        assert_eq!(n.theme.overrides["error"], "black");
        let (n, e) = run(&n, RowId::Role("error"), Change::Prev);
        assert!(n.theme.overrides.is_empty(), "back to default removes it");
        assert_eq!(e, vec![SettingEdit::remove("theme", "error")]);
        let (n, _) = run(&s, RowId::Role("error"), Change::Prev);
        assert_eq!(n.theme.overrides["error"], "white");
    }

    #[test]
    fn unknown_theme_name_is_refused_by_name() {
        let s = Settings::default();
        assert_eq!(
            err(&s, RowId::ThemeName, Change::Text("neon".into())),
            "unknown theme: use default, high-contrast, light, dark"
        );
        let (n, _) = run(&s, RowId::ThemeName, Change::Text("LIGHT".into()));
        assert_eq!(n.theme.name, "light");
    }

    #[test]
    fn reset_removes_the_key_and_restores_the_default() {
        let mut s = Settings::default();
        s.layout.density = Density::Compact;
        s.media.cache_max_mb = 900;
        s.theme.overrides.insert("dim".into(), "red".into());
        let (n, e) = run(&s, RowId::Density, Change::Reset);
        assert_eq!(n.layout.density, Density::Cozy);
        assert_eq!(e, vec![SettingEdit::remove("layout", "density")]);
        let (n, e) = run(&s, RowId::CacheMaxMb, Change::Reset);
        assert_eq!(n.media.cache_max_mb, 200);
        assert_eq!(e, vec![SettingEdit::remove("media", "cache_max_mb")]);
        let (n, e) = run(&s, RowId::Role("dim"), Change::Reset);
        assert!(n.theme.overrides.is_empty());
        assert_eq!(e, vec![SettingEdit::remove("theme", "dim")]);
    }

    #[test]
    fn read_only_rows_and_wrong_input_are_refused() {
        let s = Settings::default();
        assert!(change(&s, RowId::Info("keys"), Change::Next).is_err());
        assert!(change(&s, RowId::Problem(0), Change::Reset).is_err());
        assert!(change(&s, RowId::Density, Change::Text("x".into())).is_err());
        assert!(change(&s, RowId::SendTyping, Change::Text("x".into())).is_err());
    }

    #[test]
    fn rows_say_what_applies_later() {
        let s = Settings::default();
        let hint = |sec, id| {
            rows(sec, &s, &Keymap::default(), &[], &[])
                .into_iter()
                .find(|r| r.id == id)
                .unwrap()
                .hint
        };
        assert!(hint(Section::Media, RowId::ImageProtocol).starts_with("NEXT START"));
        assert!(hint(Section::Teams, RowId::FilesRead).contains("sign-in"));
        assert!(hint(Section::Teams, RowId::FilesWrite).contains("sign-in"));
        assert!(hint(Section::Teams, RowId::TeamsPhotos).contains("sign-in"));
    }

    #[test]
    fn custom_marks_values_the_file_overrides() {
        let mut s = Settings::default();
        s.layout.density = Density::Compact;
        let r = rows(Section::Layout, &s, &Keymap::default(), &[], &[]);
        let custom: Vec<_> = r.iter().filter(|r| r.custom).map(|r| r.id).collect();
        assert_eq!(custom, vec![RowId::Density]);
    }

    #[test]
    fn problems_header_tops_every_page_but_about_lists_them() {
        let s = Settings::default();
        let p = vec!["one".to_string(), "two".to_string()];
        let r = rows(Section::Layout, &s, &Keymap::default(), &[], &p);
        assert_eq!(r[0].value, "SETTINGS FILE PROBLEMS: 2 - see About");
        assert!(r[0].is_error() && !r[0].selectable());
        let about = [("Version", "chatcli 0".to_string())];
        let r = rows(Section::About, &s, &Keymap::default(), &about, &p);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].value, "one");
        assert!(r[1].is_error());
        assert!(
            rows(Section::Layout, &s, &Keymap::default(), &[], &[])[0].id
                != RowId::Info("problems")
        );
    }

    #[test]
    fn edit_start_is_the_current_value() {
        let mut s = Settings::default();
        assert_eq!(edit_start(&s, RowId::CacheMaxMb), "200");
        assert_eq!(edit_start(&s, RowId::SidebarWidth), "auto");
        assert_eq!(edit_start(&s, RowId::Role("dim")), "");
        s.theme.overrides.insert("dim".into(), "#aabbcc".into());
        assert_eq!(edit_start(&s, RowId::Role("dim")), "#aabbcc");
    }

    #[test]
    fn sections_step_and_wrap() {
        assert_eq!(Section::Appearance.step(-1), Section::About);
        assert_eq!(Section::About.step(1), Section::Appearance);
        assert_eq!(Section::Layout.step(1), Section::Notifications);
    }

    #[test]
    fn key_rows_mark_what_the_user_set() {
        let mut s = Settings::default();
        let t = s.keys.bindings.entry("list".into()).or_default();
        t.insert("f5".into(), "refresh".into());
        t.insert("p".into(), "unbound".into());
        let keys = Keymap::build(&s.keys).0;
        let r = rows(Section::Keys, &s, &keys, &[], &[]);
        let find = |a: Action| {
            r.iter()
                .find(|r| r.id == RowId::KeyAction(Context::List, a))
                .unwrap()
        };
        assert!(find(Action::Refresh).custom);
        assert_eq!(find(Action::Refresh).value, "F5, r *");
        assert!(find(Action::Pin).custom, "an unbound preset key counts");
        assert_eq!(find(Action::Pin).value, "- *");
        assert!(!find(Action::Mute).custom);
        assert_eq!(find(Action::Mute).value, "m");
    }

    #[test]
    fn key_rows_are_changed_by_capture_not_by_change() {
        let s = Settings::default();
        let id = RowId::KeyAction(Context::List, Action::Refresh);
        assert_eq!(
            err(&s, id, Change::Next),
            "Enter replaces a key, a adds one, d deletes one"
        );
        assert!(change(&s, RowId::KeyHeading(Context::List), Change::Next).is_err());
        assert!(change(&s, RowId::ResetAllKeys, Change::Next).is_err());
    }

    #[test]
    fn capture_line_names_the_action_or_the_swap() {
        let c = PageEdit::Capture {
            ctx: Context::List,
            action: Action::Refresh,
        };
        assert_eq!(
            capture_line(Some(&c)).as_deref(),
            Some("Press the new key for Refresh (any key is taken, Esc too)")
        );
        let c = PageEdit::Conflict {
            ctx: Context::List,
            action: Action::Refresh,
            key: parse_key("ctrl+n").unwrap(),
            other: Action::Pin,
        };
        assert_eq!(
            capture_line(Some(&c)).as_deref(),
            Some("Ctrl+N is already Pin. Enter: Pin gets Refresh's old key (or none), Esc cancels")
        );
        assert_eq!(
            capture_line(Some(&PageEdit::Text(TextInput::default()))),
            None
        );
        assert_eq!(capture_line(None), None);
    }
}
