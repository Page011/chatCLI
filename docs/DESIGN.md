# chatcli design

A terminal chat client for WhatsApp and Microsoft Teams chats, so the heavy desktop apps can stay closed. One full-screen app, a tab per network, every chat in a sidebar.

## Constraints that shape everything

- **Teams: no polling.** Microsoft allows polling Teams APIs at most once per day ([Terms](https://learn.microsoft.com/graph/api/resources/teams-api-overview#polling-requirements)). Live messages come from Graph change notifications, which Microsoft only delivers to a public HTTPS URL. A laptop has none, so each user deploys a tiny Cloudflare Worker relay. The relay forwards ids only: no message text ever passes through it.
- **Teams: work or school accounts only.** Graph chat APIs do not support personal Microsoft accounts. Each user registers their own Entra app; nothing tenant-specific is built into chatcli.
- **WhatsApp: unofficial.** chatcli links as a WhatsApp Web device through [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust). WhatsApp's terms forbid unofficial clients and an account can be suspended. Personal, low-volume use; never automate sending.
- **Scope:** Teams 1:1 and group chats (no channels, no meeting chats). WhatsApp direct and group chats.

## Workspace

| Crate | Path | Owns | May depend on |
|---|---|---|---|
| `chatcli-core` | `crates/core` | `model` (Network, ChatSummary, ChatMessage, Status, Event, Command), `store` (SQLite), `config`, `secrets`, `paths`, `log` | nothing internal |
| `chatcli-teams` | `crates/teams` | Entra sign-in, Graph calls, HTML to text, `backend` task, `relay` client, subscription upkeep | core |
| `chatcli-whatsapp` | `crates/whatsapp` | `backend` task on whatsapp-rust | core |
| `chatcli-tui` | `crates/tui` | ratatui screen, input, demo backend | core only, never a network crate |
| `chatcli` (bin) | `crates/app` | `main.rs` wiring, `cli.rs` subcommands | all |
| relay Worker | `relay/` | Cloudflare Worker + Durable Object (TypeScript, Bun) | nothing |

Dependency notes: `rusqlite` is pinned to 0.39 because whatsapp-rust's SQLite storage links `libsqlite3-sys` 0.37 and only one crate may link sqlite. whatsapp-rust runs with `default-features = false` because its default `simd` feature needs nightly Rust.

## Data on disk

All under `%LOCALAPPDATA%\chatcli` (override with `CHATCLI_DATA_DIR`, which tests always set):

| File | Holds | Notes |
|---|---|---|
| `config.json` | Teams client and tenant id, relay URL | Not secret. Written by `chatcli teams setup` / `chatcli relay setup`. |
| `store.db` | Chats and message text | Kept forever; reads are always LIMITed. No media. |
| `whatsapp.db` | WhatsApp linked-device keys | Anyone with this file can act as the device. Never copy or commit it. |
| `chatcli.log` | Connection, sign-in, subscription, relay and send events | Rolls at 5 MB. Never tokens, secrets or message text. |

Secrets (Teams refresh token, relay token, relay client state) live in Windows Credential Manager under service `chatcli`, split into numbered pieces past its 2560-byte limit.

## Backend contract

Each backend is one tokio task: `pub async fn run(store: Store, commands: CommandRx, events: EventTx)`. It owns its network and never touches the terminal. On start: `Status::Starting`, then the cached chats from the store as `Event::Chats` (instant sidebar), then sign-in state, then a fresh chat list from the network. The TUI owns the terminal and never touches the network. Every user-visible failure is an `Event::Notice` or `Status::Error` and a `log::error` line.

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
 Tab network  Up/Down chat  Enter open  / filter  r refresh  Esc back     [notice line]
```

- Tabs show unread count and a text status chip (LIVE, NOT LIVE, SIGN-IN NEEDED, SIGNING IN, SCAN QR, CONNECTING, ERROR; shortened on terminals under 60 columns). Colour is never the only signal. Selected tab and row: tint or reverse plus bold, never a coloured stripe on one edge.
- Sign-in lives in the tab: Teams "TEAMS SIGN-IN NEEDED, Enter to sign in (opens browser)"; WhatsApp "Enter to link", then a QR drawn with half-block characters and "Linked devices (Android: menu, iPhone: Settings) > Link a device". A terminal too small for the QR says so.
- Times local: HH:MM today, weekday this week, dd/mm/yy older. Day separators in the message pane.
- Own messages labelled "You". Pending shows "sending"; failed shows "NOT SENT: reason" in red text.
- Keys: Tab/Shift+Tab or Alt+1/Alt+2 switch network; Up/Down or j/k move; Enter opens a chat and focuses the composer; Esc back to the list; `/` filters chats; `r` refreshes (user-triggered only); PgUp/PgDn scroll; Ctrl+C quits anywhere, `q` from the list. Mouse: click a tab, click a chat, wheel scrolls.
- ANSI 16 colours only, no RGB, so the terminal's own light or dark theme decides.
- `chatcli --demo` runs the TUI on a fake backend (no network) so the screen can be tried without accounts.

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
