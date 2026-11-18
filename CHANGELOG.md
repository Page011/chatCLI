# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Mouse: click a message, a link (asks y/n with the host), an attachment chip, a chat row (a second click opens it), the message box text, a completion item, a footer key hint (not send or quit), the y or n of a question, and items in the command bar, search, settings and the attach prompt; the wheel scrolls them. Pointer moves do nothing.
- `settings.toml`: one hand-edited file for key bindings, theme, layout, notifications, media limits, optional Teams scopes and WhatsApp privacy options. A broken section falls back to its defaults with one notice naming the line and key. `chatcli settings path` and `chatcli settings check`.
- Keys are named actions in a keymap. Rebind them in `settings.toml` (`[keys.<context>]`), with a notice for every mistake and a suggested name. `?` opens a help screen generated from the active keys.
- Message selection (`Enter` on a chat, or `Esc` from the message box): reply with a quote, react, edit and delete your own messages, open links, open attachments.
- Replies with a quote on Teams and WhatsApp. Teams needs the new `ChatMessage.Send` permission: log out of Teams and sign in again after upgrading.
- Reactions on Teams and WhatsApp, shown as counts under each message, with an emoji picker (`Ctrl+E`, Recent first) and `:shortcode` completion.
- Edit and delete your own messages on Teams and WhatsApp. Edits and deletes from other people update in place.
- `@mention` completion in group chats on both networks. Chats where you were mentioned show `@` after the unread count.
- `[keys.common]`: bind a key once for every context its action applies to. The movement, accept and back keys are one action each wherever they work (`up`, `down`, `top`, `bottom`, `page_up`, `page_down`, `accept`, `back`), so one binding moves through the chat list, a message selection, a popup and help alike. The editing keys (cursor, word jumps, `Ctrl+W`, `Ctrl+Backspace`, `Delete`, `Ctrl+U`, `Home`, `End`) work in every box you type in: the chat filter, the command bar, search, the emoji search and the file prompt.
- Message box editing: cursor movement, word jumps, whole-emoji delete, multi-line drafts (`Shift+Enter`, `Ctrl+J`, `Alt+Enter`), safe paste that never sends, and a length check before sending. Shift+Enter works on Unix terminals that offer the kitty keyboard protocol: chatcli turns it on at start and off at exit.
- Media: attachments show as chips; `o` downloads on demand into a capped cache and opens the file in the default app; `v` shows image thumbnails with half-block characters, or Sixel, Kitty or iTerm2 graphics where the terminal supports them. `chatcli media clear`.
- Send pictures and files on both networks: `Ctrl+O` path prompt with Tab completion, dropped paths, and clipboard pictures (`Alt+V`). Pictures are re-encoded, which drops EXIF data. Teams files go through your OneDrive and need the optional `files_write` setting.
- Optional Teams file downloads (`[teams] files_read`).
- Open links from messages in the browser (`u` in message selection), after an `Open <host>? y/n` line that shows the real host.
- Colour themes with named roles (`[theme]`, `high-contrast`, `light`, `dark`) and `NO_COLOR` support.
- Layout options: sidebar width, 12-hour time, date order, compact rows, hidden previews.
- Windows toast notifications for new messages, with a choice of how much text shows (`[notifications] preview`), folded per chat.
- Scroll past the top to load older messages on Teams (one page per scroll) and WhatsApp (asks your phone).
- `n` and `N` jump to the next and previous unread chat; `R` marks the chat read, and `:readall` every chat after a y/n.
- Pin, mute and hide chats (`p`, `m`, `z`, `Z`). WhatsApp syncs them with the phone.
- WhatsApp message status in a column left of your messages (#71): `✓` on your newest message the other person has read (in a group, everyone has), then `●` on each later one that reached their phone and `○` on each that has not yet. A message still sending (Teams too) shows `◷` there instead of `[sending]`. The marks can be changed in Settings, Appearance (`[theme] mark_sending`, `mark_sent`, `mark_delivered`, `mark_read`). Not in your chat with yourself. Teams cannot show this: Microsoft Graph does not say what other people have read.
- Local full-text search across both networks (`Ctrl+F`, `chatcli search <text>`).
- WhatsApp typing indicator, and your own typing (on by default; `[whatsapp] send_typing = false` turns it off).
- WhatsApp pictures download by themselves when you open a chat (or a new one arrives in it): thumbnails show under the message as with `v`. `[whatsapp] download_images = false` turns it off.
- `chatcli doctor [--online] [--json]` checks the setup and prints fixes.
- `chatcli relay url <url>` and `chatcli relay setup <url> --rotate`.
- `chatcli --help`.
- `--demo` shows every feature above on fake data, with its media in a temporary folder.
- Portability groundwork for Linux and macOS: data folder, file lock, browser launch, keychains, best-effort CI jobs.
- Linux: `CHATCLI_SECRET_STORE=file` keeps secrets in `secrets.json` in the data folder (mode 0600, not encrypted) for systems with no Secret Service.
- Winget and Scoop manifests (not yet submitted), a code-signing write-up, a memory benchmark script, a relay end-to-end test, WhatsApp fake-transport tests, and release notes built from this file.
- Sidebar preview: moving through the chat list (arrow keys, j/k, page keys, a click) shows each chat in the message pane at once. Enter replies, Esc goes back. A previewed chat counts as read after 2 s on screen, never while its pane is hidden.
- `:` command bar (everywhere outside the boxes you type in): lists every command and your chats. Commands also work as `:command` at the start of the message box, from one list with the emoji shortcodes: `:help` `:settings` `:keys` `:theme` `:layout` `:notifications` `:search` `:refresh` `:readall` `:signin` `:signout` `:about` `:tour` `:quit`.
- Settings screen (`F2`, `:settings`): theme and the 14 colour roles with a live preview, layout, notifications, media limits, Teams file permissions and WhatsApp privacy options. Every change applies at once and is saved to `settings.toml`, one key at a time, with comments kept. A `settings.toml` that does not parse is never written: the change applies for the session and `NOT SAVED` says why.
- Palettes (`:settings` > Appearance > Palette): `lines` (bordered panels, the default), `blocks` (no borders, the chat window keeps the terminal background, the sidebar and message box are fixed lighter shades that do not change with focus), `minimal` (a top rule only) and `rounded`. Each panel (sidebar, messages, message box, popups) can override its outline (`none`, `rule`, `single`, `rounded`, `double`, `thick`) and fill, and the top bar and footer their fill. `NO_COLOR` turns shaded panels back into lines.
- Emoji picker placement (#74): `[layout] emoji_picker = "popup" | "box" | "inline"` (Settings > Layout > Emoji picker) sets where `Ctrl+E` opens. `popup` is the box in the middle (the default). `box` docks it between the conversation and the message box, as wide as the message box, and the conversation shrinks so its newest messages stay in view. `inline` is one row inside the message box under the draft: the search on the left (showing the category while empty), the emoji after it. Where there is no room for `box` or `inline`, the picker opens in the middle. The reaction picker always opens in the middle.
- Group chats show messages Discord style: an avatar on the left (initials in the person's colour), the name in that colour with the time beside it, and the text underneath. Messages from one person within five minutes share one name row, and a blank row separates people. 1:1 chats get the same name rows without avatars, set 2 columns in from the sidebar. `[layout] avatars = "groups" | "all" | "off"` (Settings > Layout > Avatars); `off` keeps the one-line layout everywhere. A short conversation sits at the bottom of the pane, next to the message box.
- Profile photos over the initials avatars on terminals with Sixel, Kitty or iTerm2 graphics (other terminals, and `NO_COLOR`, keep initials). Teams asks for the new `User.ReadBasic.All` permission: sign in again after upgrading. WhatsApp looks photos up slowly (10 a minute, once per person per run) and signing out deletes them. Photos are cached for 7 days. `[layout] photos = false` (Settings > Layout > Photos) and `[teams] photos = false` (Settings > Teams > Profile photos) turn them off.
- Key bindings page (`:keys`): change any key by pressing the new one, swap on a clash, remove, and reset one or all. `/` filters it by action, name, key or context. Saved to `[keys]` in `settings.toml`.
- `Ctrl+C` and `Esc` are ordinary keys: a context can bind `Ctrl+C` to something else and `quit` can move to another key. chatcli checks instead that some key quits from every context and that help, popups and `y/n` questions keep a key that backs out; the keys page refuses a change that breaks this, and `settings.toml` falls back to `Ctrl+C` or `Esc` there with a notice. Capturing a key on the keys page takes every key, `Esc` and `Backspace` included. The header shows the key that quits.
- Key bindings page: several keys per action. `a` adds a key and keeps the others, `d` chooses one of the action's keys with `Left` / `Right` and deletes it. A key another action of the same context holds is refused and named; one that overlaps an `Everywhere` key or a two-key sequence asks first.
- Guided tour: `chatcli --demo --tour` (or `:tour` in the demo) plays 26 captioned steps over the demo data. `Space` pauses, `n` skips a step, `Esc` ends it. The demo and the tour run on a temporary copy of the settings and never write your real `settings.toml`.

### Changed

- Every popup (help, commands, search, the emoji picker, the `:shortcode` / `@mention` list, the file list, the sign-in panel) is drawn by one component, so they look and behave alike. Help, commands and search hide the screen behind them; the others keep it in view around them. The same search row, the same "no match" note on the list's first row (the emoji picker and search showed it elsewhere), the same full-width highlight on the chosen row (the file list highlighted only the name), the same hint style for the empty search row (the command bar was dimmer), and one "Terminal too small for ..." message.
- Tabs: signed-in networks come first and the ones waiting for a sign-in after, so a WhatsApp-only setup starts with WhatsApp in front; `Alt+1` / `Alt+2` follow the order shown. chatcli starts on the tab you used last (kept in the local store).
- `:` opens the command bar and `/` filters the chat list. In the message box, `:word` runs a command only when `Enter` takes it from the list (a space or `Esc` first sends it as text); a message that starts with `/` is plain text, so `//` escaping is gone.
- New default keys and chat flow, the same keys meaning the same thing everywhere. `Enter` (or `l`, `Right`) goes into a chat with its newest message selected, `i` goes to its message box; `Esc` steps back out: the message box to selection, selection (also `h`, `Left`) to the chat list, and on the list it clears the filter or closes the open chat. `j`/`k`, `g`/`G`, `H`/`L` (and `M` in selection) move in the chat list, message selection and help. In selection `c` edits and `x` or `d` deletes. `Ctrl+O` jumps back to where the selection was before the last big jump. With the newest message selected, new ones take the selection with them. `y` copies the selected message (the system clipboard, and the terminal through OSC 52, which also works over SSH). `?` helps from anywhere outside a text box. Removed: `q` (use `Ctrl+C` or `:quit`), `Tab` to accept in popups (use `Enter`), and the help's `q`, `?` and `F1` to close it (use `Esc`). `open_link` in the chat list keeps its action with no default key.
- Most default keys now live in `[keys.common]`, and `"key" = "unbound"` in a context's table stops a `common` key there.
- `settings.toml` is no longer read-only to chatcli: the settings screen writes it (see Added).
- WhatsApp read receipts are sent by default and can be turned off (`[whatsapp] send_read_receipts = false`).
- Toast notifications show the sender and message text by default (`[notifications] preview = "full"`). Set `"off"` to keep text out of toasts.
- An edited WhatsApp message no longer gets an `(edited)` text suffix: the message carries an edited flag. A deleted message has empty text and shows `[message deleted]`.
- Teams chats you hid in Teams are kept in the list as hidden (`H` shows them) instead of being dropped.
- `chatcli relay setup` stops with a message when the secrets already exist, instead of replacing them.
- Key matching is exact on modifiers (`Shift+Up` no longer acts as `Up`).
- Emoji, flags and CJK text are measured, cut and wrapped by grapheme cluster, so they are no longer split.
- `teams ...`, `relay setup`, `relay url` and `media clear` refuse to run while the full client is open, so a logout cannot race a token refresh.
- The store moves to schema version 2 (search index, reactions, attachments, chat flags) in one transaction. Back up `store.db` before the first start of this version if you want to be able to go back.

### Fixed

- Entering a chat whose messages had not loaded yet (`Enter`, `l` or `Right` straight after landing on it, or a chat never opened before) left you in the message box. The newest message is now selected when the messages arrive, unless you pressed a key, clicked or pasted in the meantime.
- Auto-download no longer fetches your own sent pictures back (that includes ones you sent from your phone: press `v` for those). A completion list closed by a click stays closed. Settings: clicking a section ends the `/` filter, and turning Teams photos back on asks again for people missed while off. A photo answer that arrives after sign-out is dropped on WhatsApp as well as Teams. Auto-download fetches three pictures at a time instead of up to twenty together. An open completion list no longer hides the newest messages. Avatars count towards the cache size limit at once. NO_COLOR also removes the colour swatches on the Appearance page.
- No more screen flashes when switching tabs or chats or scrolling: a picture that moves or goes away now has only its own cells redrawn instead of the whole screen being cleared, and every frame is sent as one synchronized update (terminals without support ignore it).
- Linux on Wayland: the clipboard is the native Wayland one (wlroots compositors such as Hyprland and sway, and KDE), so `Alt+V` reads pictures and copied text reaches other apps. Through XWayland it did neither. Older GNOME, without the data-control protocol, still goes through XWayland.
- WhatsApp: chats read in chatcli are now marked read on the phone and on other linked devices (#66). WhatsApp files 1:1 chats under a hidden ID (LID) on accounts it has moved over, and the phone ignored read marks sent under the phone number. Pin, mute, hide and read receipts now use the same address. Sending in a chat also marks it read. Each read mark writes one line to the log naming the kind of address used; a refused one is logged as an error.
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
- Footer: a y/n question is bold, the footer keeps whole items (never `P...`), the newline hint is `Shift+Enter` where the terminal reports it (else `Ctrl+J`), and the chat list and message box show the help key.
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
