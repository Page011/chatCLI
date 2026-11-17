# chatcli design

A terminal chat client for WhatsApp and Microsoft Teams chats, so the heavy desktop apps can stay closed. One full-screen app, a tab per network, every chat in a sidebar.

## Constraints that shape everything

- **Teams: no polling.** Microsoft allows polling Teams APIs at most once per day ([Terms](https://learn.microsoft.com/graph/api/resources/teams-api-overview#polling-requirements)). Live messages come from Graph change notifications, which Microsoft only delivers to a public HTTPS URL. A laptop has none, so each user deploys a tiny Cloudflare Worker relay. The relay forwards ids only: no message text ever passes through it.
- **Teams: work or school accounts only.** Graph chat APIs do not support personal Microsoft accounts. Each user registers their own Entra app; nothing tenant-specific is built into chatcli.
- **WhatsApp: unofficial.** chatcli links as a WhatsApp Web device through [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust). WhatsApp's terms forbid unofficial clients and an account can be suspended. Personal, low-volume use; never automate sending.
- **Scope:** Teams 1:1 and group chats (no channels, no meeting chats). WhatsApp direct and group chats. Text, replies, reactions, mentions, edit and delete, pictures and files. Media bytes are fetched when the user asks, and for WhatsApp pictures in an opened chat unless `download_images` is off.

## Workspace

| Crate | Path | Owns | May depend on |
|---|---|---|---|
| `chatcli-core` | `crates/core` | `model` (Network, ChatSummary, ChatMessage, Status, Event, Command), `store` (SQLite + search), `settings`, `media`, `limits`, `open`, `runtime`, `doctor`, `config`, `secrets`, `paths`, `log` | nothing internal |
| `chatcli-teams` | `crates/teams` | Entra sign-in, Graph calls, HTML to text, `backend` task, `relay` client, subscription upkeep | core |
| `chatcli-whatsapp` | `crates/whatsapp` | `backend` task on whatsapp-rust | core |
| `chatcli-tui` | `crates/tui` | ratatui screen, input, demo backend | core only, never a network crate |
| `chatcli` (bin) | `crates/app` | `main.rs` wiring (settings, stores, media cache, contexts), `cli.rs` subcommands, `doctor.rs`, `toast.rs` (the one Windows toast implementation) | all |
| relay Worker | `relay/` | Cloudflare Worker + Durable Object (TypeScript, Bun) | nothing |

Dependency notes: crate versions used by more than one crate (`image` among them) live in the root `[workspace.dependencies]`. `rusqlite` is pinned to 0.39 because whatsapp-rust's SQLite storage links `libsqlite3-sys` 0.37 and only one crate may link sqlite. whatsapp-rust runs with `default-features = false` because its default `simd` feature needs nightly Rust.

## Data on disk

All under `%LOCALAPPDATA%\chatcli` on Windows (macOS `~/Library/Application Support/chatcli`, Linux `$XDG_DATA_HOME/chatcli` or `~/.local/share/chatcli`; override with `CHATCLI_DATA_DIR`, which tests always set):

| File | Holds | Notes |
|---|---|---|
| `config.json` | Teams client and tenant id, relay URL | Not secret. Written by `chatcli teams setup` / `chatcli relay setup`. |
| `store.db` | Chats, message text, reactions, attachment metadata, chat flags, members, recent emoji, the full-text index, and WhatsApp contact names with phone-number/LID address links | Kept forever; reads are always LIMITed. No media bytes. The names table holds address-book names and phone numbers: WhatsApp logout deletes them (`Store::clear_names`); deleting `store.db` removes everything. |
| `settings.toml` | Key bindings, theme, layout, notifications, media limits, optional Teams scopes, WhatsApp privacy options | Written by the settings screen (one key at a time, comments kept) or edited by hand. A hand edit is read at start. A file that does not parse is never written. See the 0.2 contract below. |
| `media/` | Attachments you chose to open, named by hash | Capped by `[media] cache_max_mb`, evicted by age and size. Not encrypted. `chatcli media clear` empties it. |
| `tmp/` | Pasted clipboard images before sending | Files older than 24 h are removed at start. |
| `whatsapp.db` | WhatsApp linked-device keys | Anyone with this file can act as the device. Never copy or commit it. |
| `chatcli.log` | Connection, sign-in, subscription, relay and send events | Rolls at 5 MB. Never tokens, secrets or message text. |
| `secrets.json` | Linux only, with `CHATCLI_SECRET_STORE=file`: the secrets, in place of the Secret Service | Mode 0600, not encrypted. Absent otherwise. |
| `chatcli.lock`, `secrets.lock` | Single-instance lock, credential-store guard | Empty. `std::fs::File::try_lock` (LockFileEx on Windows, flock elsewhere); the OS frees it when the process dies. |

Secrets (Teams refresh token, relay token, relay client state) live in Windows Credential Manager under service `chatcli`, split into numbered pieces past its 2560-byte limit (macOS Keychain and Linux Secret Service elsewhere). On Linux, `CHATCLI_SECRET_STORE=file` swaps the store for `secrets.json` in the data folder, which holds the same entries as a JSON object of service to an object of entry name to value, and is rewritten whole through `config::write_private` (exclusive temp file, mode 0600, rename). Any other value of the variable is an error.

## Backend contract

Each backend is one tokio task: `pub async fn run(ctx: BackendCtx, commands: CommandRx, events: EventTx)` (`BackendCtx` = its own store connection, the settings and the media cache). It owns its network and never touches the terminal. On start: `Status::Starting`, then the cached chats from the store as `Event::Chats` (instant sidebar), then sign-in state, then a fresh chat list from the network. The TUI owns the terminal and never touches the network. Every user-visible failure is an `Event::Notice` or `Status::Error` and a `log::error` line.

## TUI

```
 chatcli   Teams 3  LIVE   |  WhatsApp 12  SIGN-IN NEEDED               Ctrl+C quit
+- Chats ---------------------+ +- Alex Example ----------------------------------+
| 2 Alex Example       09:15  | |                ---- Today ----                  |
|   is the build done?        | | 09:10 Alex Example  is the build done?          |
|   Project team       08:02  | | 09:15 You           yes, shipped                |
|   Sam: thanks               | |                                                  |
|                             | +--------------------------------------------------+
|                             | | > type a message, Enter to send                  |
+-----------------------------+ +--------------------------------------------------+
 Up/Down chat  Enter open  i type  / filter  : commands  ? help  Tab network   [notice line]
```

- Tabs show unread count and a text status chip (LIVE, NOT LIVE, SIGN-IN NEEDED, SIGNING IN, SCAN QR, CONNECTING, ERROR; shortened on terminals under 60 columns). Colour is never the only signal. Selected tab and row: tint or reverse plus bold, never a coloured stripe on one edge.
- Sign-in lives in the tab: Teams "TEAMS SIGN-IN NEEDED, Enter to sign in (opens browser)"; WhatsApp "Enter to link", then a QR drawn with half-block characters and "Linked devices (Android: menu, iPhone: Settings) > Link a device". A terminal too small for the QR says so.
- Times local: HH:MM today, weekday this week, dd/mm/yy older. Day separators in the message pane.
- Own messages labelled "You". Pending shows the sending mark (`◷`) in the status column, and a file in flight `[sending NN%]`; failed shows "NOT SENT - reason" in red text.
- Preview: moving the selection (Up/Down, j/k, PgUp/PgDn, Home/End, a click) shows that chat in the message pane at once from what the TUI holds; focus stays in the list. `Command::OpenChat` goes out once the selection has rested 250 ms (holding Down sends none for the chats passed over; never twice in a row for one chat). A previewed chat is not read: `Command::MarkRead` goes out on Enter or a send when it has unread messages, or after 2 s of preview, and the preview timer runs only while the message pane is on screen (active tab, terminal at least 60 columns wide or composer focused; hiding the pane stops the timer, showing it restarts it), because WhatsApp turns a read into read receipts. `requested` (the chat the backend was last asked for) is cleared by Esc from the composer, `r` and a status change to Online, so Enter on it asks the backend again. `App` stays pure: `fire_timers` runs what is due at the clock set by `tick`, and `next_deadline` tells the run loop when to wake, so it sleeps while no timer is pending and never wakes more than 10 times a second.
- Keys come from named actions in one keymap (the built-in keys, then the user's `[keys]` bindings); a context reads its own table, then `[keys.common]` for the actions that apply there (`common_applies`), skipping a common key that would type a character in a text box; the footer and the `?` help overlay are generated from it, so they cannot disagree with it. Ctrl+C (`quit`) and Esc (`back`) are ordinary bindings; `Keymap::missing_exits` works out, through `lookup`, the contexts no key quits from and the overlays no key backs out of: `Keymap::build` puts Ctrl+C or Esc back there with an error, and the keys page refuses the edit (`keys_edit::exit_lost`). The settings screen and the tour read the `[keys.common]` quit key. Two-key sequences are held with no timer and echoed in the footer; one in `[keys.common]` works where its action applies, like a single key. None are bound by default (`g` alone is top). A context's `unbound` entry for a `common` key hides it there (`Keymap::hidden`). Most default keys are in `common`; a context table holds only what differs (`OWN`). Mouse: click a tab, click a chat (preview), wheel scrolls.
- Colours come from named roles in a `Theme` (`theme.rs`) passed to the draw code. Built-ins use ANSI 16 only, so the terminal's own light or dark theme decides. Exceptions, all deliberate: `[theme]` overrides may use an index or RGB, thumbnails use RGB or the nearest xterm-256 index, and the sign-in QR is black on white so it scans. `NO_COLOR` removes every colour; under it the QR stays and thumbnails are switched off.
- `chatcli --demo` runs the TUI on a fake backend (no network, in-memory store, media and a copy of `settings.toml` in a temp folder deleted on exit) so every feature can be tried without accounts. `--demo --tour` plays a guided tour on top (starts from default settings).

## Relay protocol (Worker <-> chatcli)

- `GET /health` -> 200 `ok`.
- `POST /graph/notify` and `POST /graph/lifecycle`: a `validationToken` query parameter is echoed back as `text/plain` 200 (Graph's endpoint check, within 10 s). Otherwise the JSON body `{ "value": [...] }` is read; every item's `clientState` must equal secret `CLIENT_STATE` (constant-time compare) or it is dropped and counted. Answer 202 at once (Graph wants under 3 s). Forwarded frames carry ids only:
  - `{"type":"notify","items":[{"subscriptionId","changeType","resource"}]}` where `resource` looks like `chats('19:...')/messages('1612289765949')`.
  - `{"type":"lifecycle","items":[{"subscriptionId","lifecycleEvent"}]}` (`reauthorizationRequired`, `subscriptionRemoved`, `missed`).
  - `{"type":"alert","text":"..."}` for dropped bad-clientState items.
- `GET /ws`: WebSocket upgrade with `Authorization: Bearer <RELAY_TOKEN>` (constant-time compare, 401 otherwise). One Durable Object instance holds at most one socket; a new connection closes the old one (code 4000 "replaced"). On connect it sends `{"type":"hello","dropped":N}`: notifications that arrived with no socket connected since the last hello, so chatcli knows to catch up. `ping` text frames get `pong` via the hibernation auto-response.
- `RELAY_TOKEN` and `CLIENT_STATE` are Worker secrets and also live in Credential Manager (`relay-token`, `relay-client-state`). `chatcli relay setup <url>` generates both, stores them, writes `relay_url` to `config.json`, and prints the two `wrangler secret put` steps.
- chatcli side: connect with backoff 1 s to 60 s; send `ping` every 30 s, reconnect when no `pong` within 90 s; after every (re)connect and on `missed`, catch up: re-list chats and fetch messages for chats whose last message is newer than the store.
- Graph subscription: one per user on `/users/{id}/chats/getAllMessages`, `includeResourceData: false`, `lifecycleNotificationUrl` set, expiry 24 h, renewed at half-life, id kept in store meta `teams.subscription_id`, orphans (same notification URL, other id) deleted before creating, deleted on clean exit. Graph allows 10 per user, so a leak is a real failure.

## Design notes

1. **Who, and what can they do afterwards?** Someone who lives in WhatsApp and Teams chats: read and reply to both in one terminal with the desktop apps closed.
2. **Empty state.** First run shows a sign-in panel per tab. "No chats yet", "filter hides everything" and "network down" look different: the last is a status chip, never an empty list.
3. **10x / 100x.** 1,000 messages a day is ~365k rows a year, indexed on `(network, chat_id, ts)`, always read with a LIMIT. Teams chat lists page through `@odata.nextLink` with a logged cap. Media is never downloaded by default.
4. **Everyone at once.** One user, several clients (phone, official apps, chatcli). Two chatcli instances would fight over the subscription and relay socket: a single-instance lock refuses the second one.
5. **Performance.** The point is memory: measure the desktop apps' working set and private bytes over a normal session, then chatcli the same way. No claim without both numbers.
6. **Records.** The log file above.
7. **Every state it is seen in.** Terminal theme decides colours; every status has a text label. Check in a dark and a light terminal theme.
8. **Automated later.** Backends and store are libraries; the TUI is one front end. `chatcli teams send` already reuses them.
9. **Modular.** One crate per network behind the shared Event/Command contract. Most likely to change: the WhatsApp library when WhatsApp changes its protocol; only `crates/whatsapp` changes.
10. **How would we know it is broken?** Next section.

## Silent failures and how they surface

| Failure | What the user would see without a check | Check |
|---|---|---|
| Teams subscription expired or removed | A quiet afternoon on Teams | NOT LIVE chip whenever there is no active subscription or the relay socket is down; renew at half-life; `subscriptionRemoved` re-creates it |
| Notifications missed | Missing messages | `missed` and `hello.dropped > 0` trigger catch-up; every start compares each chat's last message with the store and logs gaps filled |
| Relay socket half-dead | Same | ping every 30 s; no pong in 90 s -> reconnect + catch-up |
| Bad `clientState` at the Worker | Nothing | Worker counts it and sends an `alert` frame; the TUI shows it |
| WhatsApp device unlinked (from the phone or by WhatsApp) | Endless reconnect | SIGN-IN NEEDED chip; reconnect loop stops |
| Teams refresh token rejected | Silent 401s | SIGN-IN NEEDED chip, Microsoft's reason in the log |
| Sign-in "works" but no refresh token | Fine once, signed out next start | First sign-in without a refresh token is refused with a reason |
| Send fails | Message looks sent | Message stays NOT SENT with the reason |
| Teams chat list capped | Missing chats | The cap is logged and shown as a notice |

## 0.2 contract (core)

The shared vocabulary for roadmap issues #2 to #52. `crates/core` is the source of truth; this section says what each part is for and the rules every crate follows.

### Model additions (`model.rs`, frozen after the core stage)

- `ChatSummary` gains `mentions` (unread messages that mention you, a subset of `unread`) and `flags: ChatFlags` (`pinned`, `muted_until`, `hidden`; `MUTE_FOREVER` = no end). `ChatMessage` gains `sender_id`, `reply_to: Option<QuotedRef>`, `reactions: Vec<Reaction>` (count desc, then emoji asc), `edited`, `attachments: Vec<Attachment>`, `mentions: Vec<PersonRef>`, `mentions_me`. Both have `blank(network)` so literals stay short.
- `Attachment` is metadata only. `FetchRef` is the backend's opaque recipe to fetch it (WhatsApp's holds the media key): `Debug` prints `FetchRef(<redacted>)`, and it is never logged. The sender's file name is display only and never becomes a path.
- New events: `MessageChanged`, `OlderMessages`, `OlderFailed`, `MediaReady`, `MediaFailed`, `SendProgress`, `Typing`, `Members`, `Capabilities`, and later `Receipts` (#71). New commands: `SendMedia`, `React`, `Edit`, `Delete`, `FetchMedia`, `LoadOlder`, `LoadMembers`, `Typing`, `SetChatFlag`; `Send` gains `reply_to` and `mentions`.
- `Event::Message` is only for a message that just arrived live. History is `Messages` / `OlderMessages`. Every edit, delete or reaction update is `MessageChanged`, which never raises a notification. A backend updates one field by reading the full message back from the store first, never by building a fresh one (that would wipe attachments, quote and mentions).
- `Capabilities` says what a backend can do now. Before the first `Capabilities` event the TUI treats every field as false.
- A command a backend does not support is never silent: `model::unsupported(network, &cmd)` returns the contract's default answer (`<Feature> is not available on <Network>.`), as the right outcome event for each command. Teams drops `Typing` silently; that is the one exception.

### Store schema v2 (`store.rs`)

- Migrations run forward only, 0 to 1 to 2 to 3 to 4, inside one `BEGIN IMMEDIATE` transaction that re-reads `user_version`, so two processes opening at once migrate once. The version is set inside the same transaction, so a crash can never leave a v2 layout labelled v1. A failure rolls back and says `Could not upgrade the local store (...). Your data is unchanged: report this and use the previous chatcli.` A newer schema is refused.
- The messages table is rebuilt with an integer `rid` so FTS5 (external content, `unicode61 remove_diacritics 2`, insert/update/delete triggers) has a stable rowid. New tables: `attachments`, `reactions`, `chat_flags`, `members`, `emoji_recent`.
- `upsert_message` replaces the message's attachment rows and never touches reactions. `confirm_message`, `rename_chat` and `replace_chats` move or clean the new tables; `replace_chats` keeps `chat_flags`, so a chat that comes back keeps its pin and mute. A deleted message reads back with no attachments or reactions.
- `mentions` on a chat row belongs to `bump_mentions` and `set_unread`: a chat-list refresh never zeroes it, it stays within `unread`, and `set_unread(.., 0)` clears it.
- Search: `fts_query` turns user text into a quoted, prefix-matched FTS5 expression bound as a parameter. `Store::search` returns newest first, never matches deleted messages, splits the snippet into `SnippetPart`s (no control characters leak) and turns an FTS error into `Search could not read that query: try plain words.`
- Account binding: `Store::bind_account(network, id)` records the signed-in account in meta. A different id deletes that network's cached chats, messages, attachments, reactions, members, read and delivery marks, flags, names and older-history cursors (they hold account-relative facts such as `from_me`), and returns true so the caller reloads. Backends call it after sign-in.
- `Store::clear_names(network)` deletes the saved contact names and id links; WhatsApp logout calls it.
- Schema v3 adds `read_marks (network, chat_id, reader, ts)` and v4 `delivered_marks` of the same shape. `advance_read_mark` and `advance_delivered_mark` only move a mark forward; `set_delivery_baseline` writes the reader `BASELINE_READER` (`""`) once and never moves it. `rename_chat`, `replace_chats` and `bind_account` move or clean both with the other chat tables.
- Every read has a LIMIT or a full primary key. Search on a 200,000-row store (debug build): one common word 130 ms, rare token 0.2 ms, a prefix matching every row 650 ms (`cargo test -p chatcli-core search_speed -- --ignored --nocapture`).

### Settings (`settings.rs`, issue #2)

- `settings.toml` in the data folder may be hand-edited, and is written by the settings screen only (`config.json` is the file commands write). A missing file is today's behaviour exactly. `save_edits` reads the file, edits it with `toml_edit` (comments, order and unknown keys stay), checks the result still parses and writes it by temp file and rename; a file that does not parse is refused with `NOT SAVED`, so a hand-broken file is never mangled.
- The text is cut into groups at top-level `[section]` headers, and each group is parsed alone. A syntax error costs only its own section (defaults plus one error with the line). A bad value, or `max_file_mb` above `cache_max_mb`, gives that section's defaults plus one error naming line and key. Unknown sections, keys and root keys make one warning. `SettingsProblem::text()` is the one-line form for the notice bar and the CLI.
- Core keeps `[keys]` and `[theme]` as raw text; the TUI validates action names, key strings, roles and colours.
- `chatcli settings path` prints the location. `chatcli settings check` prints every problem, exits 1 on any error and 0 on warnings only.

### Media, limits, opening (`media.rs`, `limits.rs`, `open.rs`)

- `MediaCache` stores downloads as `<root>/<network>/<32 hex of sha256(network + "\n" + fetch_ref)>.<ext>`: never the sender's name. The extension comes from an allow list, never from the blocked list. Writes go to `.part` and are renamed on commit; a write past `max_file_mb` errors; an abandoned `.part` is deleted. `evict` removes stale `.part` files, files past the retention, then the least recently opened until the total is within `cache_max_mb`; the file just written is never the one evicted. Cached media is not encrypted on disk.
- `prepare_image` decodes and re-encodes, which drops EXIF and other metadata, after applying the EXIF rotation. It refuses more than 25 megapixels (`IMAGE TOO LARGE`) and a result over the caller's limit (`IMAGE OVER <n> MB`).
- `limits`: `teams_escape` / `teams_body_len` are the canonical Teams HTML body and its length (the Teams body builder should call `teams_escape`); `send_limit_bytes` is the TUI's pre-check.
- `open::web_url` accepts only clean http(s) URLs with a host. `url_host` returns the host the browser really goes to (credentials never hide it). `open_file` refuses executable types (`BLOCKED: executable`) and missing files (`FILE NOT FOUND`). `opener` passes the argument to the OS with no shell.

### Runtime context (`runtime.rs`, `doctor.rs`)

`BackendCtx { store, settings, media }` for each backend; `UiCtx { settings, startup_notices, store, media, notifier, demo }` for the TUI; `Notifier` is implemented by the app crate (the TUI never links a toast crate). `doctor::{Check, Level}` are the result types of `chatcli doctor`.

### Credential Manager layout (`secrets.rs`)

A secret is stored in pieces under `<name>~<generation>~<index>` and a single pointer entry `<name>` = `@v2:<generation>:<pieces>` says which set is live. A write fills the other generation and only then rewrites the pointer (one credential, so one atomic switch): a process killed half way leaves the old, complete secret. Secrets written by earlier versions (`<name>`, `<name>-1`, ...) still read and are converted by the next write. A piece missing under a valid pointer reads as "no secret" and is logged.

## 0.2 crates

### Teams (`crates/teams`)

- Scopes: `offline_access User.Read Chat.ReadWrite ChatMessage.Send`, plus `Files.Read.All` when `[teams] files_read` is on and `Files.ReadWrite` when `files_write` is on, and `User.ReadBasic.All` (profile photos, no admin consent) when `photos` is on. Nothing optional is asked for while its setting is off. Refresh falls back to fewer scopes and never signs the user out for a new optional scope; a one-time notice names scopes that settings want but the token lacks.
- Every text send is an HTML body built by `limits::teams_escape` (a `body::text_body` with no mentions is exactly that string), checked against the size limit on the final HTML, including `<at>` tags, before any request.
- Reply is `replyWithQuote` and needs `ChatMessage.Send`. Without the scope, or on a 403, the send fails as `REPLY NEEDS ChatMessage.Send, sign in again`, stored as NOT SENT with its quote, and never falls back to a plain send. Edit is a PATCH that replaces the whole body, so a message with a quote or files is refused locally ("edit it in Teams") instead of silently dropping them; an edit sends the message's stored mentions again (`<at>` tags for the names still in the text), so the people stay mentioned. A link shown as `text (https://...)` is saved back in that literal form.
- Reactions: one per user. Setting a different emoji first unsets the user's others. The store keeps each Teams reaction under the reactor key `<user id or @me>#<emoji>`, so anything reading raw reaction rows must not assume reactor == user id; aggregation is unaffected.
- Media metadata is read from inline pictures and `reference` file attachments; no bytes are touched until `FetchMedia`. Inline pictures stream into the media cache in constant memory. Linked files need the `files_read` scope; without it the answer is `FILE: open in browser` with a fallback URL and no Graph request. Redirects to the pre-signed address drop the bearer token on another host. Uploads over 4 MiB use an upload session on an https address with no bearer token, in 10 MiB chunks.
- Hidden chats are kept (`flags.hidden`), and each chat refresh makes Teams' `viewpoint.isHidden` win over the stored flag. Pin and mute are local only.
- Older history is one Graph page per scroll (`$top=50`, `$orderby` and `$filter` on `createdDateTime`), answered from the store first. The store meta keys `teams.older.<chat>` (`start` or `next:<link>`) and `teams.members.<chat>` (timestamp) hold the cursor and the members-cache age. A `nextLink` to any other host is refused so the token never follows it.
- Catch-up also refetches a chat whose last message was edited or deleted (compared with the chat-list preview). `reauthorizationRequired` is answered with one renew `PATCH`, never reauthorize plus renew (Graph forbids both within 10 minutes).
- The Graph base URL is a constant. The only override is a `#[cfg(test)]` field used with an in-process mock HTTP server; a test proves no release code path reads it or an environment variable.
- `doctor::checks(online)` feeds `chatcli doctor`. Its relay probe is a plain `GET /ws` without a WebSocket upgrade (the Worker answers 426 for a good token, 401 for a bad one), so running doctor can never evict a running client's relay socket.

### WhatsApp (`crates/whatsapp`)

The backend sends and receives through the `Wa` trait (`wa.rs`); `RealWa` wraps whatsapp-rust and tests use a recording fake. Slow calls (react, edit, delete, uploads, downloads, phone history, flags, read marks) run on tasks in a `JoinSet` and answer with `Msg::Done`; the store write and the event happen on the backend task. Shutdown aborts the tasks within 5 s. History chunks never overwrite a message that was edited or deleted.

- `fetch_ref` is versioned JSON `{v, kind, path, key, enc, sha, len}`. It holds the media key and is never logged. Downloads are checked against the sender's plaintext hash before they are written.
- A reply needs the quoted message's sender. When it is unknown (an older group message chatcli never saw) the send fails with a notice and is never sent plain.
- Read receipts and our own typing are on unless `send_read_receipts` and `send_typing` are set to false. Incoming typing is always shown.
- Read and waiting ticks (#71, `receipts.rs`): `read` and delivery receipts for our stored messages advance a per-reader mark (`read_marks`, `delivered_marks`: the `ts` of our newest message that reader has read or received, keyed by user part, forward only). A read counts as delivered. `played` is ignored: WhatsApp sends it even when the reader turned read receipts off, and a voice note gets a `read` receipt when seen like any message. A direct chat is read (delivered) up to its newest mark; a group up to the oldest mark among its members (none while any member has none), matching a member by phone number or LID through the `names` links. The member list is fetched once a run per group for this, quietly (`Done::ReadMembers`: stored, no `Members` event, a failure only logged).
- Messages from before delivery was tracked have no receipts, so each chat gets a delivery baseline the first time it matters, counted as every member's delivered mark: our first stored message in it (sent from here or from the phone; baseline = its `ts` - 1) or its first receipt (baseline = now). Without a baseline or a receipt nothing is known and nothing is marked. The chat with yourself never reports marks: no one else sends receipts there.
- `Event::Receipts { read, delivered, since }` (`since` = the baseline) goes out when a mark moves, when a baseline is set, and after `Messages` for `OpenChat`. The TUI keeps it per chat (`NetState::receipts`) and draws a status column (`status_of`, `put_status`): the last two columns of the indent on a message's first row under its name row, in every layout (name-row padding, the gap after an avatar, the gap after the sender). Own `Sent` messages only: the read mark (`accent`) on the newest at or before `read`; on later ones newer than `since`, the delivered mark (dim) when at or before `delivered`, or the sent mark (dim) when not (at once, no timer: a normal delivery changes it within a second). A `Pending` message of yours (either network) shows the sending mark there instead. The marks are `Theme::marks`, `[theme] mark_sending` / `mark_sent` / `mark_delivered` / `mark_read` (defaults `◷` `○` `●` `✓`, edited on the Appearance page): one character one column wide (`theme::mark_char`), since the column is one wide; marks that are the same give a warning. Shapes differ, so colour is never the only signal. Older history carries nothing (not one mark per message). Never red: waiting is not a failure on the user's side. Teams has no source: Graph's `viewpoint` is only your own read time.
- `[whatsapp] download_images` (default on): `App::auto_images` asks for the pictures of the newest 20 messages (not our own) of the open WhatsApp chat through the same path as `v` (`begin_view`), once the chat counts as read (Enter, or the 2 s preview timer) and when a new message or history lands in it. Only with `fetch_media`, inline images on, and a size within `max_file_mb` (an unknown size is capped by the backend while downloading); each picture once per run (`auto_tried`), failures are retried by hand with `v`.
- Pin, mute (forever) and archive (hidden) are written to the phone and read back from phone events; the phone wins.
- Executable files are sent as commanded: the TUI asks first, and `SendMedia` has no flag the backend could check.

### TUI (`crates/tui`)

- `App` stays pure. Things that touch the world are queued and the run loop (`lib.rs`) performs them: `take_open_urls` and `take_open_files` (through `core::open`, no shell), `take_decodes` (thumbnails on worker threads, one at a time, 25 MP cap), `take_clipboard_request`, `take_trash` (deletes only `paste-*` files inside the temp folder), `take_toasts` (handed to `ctx.notifier`). A worker that panics answers with a visible failure, never a stuck "loading".
- Every box you type in edits a `TextInput` (`input.rs`): text with a cursor measured in grapheme clusters, the editing operations, and `view` for a one-line box (scrolls to keep the cursor visible, shows text only through `clean_line`). The chat filter, the command bar, search, the emoji picker's query, the file prompt and settings values use it directly. The message box (`composer.rs`) is a `TextInput` plus the chosen `@mentions`, wrapped at spaces, growing to 6 rows. Lists share `list.rs`: `step` (wraps), `page` (stops at the ends), `keep_visible` (the first row to show, never past the end) and `matches` (the one case-insensitive "type to filter" test, used by the chat filter and the command bar's chats). All measuring, cutting and wrapping goes through `text.rs` by cluster. Suggestions (`:shortcode`, `@mention`), the emoji picker, the file prompt, search and the link prompt are modal overlays handled by `App`.
- Commands, settings, keys and the tour (S1-S5 of the settings work). `commands.rs` is the one registry behind the `:` bar (`palette.rs`) and `:command` at the start of the message box (stricter matching there, `typed_matches`, so emoticons like `:P` stay text; the list mixes commands and emoji and Enter takes the highlighted item; nothing is parsed at send time); `App::run_cmd` runs them. The settings screen (`settings_view.rs` model, `settings_ui.rs` drawing, `app/settings.rs` keys) takes its own fixed keys while open. Every change goes through `apply_settings` (live) and queues a `SettingEdit`; the run loop saves the queue with `save_edits` (`save_pending`) and sends `Command::Settings` to the backends when a setting they read changed. The keys page edits `[keys]` by capture (`keys_edit.rs`: pure bind, swap, add, remove, reset; adding checks overlaps only where two bindings are live at once: the same context, or a context and a `common` key that reaches it). The demo runs on a temporary copy of `settings.toml`, so nothing it changes reaches the real file.
- The tour (`tour.rs` script, `app/tour.rs` player) is data: a list of steps, each a caption and keys. One timer through `next_deadline` / `fire_timers`, and every key goes through `key_now`, the handler typed keys use, so the tour cannot do what a user could not. `Space` pauses, `n` runs the rest of a step with no pauses, `Esc` ends it, `Ctrl+C` quits; a paste or a click is ignored while it plays. Three tour-only acts go straight to a place (`Row`, `SelectMsg`, `ToList`) so reordering settings rows or demo messages cannot break it. `tests/tour.rs` plays all steps against the real demo backend on a paused clock and replays the saved edits to prove the settings end where they began.
- Message selection is a focus (`Focus::Select`) with its own keymap context. The selected message is kept by id, so it survives incoming messages, wheel scroll and resize. Pending, failed and deleted messages cannot be selected. Reply and edit are per-chat targets drawn as one line in the message box; an edit keeps and later restores the earlier draft. React, edit and delete never change the pane by themselves: the backend answers `Event::MessageChanged` (replace by id, no notification, no read, a never-loaded chat is skipped).
- Attachments are chips drawn from `attachments`; the backends' text markers are removed when chips exist. `o` and `v` act on the selected message only; nothing else downloads by itself except WhatsApp pictures under `[whatsapp] download_images` (see WhatsApp above). `MediaReady` paths outside the cache root, or with `..`, are refused. Thumbnails are half-block lines in the conversation (they clip with scrolling); a terminal that reports Sixel, Kitty or iTerm2 gets the sharper image drawn over a thumbnail that is wholly on screen, and the screen is cleared when that set changes.
- Chats sort pinned first (`chats.rs`). Search runs `Store::search` on the UI thread per keystroke (limit 50). Older history is requested only after a user scroll past the top, one request per chat at a time; `at_start` ends the requests; the merge keeps the view still. Typing events live per chat with a 10 s end checked on the 1 s tick.
- Toasts: `notify.rs` decides (not own, not stale, not muted, not the open chat while focused; one per chat per 10 s, folded after). Focus comes from terminal focus events; a terminal that never sends one counts as unfocused. Toasts hang on `Event::Message` only, so backends must send it only for live arrivals. `app/src/toast.rs` registers the AppUserModelID `chatcli` under `HKCU\Software\Classes\AppUserModelId` and shows the toast through WinRT.
- `Event::Messages` replaces the TUI's messages from the batch's oldest `(ts, id)` onward and keeps older ones and unresolved local sends. A notice can be app-wide (no network prefix).

### App (`crates/app`)

`main.rs` loads `settings.toml` once, opens the UI store first (it runs the migration), then one store per backend, builds the media cache, evicts it and cleans the temp folder, and passes `BackendCtx` and `UiCtx` down. Settings problems from core become start-up notices; the TUI adds its own `[keys]` and `[theme]` problems and shows one notice line (`chatcli settings check` lists all). Commands that change sign-in state or the media cache take the same lock as the full client, so they never run beside it (a `teams logout` between a token refresh's read and write would bring the sign-in back). `doctor` does not take it: it reports the lock instead.

### Risks to know

- Terminals disagree on emoji widths (VS16, ZWJ) and on whether ConPTY returns the Sixel capability reply. A wrong answer falls back to half-blocks; the picker uses a fixed 3-column stride per cell to limit damage.
- The capability query reads stdin once and can swallow one key on a terminal that never answers.
- A terminal that never sends focus events makes every message in the open chat toast.
- Cached media and `store.db` are not encrypted. Search on a very large store can take about 0.6 s in a debug build for a prefix that matches every row (release not measured).
- Which emoji Teams accepts as a reaction is undocumented, and WhatsApp mention notification for LID-addressed group members is unverified.

### More silent failures

| Failure | What the user would see without a check | Check |
|---|---|---|
| Toast cannot be shown | No notification, ever | One log line, once; never a loop |
| Reply without `ChatMessage.Send` | A plain message that looks like a reply | `REPLY NEEDS ChatMessage.Send, sign in again`, stored NOT SENT, no plain send |
| Schema migration fails | A half-converted store | One transaction, rolled back, `Your data is unchanged`; a newer schema is refused |
| Cached media past its cap or age | A growing disk | Evicted at start and after each download; `chatcli media clear` |
| Phone does not answer a history request | A spinner that never ends | 30 s deadline, `PHONE DID NOT ANSWER`, retry allowed |
| Sign-in command races a token refresh | A logout that undoes itself | Those commands take the single-instance lock |
| A setting the app cannot read | Defaults with no explanation | One notice with file, line and key; `chatcli settings check` |
