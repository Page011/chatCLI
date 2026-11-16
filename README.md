# chatcli

[![CI](https://github.com/Page011/chatCLI/actions/workflows/ci.yml/badge.svg)](https://github.com/Page011/chatCLI/actions/workflows/ci.yml)

A terminal chat client for WhatsApp and Microsoft Teams. One full-screen app, one tab per network, every chat in a sidebar. You read and reply to both with the desktop apps closed.

You run everything yourself: your own Teams app registration, your own optional Cloudflare Worker, your own WhatsApp link. Nothing is hosted for you, and nothing is shared with anyone else.

```
 chatcli  Teams 101 LIVE   WhatsApp SIGN-IN NEEDED                   Ctrl+C quit
┌ Chats ───────────────┐┌ Alex Example ────────────────────────────────────────┐
│    Alex Example 09:15││                  ---- Yesterday ----                 │
│    Alex Example: is …││16:05 Alex Example  Notes from the review: the summary│
│99+ Project Team 08:02││                    page needs a shorter introduction,│
│    Sam Sample: thanks││                    the figures need captions, and the│
│  1 Sam Sample     Sun││                    appendix can move to the end of   │
│    You: see you tomo…││                    the document.                     │
│    Release Plann… Fri││                    ---- Today ----                   │
│    Jordan Placeholde…││09:10 Alex Example  is the build done? 🚀             │
│    田中太郎… 05/11/26││                    今日はどうですか                  │
│    了解しました      ││09:15 You           yes, shipped                      │
│    Jordan P… 09/10/26││09:30 You           on it, will confirm shortly       │
│    You: thanks       ││                    [sending]                         │
│                      ││09:30 You           this one will fail                │
│                      ││                    NOT SENT: demo: this chat refuses │
│                      ││                    messages                          │
│                      │└──────────────────────────────────────────────────────┘
│                      │╔ Message ═════════════════════════════════════════════╗
│                      │║> typing a draft                                      ║
└──────────────────────┘╚══════════════════════════════════════════════════════╝

Enter send  Esc back to list  PgUp/PgDn scroll  Tab network
```

That is `chatcli --demo`: fake data, no accounts, no network.

## Read this first

- **Early release.** chatcli works day to day but has rough edges. Please [open an issue](https://github.com/Page011/chatCLI/issues) when something breaks. Live Teams updates through the relay are the newest and least tested part.
- **WhatsApp support is unofficial.** chatcli links to your account as a WhatsApp Web linked device, using [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust). WhatsApp's terms forbid unofficial clients. WhatsApp can suspend an account that uses one. Use it for personal, low-volume chatting only. Never automate sending. You accept this risk.
- **Teams needs a work or school account.** Microsoft Graph chat APIs do not work with personal Microsoft accounts. Your tenant must also allow you to register an app (or an admin must do it for you).
- **Windows 10 or 11 only for now.** Secrets are kept in Windows Credential Manager.
- **Scope.** Teams 1:1 and group chats (no channels, no meeting chats). WhatsApp direct and group chats. Text only: no media is downloaded.

## Download

Get `chatcli-v<version>-x86_64-pc-windows-msvc.zip` from [GitHub Releases](https://github.com/Page011/chatCLI/releases). Unzip it and run `chatcli.exe`, or put it on your `PATH`.

Verify the download before you run it. The release also has a `.sha256` file. In `cmd`, in the folder with the zip:

```
certutil -hashfile chatcli-v<version>-x86_64-pc-windows-msvc.zip SHA256
```

The hash printed must match the one in the `.sha256` file.

The exe is not code-signed, so Windows SmartScreen may warn ("Windows protected your PC"). Check the hash, then choose **More info > Run anyway** only if you trust it. Building from source (next section) remains the alternative.

## Build

You need Rust (stable), installed with [rustup](https://rustup.rs), and the Visual Studio C++ Build Tools that rustup asks for.

```
git clone https://github.com/Page011/chatCLI.git
cd chatCLI
cargo build --release
```

The program is `target\release\chatcli.exe`. Copy it anywhere on your `PATH`, or run it from there. The examples below assume `chatcli` is on your `PATH`.

Try it with no accounts:

```
chatcli --demo
```

Press `Ctrl+C` to leave the demo.

## Set up Teams

You register your own app in Microsoft Entra. chatcli then signs you in through your browser. No client secret is needed: chatcli is a public client and uses PKCE, so there is no secret to leak or rotate.

1. Open the [Microsoft Entra admin center](https://entra.microsoft.com) and sign in with your work or school account.
2. Go to **Entra ID > App registrations > New registration**. (Older menus: **Identity > Applications > App registrations**.)
3. Enter a name, for example `chatcli`.
4. Under **Supported account types**, choose **Single tenant only - <your tenant>** (older label: **Accounts in this organizational directory only**).
5. Leave **Redirect URI** empty. Select **Register**.
6. Open **Authentication**. On the **Redirect URI configuration** tab select **Add Redirect URI** (older layout: **Add a platform**), then **Mobile and desktop applications**.
7. Under **Custom redirect URIs**, enter `http://localhost`. Select **Configure**. (chatcli picks a free local port at sign-in. Entra ignores the port for `http://localhost`.)
8. Open **API permissions > Add a permission > Microsoft Graph > Delegated permissions**. Add all three:
   - `User.Read`
   - `Chat.ReadWrite`
   - `offline_access`
9. Leave the permissions as they are. In most tenants you approve them yourself the first time you sign in ("Not granted for ..." in the **Status** column is normal). If sign-in later says the app needs admin approval (`AADSTS65001` or "Need admin approval"), your tenant blocks user consent: ask a tenant admin to open this page and select **Grant admin consent for <tenant>**.
10. Open **Overview**. Copy **Application (client) ID** and **Directory (tenant) ID**. For the tenant you can also use a verified domain such as `contoso.onmicrosoft.com`. `common` and `organizations` are not accepted: chatcli signs in to one named tenant.

Save them in chatcli, then sign in:

```
chatcli teams setup <client-id> <tenant-id>
chatcli teams login
```

`teams login` opens your browser. If no browser opens, it prints a link: paste it into a browser. You can also skip `teams login`: start `chatcli`, open the Teams tab, and press `Enter`.

Check it works:

```
chatcli teams chats
```

That is all Teams needs. Sign-in, the chat list, reading and sending all go straight from your PC to Microsoft (login.microsoftonline.com and graph.microsoft.com). Cloudflare is not involved. Without a relay, chats load when chatcli starts, opening a chat fetches its newest messages, and `r` refreshes the chat list. What you do not get is new messages appearing on their own: the Teams tab shows NOT LIVE.

## Make Teams live with a relay

Microsoft allows polling the Teams APIs at most once a day. Live messages come from Graph change notifications, and Microsoft sends those only to a public HTTPS URL. A laptop has none. So you deploy a small Cloudflare Worker that receives the notifications and passes them to chatcli over a WebSocket.

- **What the relay sees:** ids only (subscription id, change type, chat and message ids). Message text never goes through it. chatcli fetches the text from Microsoft directly.
- **What it costs:** nothing on the Cloudflare Workers free plan. The Worker uses a SQLite-backed Durable Object, which the free plan allows.
- **How it hooks up:** after you sign in, chatcli itself asks Microsoft Graph for a change-notification subscription on your chats (`/users/<your id>/chats/getAllMessages`, ids only, no message text). It points at `<relay URL>/graph/notify` and `/graph/lifecycle` and uses your own sign-in (delegated `Chat.ReadWrite`, so no admin is needed). Graph checks the Worker answers, then sends new-message ids there. chatcli renews the subscription every 12 hours and deletes it when you quit.
- **If you skip it:** Teams still works. It just shows NOT LIVE and you press `r` to refresh.

You need a free [Cloudflare account](https://dash.cloudflare.com/sign-up), [Bun](https://bun.sh), [Node.js](https://nodejs.org) 22.12 or newer (wrangler needs it), and the `relay` folder from this repository. If you downloaded the release zip, also get the source: `git clone https://github.com/Page011/chatCLI.git`, or use **Code > Download ZIP** on GitHub and unzip it. Run every command below from the `chatCLI` folder.

1. Install the Worker's dependencies and sign in to Cloudflare. A browser window opens for the login.

   ```
   cd relay
   bun install
   bunx wrangler login
   ```

2. Deploy it.

   ```
   bunx wrangler deploy
   ```

   On a new Cloudflare account wrangler first says you need a workers.dev subdomain and asks `Would you like to register a workers.dev subdomain now?`. Type `y`, then choose a name (it becomes `https://<name>.workers.dev`, and is shared by all your Workers). A new subdomain can take a few minutes to start working.

   wrangler prints your Worker URL: `https://chatcli-relay.<your-subdomain>.workers.dev`. Copy it.

3. Tell chatcli the URL. This makes two random secrets, keeps them in Credential Manager, and prints them once. Run it in a terminal you trust. Do not paste the values anywhere except the next step.

   ```
   chatcli relay setup https://chatcli-relay.<your-subdomain>.workers.dev
   ```

4. Give the same two secrets to the Worker. Run these in the `relay` folder. Wrangler asks for each value: paste the value that step 3 printed.

   ```
   bunx wrangler secret put RELAY_TOKEN
   bunx wrangler secret put CLIENT_STATE
   ```

   Do not start chatcli until this step is done. Until the Worker has both values it refuses chatcli (NOT LIVE, `relay answered HTTP 503`).

5. Check the Worker is reachable. Open the address below in a browser. It should show `ok`. This does not check the secrets. Step 6 does. A new workers.dev subdomain can take a few minutes before this works.

   ```
   https://chatcli-relay.<your-subdomain>.workers.dev/health
   ```

6. Restart chatcli. The Teams tab goes from CONNECTING to LIVE after a few seconds.

To see what chatcli has stored for the relay, without showing secret values:

```
chatcli relay status
```

## Set up WhatsApp

1. Start `chatcli` and open the WhatsApp tab (`Tab`).
2. Press `Enter`. A QR code appears.
3. On your phone, open WhatsApp. Android: tap the three-dot menu, then **Linked devices > Link a device**. iPhone: **Settings > Linked devices > Link a device**. Scan the code.

If the terminal is too small for the QR code, chatcli says so: make the window bigger.

You can unlink at any time from the same **Linked devices** screen on your phone. chatcli then shows SIGN-IN NEEDED.

## Using it

Start it with `chatcli`.

| Key | Does |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous network |
| `Alt+1`, `Alt+2` | Jump to Teams, WhatsApp |
| `Up` / `Down`, or `k` / `j` | Move in the chat list |
| `Enter` | Open the chat and focus the message box |
| `Enter` (in the message box) | Send |
| `Esc` | Back to the chat list |
| `/` | Filter chats by name (`Enter` keeps the filter, `Esc` clears it) |
| `r` | Refresh the chat list (you trigger it; chatcli never polls) |
| `PgUp` / `PgDn` | Scroll messages |
| `q` | Quit (from the chat list) |
| `Ctrl+C` | Quit from anywhere |

The mouse works too: click a tab, click a chat, scroll the wheel. Unsent text is kept per chat. Times are local. Colours come from your terminal theme, and every status also has a text label.

### Status chips

Each tab shows a chip next to its name. In a narrow window the chip is shortened: `CONNECT`, `SIGN-IN`, `SIGNING`, `QR`.

| Chip | Meaning | What to do |
|---|---|---|
| `CONNECTING` | Starting up or reaching the network | Wait a few seconds. |
| `SIGN-IN NEEDED` | Not signed in, or the sign-in was rejected | Press `Enter` in the tab. The reason is shown in the panel. |
| `SIGNING IN` | Waiting for you to finish in the browser | Finish the Microsoft sign-in in the browser window. |
| `SCAN QR` | WhatsApp is waiting for you to scan | Scan the code from **Linked devices**. |
| `LIVE` | Connected, and new messages arrive on their own | Nothing. |
| `NOT LIVE` | Teams works but is not receiving live updates, or WhatsApp is reconnecting | Read the reason on the bottom line. See [Troubleshooting](#troubleshooting). Press `r` to refresh. |
| `ERROR` | Something failed | Read the message on the bottom line. See `chatcli.log`. |

A failed send stays in the chat marked `NOT SENT` with the reason. Nothing looks sent when it was not.

### Commands without the full screen

```
chatcli --demo                            the full screen with fake data
chatcli teams setup <client-id> <tenant>  save your Entra app ids
chatcli teams login                       sign in through the browser
chatcli teams chats                       list chats, newest first
chatcli teams messages <chat-id> [n]      last n messages (default 20, max 50)
chatcli teams send <chat-id> <text...>    send a message (asks you to confirm)
chatcli teams logout                      forget the stored Teams sign-in
chatcli relay setup <url>                 make the relay secrets for your Worker URL
chatcli relay status                      show the relay URL and whether its secrets exist
chatcli --version
```

Any other arguments print this list.

Command-line times are UTC.

## Your data

Everything stays on your machine, under `%LOCALAPPDATA%\chatcli`.

| File | Holds | Notes |
|---|---|---|
| `config.json` | Teams client id and tenant id, relay URL | Not secret. |
| `store.db` | Chat list and message text (plus `store.db-wal` and `store.db-shm` while open) | Plain, unencrypted SQLite. Kept until you delete it. No media. |
| `whatsapp.db` (and `whatsapp.db-wal`, `whatsapp.db-shm`) | WhatsApp linked-device keys | Anyone with this file can act as your linked device. Never copy, share or commit it. |
| `chatcli.log` | Connection, sign-in, relay and send events (older entries in `chatcli.log.1`) | Rolls to `chatcli.log.1` at 5 MB. Never holds tokens, secrets or message text. |
| `chatcli.lock` | Single-instance lock | Empty. |
| `secrets.lock` | Stops two chatcli processes writing Credential Manager at once | Empty. |

`CHATCLI_DATA_DIR` overrides the folder (used by the tests).

Secrets are in Windows Credential Manager (Control Panel > Credential Manager > Windows Credentials > Generic Credentials). Names end in `.chatcli`:

- `teams-refresh-token` (long tokens are split into numbered pieces: `teams-refresh-token-1`, ...)
- `relay-token`
- `relay-client-state`

### Remove everything

1. Quit chatcli (`q` or `Ctrl+C`). On quit it deletes its Teams subscription. One left by a crash expires on its own within 24 hours.
2. Sign out of Teams: `chatcli teams logout`
3. Delete the folder `%LOCALAPPDATA%\chatcli`.
4. In Credential Manager, delete every Generic Credential whose name ends in `.chatcli`.
5. On your phone, open **Linked devices** (as in WhatsApp setup, step 3) and log out chatcli.
6. Delete the Worker: Cloudflare dashboard > Workers & Pages > `chatcli-relay` > Settings > Delete. Or from the `relay` folder: `bunx wrangler delete`.
7. Delete the app registration: Entra admin center > App registrations > your app > Delete.
8. Delete `target\release\chatcli.exe`, or the whole `chatCLI` folder.

## Troubleshooting

**`chatcli is already running`.** Only one chatcli can run at a time, because two would fight over the Teams subscription and the relay connection. Close the other window. If you have none open, check Task Manager for `chatcli.exe`.

**`chatcli needs an interactive terminal`.** Run it from Windows Terminal or a console window, not from a pipe or a scheduled task.

**TLS or certificate errors on a work network.** chatcli trusts the public root certificates built into it plus the roots in the Windows certificate store, so a network that re-signs HTTPS with a company root installed in Windows normally works. One exception: when the `SSL_CERT_FILE` environment variable is set, chatcli reads extra roots from that file instead of the Windows store. If the file does not hold your company's root, connections fail with `UnknownIssuer` (chatcli adds a hint saying so). Unset it for chatcli:

```
set SSL_CERT_FILE=
chatcli
```

In PowerShell: `Remove-Item Env:SSL_CERT_FILE; chatcli`. If `bunx wrangler` fails with a certificate error on the same network, set `NODE_EXTRA_CA_CERTS` to a PEM file of your company root before running it.

**Sign-in fails with an `AADSTS` error.** The error text is shown in the Teams panel and written to the log. Common ones:

- `AADSTS65001` or "needs admin approval": a tenant admin must grant consent (Teams setup, step 9).
- `AADSTS50011` or a redirect URI error: check Teams setup, steps 6 and 7 (platform **Mobile and desktop applications**, URI `http://localhost`).
- `AADSTS700016` or "application not found": the client id or tenant id is wrong. Run `chatcli teams setup` again.
- `AADSTS7000218` ("must contain ... client_secret"): the `http://localhost` redirect was added under the **Web** platform. Remove it there and add it under **Mobile and desktop applications** (Teams setup, steps 6 and 7). Leave **Allow public client flows** off: chatcli does not need it.
- `AADSTS50020` ("user account ... does not exist in tenant"): you signed in with a personal Microsoft account or an account from another tenant. Pick your work or school account in the browser.
- `AADSTS90002` ("Tenant ... not found"): the tenant id is wrong. Copy **Directory (tenant) ID** again and run `chatcli teams setup` again.

The log is `%LOCALAPPDATA%\chatcli\chatcli.log`.

**Teams shows NOT LIVE.** The reason is on the bottom line:

| Reason | Fix |
|---|---|
| `relay not set up` | Run `chatcli relay setup <url>`, or ignore it: Teams works, press `r` to refresh. |
| `relay secrets missing: run chatcli relay setup` | Run `chatcli relay setup <url>` again, then redo both `wrangler secret put` steps. |
| `relay settings unreadable` | `config.json` or Credential Manager could not be read. The notice line says which. |
| `relay offline: relay answered HTTP 503` | The Worker does not have its secrets yet. Run both `wrangler secret put` steps, then restart chatcli. |
| `relay offline: relay answered HTTP 404` | The relay URL is wrong or the Worker was deleted. Check `chatcli relay status` and `/health`. |
| `relay offline: relay rejected the token (HTTP 401) ...` | The Worker's secrets do not match chatcli's. Run `chatcli relay setup <url>` again and put both new values on the Worker. |
| `relay offline: another chatcli took the relay connection ...` | A second copy is running, maybe on another machine. Close it, then press `r`. |
| `relay offline: ...` (anything else) | chatcli cannot reach the Worker. Check `/health` in a browser and your network. It retries on its own. |
| `Teams subscription failed: ...` | Microsoft refused the subscription. The text after the colon is Microsoft's reason. Most often the Worker was not reachable when Graph checked it. chatcli retries on its own. |
| `connecting to relay`, `no Teams subscription yet` | Normal for a few seconds after start. If it lasts, read the log. |

**The Worker shows an alert about a bad `clientState`.** Something sent the Worker a notification with the wrong secret. The Worker dropped it. If it keeps happening, redo the relay setup.

## Architecture

See [docs/DESIGN.md](docs/DESIGN.md): the crates, the relay protocol, the data layout, and the failure checks.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) to build, test and send a change. Report security problems privately, as described in [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
