# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `settings.toml`: one hand-edited file for key bindings, theme, layout, notifications, media limits, optional Teams scopes and WhatsApp privacy options. A broken section falls back to its defaults with one notice naming the line and key. `chatcli settings path` and `chatcli settings check`.
- Keys are named actions in a keymap. Rebind them in `settings.toml` (`[keys.<context>]`), with a notice for every mistake and a suggested name. `?` opens a help screen generated from the active keys.
- Vim key preset (`[keys] preset = "vim"`, `--keys vim`, or `/keys vim` live): Normal, Insert and Select modes, `gg` `gt` `gT`, and a `:` command box.
- Message selection (`Ctrl+Up`, `v` in vim): reply with a quote, react, edit and delete your own messages, open links, open attachments.
- Replies with a quote on Teams and WhatsApp. Teams needs the new `ChatMessage.Send` permission: log out of Teams and sign in again after upgrading.
- Reactions on Teams and WhatsApp, shown as counts under each message, with an emoji picker (`Ctrl+E`, Recent first) and `:shortcode` completion.
- Edit and delete your own messages on Teams and WhatsApp. Edits and deletes from other people update in place.
- `@mention` completion in group chats on both networks. Chats where you were mentioned show `@` after the unread count.
- Message box editing: cursor movement, word jumps, whole-emoji delete, multi-line drafts (`Alt+Enter`, `Shift+Enter`, `Ctrl+J`), safe paste that never sends, and a length check before sending.
- Media: attachments show as chips; `o` downloads on demand into a capped cache and opens the file in the default app; `v` shows image thumbnails with half-block characters, or Sixel, Kitty or iTerm2 graphics where the terminal supports them. `chatcli media clear`.
- Send pictures and files on both networks: `Ctrl+O` path prompt with Tab completion, dropped paths, and clipboard pictures (`Alt+V`). Pictures are re-encoded, which drops EXIF data. Teams files go through your OneDrive and need the optional `files_write` setting.
- Optional Teams file downloads (`[teams] files_read`).
- Open links from messages in the browser (`u`), after an `Open <host>? y/n` line that shows the real host.
- Colour themes with named roles (`[theme]`, `high-contrast`, `light`, `dark`) and `NO_COLOR` support.
- Layout options: sidebar width, 12-hour time, date order, compact rows, hidden previews.
- Windows toast notifications for new messages, off by default for message text (`[notifications] preview`), folded per chat.
- Scroll past the top to load older messages on Teams (one page per scroll) and WhatsApp (asks your phone).
- `n` and `N` jump to the next and previous unread chat; `M` marks every chat read.
- Pin, mute and hide chats (`p`, `m`, `x`, `H`). WhatsApp syncs them with the phone.
- Local full-text search across both networks (`Ctrl+F`, `chatcli search <text>`).
- WhatsApp typing indicator, and your own typing only with `[whatsapp] send_typing`.
- `chatcli doctor [--online] [--json]` checks the setup and prints fixes.
- `chatcli relay url <url>` and `chatcli relay setup <url> --rotate`.
- `chatcli --help`.
- `--demo` shows every feature above on fake data, with its media in a temporary folder.
- Portability groundwork for Linux and macOS: data folder, file lock, browser launch, keychains, best-effort CI jobs.
- Linux: `CHATCLI_SECRET_STORE=file` keeps secrets in `secrets.json` in the data folder (mode 0600, not encrypted) for systems with no Secret Service.
- Winget and Scoop manifests (not yet submitted), a code-signing write-up, a memory benchmark script, a relay end-to-end test, WhatsApp fake-transport tests, and release notes built from this file.
- Sidebar preview: moving through the chat list (arrow keys, j/k, page keys, a click) shows each chat in the message pane at once. Enter replies, Esc goes back. A previewed chat counts as read after 2 s on screen, never while its pane is hidden.
- `/` command bar (chat list, sign-in panel, message selection): lists every command and your chats. Commands also work as `/command` in the message box (`//` sends a message that starts with a slash) and as `:command` in the vim box: `/help` `/settings` `/keys` `/theme` `/layout` `/notifications` `/search` `/refresh` `/signin` `/signout` `/about` `/tour` `/quit`.
- Settings screen (`F2`, `/settings`): theme and the 14 colour roles with a live preview, layout, notifications, media limits, Teams file permissions and WhatsApp privacy options. Every change applies at once and is saved to `settings.toml`, one key at a time, with comments kept. A `settings.toml` that does not parse is never written: the change applies for the session and `NOT SAVED` says why.
- Palettes (`/settings` > Appearance > Palette): `lines` (bordered panels, the default), `blocks` (no borders, each panel its own background shade), `minimal` (a top rule only) and `rounded`. Each panel (sidebar, messages, message box, popups) can override its outline (`none`, `rule`, `single`, `rounded`, `double`, `thick`) and fill, and the top bar and footer their fill. `NO_COLOR` turns shaded panels back into lines.
- Group chats show messages Discord style: an avatar on the left (initials in the person's colour), the name in that colour with the time beside it, and the text underneath. Messages from one person within five minutes share one name row, and a blank row separates people. 1:1 chats get the same name rows without avatars. `[layout] avatars = "groups" | "all" | "off"` (Settings > Layout > Avatars); `off` keeps the one-line layout everywhere. A short conversation sits at the bottom of the pane, next to the message box.
- Profile photos over the initials avatars on terminals with Sixel, Kitty or iTerm2 graphics (other terminals, and `NO_COLOR`, keep initials). Teams asks for the new `User.ReadBasic.All` permission: sign in again after upgrading. WhatsApp looks photos up slowly (10 a minute, once per person per run) and signing out deletes them. Photos are cached for 7 days. `[layout] photos = false` (Settings > Layout > Photos) and `[teams] photos = false` (Settings > Teams > Profile photos) turn them off.
- Key bindings page (`/keys`): change any key by pressing the new one, swap on a clash, remove, reset one or all, and switch the key preset. Saved to `[keys]` in `settings.toml`.
- Guided tour: `chatcli --demo --tour` (or `/tour` in the demo) plays 26 captioned steps over the demo data. `Space` pauses, `n` skips a step, `Esc` ends it. The demo and the tour run on a temporary copy of the settings and never write your real `settings.toml`.

### Changed

- `F2` opens the settings screen instead of switching the key preset (use `/keys vim` or `:keys vim`). `/` opens the command bar instead of the plain chat filter; bind a key to `filter_start` to keep the filter. A message that starts with `/` is a command: type `//` to send it as text.
- `settings.toml` is no longer read-only to chatcli: the settings screen writes it (see Added).
- WhatsApp read receipts are now off by default (`[whatsapp] send_read_receipts`). Before, they were always sent.
- An edited WhatsApp message no longer gets an `(edited)` text suffix: the message carries an edited flag. A deleted message has empty text and shows `[message deleted]`.
- Teams chats you hid in Teams are kept in the list as hidden (`H` shows them) instead of being dropped.
- `chatcli relay setup` stops with a message when the secrets already exist, instead of replacing them.
- Key matching is exact on modifiers (`Shift+Up` no longer acts as `Up`).
- Emoji, flags and CJK text are measured, cut and wrapped by grapheme cluster, so they are no longer split.
- `teams ...`, `relay setup`, `relay url` and `media clear` refuse to run while the full client is open, so a logout cannot race a token refresh.
- The store moves to schema version 2 (search index, reactions, attachments, chat flags) in one transaction. Back up `store.db` before the first start of this version if you want to be able to go back.

### Fixed

- No more screen flashes when switching tabs or chats or scrolling: a picture that moves or goes away now has only its own cells redrawn instead of the whole screen being cleared, and every frame is sent as one synchronized update (terminals without support ignore it).
- Running `relay setup` twice no longer silently breaks live Teams with a 401.
- A Teams subscription that needs reauthorization is renewed with one request, never reauthorize plus renew.
- Teams catch-up sees messages that were edited or deleted while chatcli was off.
- Pending sends are kept when history arrives, and a chat is not marked read while it is not on screen.
- The sidebar keeps its wheel scroll when live updates arrive, and a send fails visibly when a backend is gone.
- Linking a different Teams or WhatsApp account no longer shows the previous account's cached chats.
- WhatsApp: messages dated years in the future no longer float a chat to the top of the list. Stored rows are repaired once at start, history sync re-times them from receipts, and a chat with no believable time sinks instead of taking the current time.
- WhatsApp: chats shown as "Unknown contact" get their contact name (saved from the address book, push names and history), and a chat known under both a LID and a phone number is one chat, not two.
- WhatsApp: a revoke or edit only applies when the sender wrote the message. Before, any group member could blank or rewrite anyone's message.
- WhatsApp downloads stop at the size limit while they run, whatever size the sender declared.
- Windows: a paste arrives as key presses, so several keys that arrive together are now one paste (it never sends). AltGr characters (`@`, euro sign, braces) type. `Ctrl+C` and every Ctrl shortcut work with Caps Lock on.
- Cached downloads carry the Windows Mark of the Web (Office opens them in Protected View). Linux and macOS: the data folder is mode 0700.
- `chatcli doctor` and `chatcli search` never upgrade the store. A pasted network path is text and is never probed.
- Teams: an edit keeps the @mentions of the message, and `@Al` no longer matches inside `@Alex Example`.
- Footer: a y/n question is bold, the footer keeps whole items (never `P...`), the newline hint is `Ctrl+J`, and the chat list and message box show `?` help and `Ctrl+Up` select.
- A skipped WhatsApp contact sync is logged as an error instead of "nothing failed".

## [0.1.0] - 2026-11-16

First release. Early: it works day to day, with rough edges.

### Added

- Full-screen terminal UI with one tab per network, a chat sidebar, a message pane and a composer. Mouse support, chat filter, unread counts, per-chat drafts, text status chips (LIVE, NOT LIVE, SIGN-IN NEEDED, SIGNING IN, SCAN QR, CONNECTING, ERROR).
- Microsoft Teams 1:1 and group chats through Microsoft Graph, with sign-in in the browser using PKCE (public client, no client secret).
- Live Teams messages through a self-hosted Cloudflare Worker relay that receives Graph change notifications and forwards ids only, over a WebSocket. Subscription upkeep, missed-notification catch-up and a NOT LIVE chip when live updates stop.
- WhatsApp direct and group chats as a linked device (QR sign-in), using whatsapp-rust.
- Local SQLite store for chats and messages, with secrets in Windows Credential Manager.
- Commands without the full screen: `chatcli teams setup|login|chats|messages|send|logout`, `chatcli relay setup|status`, `chatcli --version`.
- `chatcli --demo`: the full screen on fake data, with no accounts and no network.
- Log file that never holds tokens, secrets or message text, and a single-instance lock.

### Known limits

- Live Teams updates through the relay are the newest and least tested part. Without a relay, Teams works but is not live: press `r` to refresh.
- Windows 10 and 11 only.
- Text only: no media is downloaded or sent.
- Teams: work or school accounts only. No channels and no meeting chats.
- WhatsApp support is unofficial and can get an account suspended.
- `store.db` is plain, unencrypted SQLite.
- The executable is not code-signed.

[Unreleased]: https://github.com/Page011/chatCLI/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Page011/chatCLI/releases/tag/v0.1.0
