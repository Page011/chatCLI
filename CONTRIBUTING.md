# Contributing to chatcli

Thanks for helping. This project is small and the bar is simple: it builds, it is tested, and it never leaks anything private.

## Privacy rules (read first)

This repository is public. Never put any of these in code, tests, snapshots, logs, issues or PRs:

- Real names of people or organisations, chat or tenant ids, email addresses, phone numbers.
- Tokens, secrets, client ids or tenant ids of real apps, Cloudflare account ids, real hostnames of your own Worker.
- Local machine paths, message text, or screenshots of real chats.

Use obvious fake data (`Alex Example`, `Sam Sample`, `example.invalid`). Security problems are not reported in issues: see [SECURITY.md](SECURITY.md).

## Run it without accounts

```
cargo run -p chatcli -- --demo
```

`--demo` shows the full screen on fake data. No Teams or WhatsApp account and no network is needed. Press `Ctrl+C` to leave.

## Build and check

You need Rust stable (see `rust-toolchain.toml`) and, on Windows, the Visual Studio C++ Build Tools. chatcli targets Windows 10 and 11.

```
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run `cargo fmt --all` to fix formatting. CI runs the same commands and must be green before merge.

## Relay Worker

The Cloudflare Worker in `relay/` uses [Bun](https://bun.sh). Use Bun only, not npm, npx or yarn.

```
cd relay
bun install
bun test
bun run typecheck
```

## UI snapshots

Screens are checked against stored snapshots in `crates/tui`. If you change the UI on purpose, rewrite them and read the diff before you commit:

```
UPDATE_SNAPSHOTS=1 cargo test -p chatcli-tui
```

On Windows `cmd`, run `set UPDATE_SNAPSHOTS=1`, then the test, then `set UPDATE_SNAPSHOTS=`.

## Pull requests

- One change per PR. Keep it small and reviewable.
- Use [Conventional Commits](https://www.conventionalcommits.org) for commit messages and PR titles: `feat: ...`, `fix: ...`, `docs: ...`, `refactor: ...`, `test: ...`, `chore: ...`. Add `!` or a `BREAKING CHANGE:` footer for breaking changes.
- Add or update tests for behaviour changes. Update docs and the `[Unreleased]` section of [CHANGELOG.md](CHANGELOG.md).
- Fill in the PR template, including the commands you ran.
- Every failure the user can see must show as a notice or status, and be logged without tokens or message text. See the failure table in the design doc.

## Design

Read [docs/DESIGN.md](docs/DESIGN.md) before a larger change: the crate boundaries (the TUI never touches the network; backends never touch the terminal), the relay protocol, the data layout and the silent-failure checks.

## Licence

By contributing you agree that your contribution is licensed under the Apache License 2.0, the same as the project.
