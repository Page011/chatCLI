# Security policy

## Supported versions

Only the latest release receives security fixes. Please update before reporting.

## Reporting a vulnerability

Report privately. Do not open a public issue, discussion or PR.

1. Open the [Security tab](https://github.com/Page011/chatCLI/security) of this repository.
2. Select **Report a vulnerability**, or go straight to <https://github.com/Page011/chatCLI/security/advisories/new>.
3. Describe the problem, the version, and the steps to reproduce. Do not include real tokens, real chat content or personal data: use fake values.

## In scope

- Handling of tokens and secrets: the Teams refresh token, the relay token and the relay client state (Windows Credential Manager storage, logging, memory).
- Relay Worker authentication: the `RELAY_TOKEN` check on `/ws`, the `clientState` check on Graph notifications, and anything that makes the relay forward data it should drop.
- Local data files under `%LOCALAPPDATA%\chatcli`: `store.db`, `whatsapp.db`, `config.json`, logs, and their permissions.
- Anything that causes message text or secrets to reach a log, the relay or a third party.

## Out of scope

- WhatsApp account suspension. The WhatsApp link is unofficial and its terms forbid unofficial clients. That risk is documented in the README.
- Problems in Microsoft Graph, Cloudflare, WhatsApp or other upstream services, or in a dependency with no impact on chatcli. Report those upstream.
- Attacks that need local administrator access or an already compromised Windows account.
- Data stored unencrypted by design and documented: `store.db` is plain SQLite.

## What to expect

- Acknowledgement within 7 days.
- A first assessment within 14 days, and updates as the work progresses.
- A fix in a new release for confirmed issues, with credit in the advisory if you want it.
- Please keep the report private until a fix is released.

This is a small personal project maintained on a best-effort basis. There is no bug bounty.
