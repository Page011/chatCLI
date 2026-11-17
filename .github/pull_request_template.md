## What and why

<!-- One change per PR. What does it change, and what problem does it solve? Link the issue if there is one. -->

## How it was tested

<!-- Paste the commands you ran and the result. -->

```
cargo test --workspace
```

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Relay tests pass (`bun test` and `bun run typecheck` in `relay/`), if `relay/` was touched
- [ ] Snapshots updated with `UPDATE_SNAPSHOTS=1 cargo test -p chatcli-tui` and the new screens read and checked, if the UI changed
- [ ] `CHANGELOG.md` has a line under Unreleased for this change (it becomes the release notes), unless the change is invisible to users
- [ ] Docs updated (README, `docs/DESIGN.md`, `CHANGELOG.md` under Unreleased), if behaviour changed
- [ ] No secrets, tokens, personal data or real names in code, tests, snapshots or logs
