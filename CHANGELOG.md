# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
