# chatcli

[![CI](https://github.com/Page011/chatCLI/actions/workflows/ci.yml/badge.svg)](https://github.com/Page011/chatCLI/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Page011/chatCLI)](https://github.com/Page011/chatCLI/releases)
[![License](https://img.shields.io/github/license/Page011/chatCLI)](LICENSE)

**WhatsApp and Microsoft Teams in your terminal.** One interface for chats, messages, replies and files, without keeping either desktop app open.


## Features

- Teams and WhatsApp direct/group chats in separate tabs
- Replies, reactions, editing, deleting, mentions and attachments
- Search, unread counts, notifications, configurable themes and keybindings
- Optional Vim keybindings, keyboard navigation and local message storage
- Local-first: connect your own accounts; no chatcli-hosted messaging service

## Get started

**Windows 10/11:** Download the Windows ZIP from [Releases](https://github.com/Page011/chatCLI/releases), extract `chatcli.exe`, and run it in your terminal. Windows builds are not code-signed; verify the release's SHA-256 checksum before running.

**Build from source** (Rust stable and platform C build tools required):

```sh
git clone https://github.com/Page011/chatCLI.git
cd chatCLI
cargo build --release
```

Then launch `target/release/chatcli` (Windows: `target\\release\\chatcli.exe`).

Try the app without accounts or network access:

```sh
chatcli --demo --tour
```

Linux and macOS builds are experimental and must currently be built from source.

## Connect your accounts

**WhatsApp:** Run `chatcli`, select WhatsApp, press `Enter`, then scan the QR code using **WhatsApp → Linked devices → Link a device**.

**Microsoft Teams:** Requires a work/school account and your own Microsoft Entra app registration. Set the app's desktop redirect URI to `http://localhost` and add delegated Microsoft Graph permissions: `User.Read`, `Chat.ReadWrite`, `ChatMessage.Send`, `offline_access`. Then:

```sh
chatcli teams setup <client-id> <tenant-id>
chatcli teams login
chatcli
```

Teams works without a relay; press `r` to refresh chats. For automatic live updates, deploy the optional Cloudflare Worker using the [relay setup guide](docs/guide.md#make-teams-live-with-a-relay).

## Essential keys

| Key | Action |
| --- | --- |
| `Tab` | Switch network |
| `↑` / `↓`, `Enter` | Select and open a chat |
| `Enter` | Send message |
| `Ctrl+J` | New line |
| `Ctrl+Up` | Select a message to reply/react/edit |
| `Ctrl+O` | Attach a file |
| `Ctrl+F` | Search saved messages |
| `F2` | Settings |
| `F1` / `?` | Help |
| `Ctrl+C` | Quit |

## Important notes

- **Early release:** Some features need more live-account testing.
- **Unofficial WhatsApp client:** Uses a linked device via `whatsapp-rust`; this may violate WhatsApp's terms and risks account suspension. Avoid automated or high-volume sending.
- **Teams limitations:** Work/school accounts only; supports 1:1 and group chats, not channels or meeting chats. Live updates require the optional relay.
- **Privacy:** Chat history and downloaded media are stored locally **without encryption**. Protect your device with full-disk encryption. Avoid syncing the chatcli data folder to the cloud.

**More:** [Full setup and reference guide](docs/guide.md) · [Troubleshooting](docs/guide.md#troubleshooting) · [Report an issue](https://github.com/Page011/chatCLI/issues) · [Contributing](CONTRIBUTING.md) · [License](LICENSE)
