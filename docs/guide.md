# chatcli

[![CI](https://github.com/Page011/chatCLI/actions/workflows/ci.yml/badge.svg)](https://github.com/Page011/chatCLI/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Page011/chatCLI)](https://github.com/Page011/chatCLI/releases)
[![License](https://img.shields.io/github/license/Page011/chatCLI)](https://github.com/Page011/chatCLI/blob/main/LICENSE)
[![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-4c1d95)](https://github.com/Page011/chatCLI/blob/main/docs/guide.md#linux-and-macos-experimental)

A terminal chat client for WhatsApp and Microsoft Teams.

Right now you can host everything yourself. 
 - Azure Registration for Teams
 - Cloudflare worker for Teams

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
│    Jordan Placeholde…││09:10 Alex Example  is the build done? 🚀            │
│    田中太郎 05/11/26  ││                    今日はどうですか                   │
│    了解しました       ││09:15 You           yes, shipped                      │
│    Jordan P… 09/10/26││09:30 You         ◷ on it, will confirm shortly       │
│    You: thanks       ││09:30 You           this one will fail                │
│                      ││                    NOT SENT - demo: this chat refuses│
│                      ││                    messages                          │
│                      ││                                                      │
│                      │└──────────────────────────────────────────────────────┘
│                      │╔ Message ═════════════════════════════════════════════╗
│                      │║> typing a draft                                      ║
└──────────────────────┘╚══════════════════════════════════════════════════════╝

Enter send  F1 help  Esc back  Ctrl+J newline  Ctrl+E emoji  Ctrl+O attach
```

## Read this first

- **Early release.** chatcli works day to day but has rough edges. Please [open an issue](https://github.com/Page011/chatCLI/issues) when something breaks. Live Teams updates through the relay are the newest and least tested part.
- **WhatsApp support is unofficial.** chatcli links to your account as a WhatsApp Web linked device, using [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust). WhatsApp's terms forbid unofficial clients. WhatsApp can suspend an account that uses one. Use it for personal, low-volume chatting only. Never automate sending. You accept this risk.
- **Teams needs a work or school account.** Microsoft Graph chat APIs do not work with personal Microsoft accounts. Your tenant must also allow you to register an app (or an admin must do it for you).
- **Windows 10 or 11 is the tested platform.** Secrets are kept in Windows Credential Manager. Linux and macOS are experimental: see [Linux and macOS](#linux-and-macos-experimental).
- **Scope.** Teams 1:1 and group chats (no channels, no meeting chats). WhatsApp direct and group chats. Text, replies, reactions, mentions, edit and delete, pictures and files. Images are not downloaded automatically.
- **Not every feature is checked against a live account yet.** Anything that talks to the real services (reactions, edits, files, older history, toasts) is built and unit tested, but the release notes say what has been tried by hand.

## Download

Get `chatcli-v<version>-x86_64-pc-windows-msvc.zip` from [GitHub Releases](https://github.com/Page011/chatCLI/releases). Unzip it and run `chatcli.exe`, or put it on your `PATH`.

Verify the download before you run it. The release also has a `.sha256` file. In `cmd`, in the folder with the zip:

```
certutil -hashfile chatcli-v<version>-x86_64-pc-windows-msvc.zip SHA256
```

The hash printed must match the one in the `.sha256` file.

The exe is not code-signed, so Windows SmartScreen may warn ("Windows protected your PC"). Check the hash, then choose **More info > Run anyway** only if you trust it. Building from source (next section) remains the alternative. Why it is not signed, and what the options are: [docs/code-signing.md](code-signing.md).

## Build

You need Rust (stable), installed with [rustup](https://rustup.rs), and a C compiler: on Windows the Visual Studio C++ Build Tools that rustup asks for, on macOS the Xcode command line tools (`xcode-select --install`), on Linux `gcc` or `clang` (Debian/Ubuntu: `sudo apt install build-essential`).

```
git clone https://github.com/Page011/chatCLI.git
cd chatCLI
cargo build --release
```

The program is `target\release\chatcli.exe` on Windows, `target/release/chatcli` on Linux and macOS. With Nix, `nix build` puts it in `result/bin/chatcli` instead. Copy it anywhere on your `PATH`, or run it from there. The examples below assume `chatcli` is on your `PATH`.

Try it with no accounts:

```
chatcli --demo
```

Press `Ctrl+C` to leave the demo. The demo shows every feature below on fake data: replies, reactions, edits, mentions, attachments and thumbnails, older history, pin/mute/hide, search, typing, and a footer line where a toast would be. It starts from a copy of your `settings.toml` (so you can try a theme or a layout), keeps its messages, its downloads and that copy in a temporary folder that is deleted on exit, and never touches your real data, your real `settings.toml` or the network.

Want it shown instead of reading about it? `chatcli --demo --tour` plays a guided tour: it signs in, opens a chat, sends, replies, reacts, edits, deletes, searches, opens the settings screen and rebinds a key, with a caption on the top row. `Space` pauses, `n` skips to the next step, `Esc` ends it (`:tour` in the demo plays it again). The tour starts from the default settings and puts everything back.

## Set up Teams

You register your own app in Microsoft Entra. chatcli then signs you in through your browser. No client secret is needed: chatcli is a public client and uses PKCE, so there is no secret to leak or rotate.

Setup takes about 15 minutes. By the end you will have written down:

- [ ] **Application (client) ID** from your app registration's Overview page
- [ ] **Directory (tenant) ID**, or your verified domain such as `contoso.onmicrosoft.com`
- [ ] **Worker URL** (only if you want live messages; see [Make Teams live with a relay](#make-teams-live-with-a-relay))

You enter the first two with `chatcli teams setup <client-id> <tenant>`, and the URL with `chatcli relay setup <url>`.

1. Open the [Microsoft Entra admin center](https://entra.microsoft.com) and sign in with your work or school account.
2. Go to **Entra ID > App registrations > New registration**. (Older menus: **Identity > Applications > App registrations**.)
3. Enter a name, for example `chatcli`.
4. Under **Supported account types**, choose **Single tenant only - <your tenant>** (older label: **Accounts in this organizational directory only**).
5. Leave **Redirect URI** empty. Select **Register**.
6. Open **Authentication**. On the **Redirect URI configuration** tab select **Add Redirect URI** (older layout: **Add a platform**), then **Mobile and desktop applications**.
7. Under **Custom redirect URIs**, enter `http://localhost`. Select **Configure**. (chatcli picks a free local port at sign-in. Entra ignores the port for `http://localhost`.)
8. Open **API permissions > Add a permission > Microsoft Graph > Delegated permissions**. Add all four:
   - `User.Read`
   - `Chat.ReadWrite`
   - `ChatMessage.Send`
   - `offline_access`

   `ChatMessage.Send` is the only permission Microsoft lists for replying with a quote ([permissions reference](https://learn.microsoft.com/graph/permissions-reference)). It was added in 0.2: after upgrading, log out of Teams and sign in again. Until you do, everything works except replies, which say `REPLY NEEDS ChatMessage.Send, sign in again`.
9. Leave the permissions as they are. In most tenants you approve them yourself the first time you sign in ("Not granted for ..." in the **Status** column is normal). If sign-in later says the app needs admin approval (`AADSTS65001` or "Need admin approval"), your tenant blocks user consent: ask a tenant admin to open this page and select **Grant admin consent for <tenant>**.
10. Open **Overview**. Copy **Application (client) ID** and **Directory (tenant) ID**. For the tenant you can also use a verified domain such as `contoso.onmicrosoft.com`. `common` and `organizations` are not accepted: chatcli signs in to one named tenant.

Save them in chatcli, then sign in:

```
chatcli teams setup <client-id> <tenant-id>
chatcli teams login
```

`teams login` opens your browser. If no browser opens, it prints a link: paste it into a browser. You can also skip `teams login`: start `chatcli`, open the Teams tab, and press `Enter`.

Sign-in redirects the browser back to a port on the same computer that runs chatcli (`http://localhost:<port>`). Sign in from a browser on that computer. Over Remote Desktop it works if you open the browser inside the remote session. Over SSH, or with the browser on another machine, the redirect fails unless you forward that port, which changes every time.

Check it works:

```
chatcli teams chats
```

That is all Teams needs. Sign-in, the chat list, reading and sending all go straight from your PC to Microsoft (login.microsoftonline.com and graph.microsoft.com). Cloudflare is not involved. Without a relay, chats load when chatcli starts, opening a chat fetches its newest messages, and `r` refreshes the chat list. What you do not get is new messages appearing on their own: the Teams tab shows NOT LIVE.

### Optional file permissions

Two more permissions are off by default. chatcli asks for them only when you switch them on in `settings.toml`:

```toml
[teams]
files_read = true    # adds Files.Read.All: files linked in messages can be downloaded with `o`
files_write = true   # adds Files.ReadWrite: Ctrl+O can send a file by uploading it to your OneDrive
```

Add the permission under **API permissions** as in step 8, switch the setting on, then log out of Teams and sign in again. Microsoft lists both delegated permissions as not needing admin consent ([permissions reference](https://learn.microsoft.com/graph/permissions-reference)), but a tenant can still block them: if sign-in then asks for an admin, set both back to `false`.

- Without `files_read`, `o` on a linked file opens it in the browser.
- An uploaded file goes to the folder `Microsoft Teams Chat Files` in your OneDrive and stays there. The recipient needs access to it: chatcli does not change sharing settings.
- Files over 100 MB are refused before any upload, and pictures over 4 MB before any request. Pictures themselves need no extra permission.

### What Teams can and cannot do here

- Edit works on your own text messages. A message with a quote or a file must be edited in Teams.
- Pin and mute are local to chatcli on Teams. Hide is also done in Teams.
- Reactions: Microsoft does not document which emoji Teams accepts. A refused one shows Teams' reason in the notice line.
- Older history loads one page of 50 per scroll past the top. chatcli never polls Teams.
- Teams shows no typing indicator because Microsoft Graph has none for chats.
- Teams shows no read ticks or delivery crosses: Microsoft Graph only says how far you have read, not other people.

## Make Teams live with a relay

Microsoft allows polling the Teams APIs at most once a day. Live messages come from Graph change notifications, and Microsoft sends those only to a public HTTPS URL. A laptop has none. So you deploy a small Cloudflare Worker that receives the notifications and passes them to chatcli over a WebSocket.

- **What the relay sees:** ids only (subscription id, change type, chat and message ids). Message text never goes through it. chatcli fetches the text from Microsoft directly.
- **What it costs:** nothing on the Cloudflare Workers free plan. The Worker uses a SQLite-backed Durable Object, which the free plan allows.
- **How it hooks up:** after you sign in, chatcli itself asks Microsoft Graph for a change-notification subscription on your chats (`/users/<your id>/chats/getAllMessages`, ids only, no message text). It points at `<relay URL>/graph/notify` and `/graph/lifecycle` and uses your own sign-in (delegated `Chat.ReadWrite`, so no admin is needed). Graph checks the Worker answers, then sends new-message ids there. chatcli renews the subscription every 12 hours and deletes it when you quit.
- **If you skip it:** Teams still works. It just shows NOT LIVE and you press `r` to refresh.

The release zip holds only `chatcli.exe`. The `relay` folder is in the source: run `git clone https://github.com/Page011/chatCLI.git`, or use **Code > Download ZIP** on GitHub and unzip it. Every command in this section runs from that `chatCLI` folder.

You need a free [Cloudflare account](https://dash.cloudflare.com/sign-up), [Bun](https://bun.sh), [Node.js](https://nodejs.org) 22.12 or newer (wrangler needs it), and the `relay` folder from the source.

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

3. Tell chatcli the URL. This makes two random secrets, keeps them in Credential Manager (or the OS credential store on Linux and macOS), and prints them once. Run it in a terminal you trust. Do not paste the values anywhere except the next step.

   ```
   chatcli relay setup https://chatcli-relay.<your-subdomain>.workers.dev
   ```

   Run it once. Running it again stops with a message instead of replacing working secrets (a replaced secret would break live Teams with a 401). To make new secrets, run `chatcli relay setup <url> --rotate`, then redo both `wrangler secret put` steps. To change only the URL, run `chatcli relay url <url>`: the secrets are kept.

   The two secrets stay in your terminal's scrollback and history. After you have pasted both into step 4, clear the screen (`cls`) or close the terminal window.

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

- Replies, reactions, mentions, edit and delete work on WhatsApp. Edit and delete only work for a short time after sending: WhatsApp decides, and its refusal is shown as a notice with the text unchanged. Mentions work in groups only.
- **Older messages:** scrolling past the top asks your phone for 50 older messages. The phone must be online; after 30 seconds without an answer chatcli shows `PHONE DID NOT ANSWER` and you can scroll again. Very old messages may be unavailable.
- **Media** is downloaded when you ask, into the media cache (not encrypted on disk). Pictures from others in the 20 newest messages of a chat you open also download by themselves and show as thumbnails (not files, and nothing over `max_file_mb`); `[whatsapp] download_images = false` turns that off. WhatsApp deletes old media from its servers: a file that is gone shows `MEDIA EXPIRED`; ask the sender to send it again.
- **Sending files:** pictures (PNG, JPEG, GIF as a still, WebP) up to 16 MB are re-encoded, which removes EXIF data such as location. Other files up to 100 MB are sent as documents. WhatsApp can flag media sent by unofficial clients: keep sends manual and low volume.
- **Typing:** you see when someone is typing. chatcli sends your own typing state unless `[whatsapp] send_typing = false` (default `true`), at most once per 5 seconds per chat.
- **Read state:** reading a chat, or sending in it, clears the unread mark on your phone and your other linked devices. This happens with read receipts on or off. Read receipts (blue ticks for the sender) are sent unless `[whatsapp] send_read_receipts = false` (default `true`).
- **Message status:** a column to the left of your messages shows how far they have got. The marks differ in shape, so they read the same without colour (change them in Settings, Appearance, or see Themes):
  - `◷` still sending: chatcli has not handed it to the network yet (Teams and WhatsApp). A file also shows how far, as `[sending 45%]`.
  - `✓` your newest message that has been read, along with everything before it. Older messages carry no mark.
  - `●` a later message that has reached their phone but is not read yet.
  - `○` a later message that has not reached their phone yet (see below).

  In a group a message counts as read, or delivered, once every member has it. A voice note counts as read when it is seen, like any message, whether or not it was played. In a 1:1 chat WhatsApp only sends these when both of you have read receipts on; in groups it sends them anyway.
- **Waiting for their phone:** a `○` beside one of your messages means it was sent but has not reached the other person's phone yet (in a group: every member's phone), or for a moment just after sending; a longer wait usually means their phone is off or offline. Nothing is wrong on your side. It changes to `●` when the message arrives. Messages from before you upgraded to this version never show one, and neither does your chat with yourself.
- **Pin, mute (for good) and hide (archive)** sync with the phone; the phone wins on a conflict.

## Using it

Start it with `chatcli`. Press `?` for a help screen generated from the keys that are active right now (so it is always right, also after you rebind keys). Quit with `Ctrl+C` or `:quit`, not by closing the window: chatcli deletes its Teams subscription as it quits.

### First run

- Press `:` for the command bar: every command, and your chats. `:settings` opens the settings screen, `:keys` the key bindings, `:help` (or `?`) the help screen.
- Everything in the settings screen applies at once and is saved: theme and colours, layout, notifications, media limits, Teams file permissions, WhatsApp privacy options and every key binding.
- chatcli writes `settings.toml` itself from that screen, one key at a time, and keeps your comments. Editing the file by hand is optional and still works (restart chatcli to read a hand edit).
- To watch it all first: `chatcli --demo --tour` (fake data, no accounts, nothing saved to your real files).

### Keys

These are the default keys. Your own bindings are below. The same keys mean the same thing everywhere they work: `j` / `k` move in the chat list, in message selection, in lists and in help; `Enter` goes in or accepts, `Esc` goes back out.

| Key | Does |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous network tab. Signed-in networks come first (Teams before WhatsApp when both are), the ones waiting for a sign-in after. chatcli starts on the tab you used last |
| `Alt+1`, `Alt+2` | Jump to the first, second tab as shown |
| `?` or `F1` | Help (`?` types a question mark in the boxes you type in; `F1` works there too). `Esc` closes it |
| `F2` | Settings screen (`:settings`): theme, layout, notifications, media, Teams, WhatsApp and every key binding. `Up`/`Down` move, `Left`/`Right` change, `Enter` edits a number or colour, `r` resets, `Tab` switches between the section list and the rows, `Esc` closes |
| `:` | The command bar: type to list commands and chats, `Enter` runs or opens the highlighted one, `Esc` closes it |
| `/` | Filter the chat list by name: `Enter` keeps the filter, `Esc` clears it |
| `Ctrl+F` | [Search](#search) every stored message |
| `Up` / `Down`, `k` / `j` | Move |
| `g` / `G`, `Home` / `End` | To the top / the bottom |
| `H` / `L` | To the first / last chat or message on screen |
| `PgUp` / `PgDn` | A page up / down |
| `Ctrl+C` | Quit from anywhere (also `:quit`) |

Chat list:

| Key | Does |
|---|---|
| `Up` / `Down`, `k` / `j` | Move in the chat list. The chat under the cursor shows in the message pane at once (a preview) |
| `Enter`, `l` or `Right` | Into the chat: its newest message is selected (see message selection below). A chat still loading opens in the message box and selects its newest message when it arrives, unless you pressed a key first |
| `i` | Straight to the chat's message box |
| `Esc` | Clear the filter; with no filter, close the chat in the message pane |
| `r` | Refresh the chat list (you trigger it; chatcli never polls) |
| `n` / `N` | Next / previous chat with unread messages (wraps; muted chats are skipped) |
| `R` | Mark the chat read. `:readall` marks every chat of this network read, after a `y/n` |
| `p` | Pin or unpin the chat (pinned chats sort first) |
| `m` | Mute (for good) or unmute: no toast, not counted in the tab total |
| `z` | Hide or show the chat. `Z` shows or hides the chats you hid |

Message box:

| Key | Does |
|---|---|
| `Enter` | Send the whole message |
| `:command` | At the start of the box, `:` opens a list: the matching commands first (`:help`, `:settings`, `:keys`, `:signout`), then the matching emoji. `Enter` takes the highlighted item: a command runs, an emoji goes in. A space, punctuation or `Esc` closes the list, and the text stays text, so `:help` then a space or `Esc` and `Enter` sends `:help`. Only a lower-case word that is a whole command name (`:q`) or at least two letters of one opens commands, so `:P` and `:D` are never commands. Commands take no arguments here: use the command bar (`:theme dark`) |
| `Esc` | Cancel a pending reply or edit; otherwise to message selection (to the chat list when the chat has no message yet) |
| `Left` / `Right`, `Ctrl+Left` / `Ctrl+Right` | Move the cursor by character, by word. These editing keys, down to `Ctrl+U`, work the same in every box you type in: the chat filter, the command bar, search, the emoji search and the file prompt (in the emoji picker `Left` and `Right` move through the grid) |
| `Home` / `End` | Start / end of the line |
| `Backspace` / `Delete` | Delete a character (an emoji goes whole) |
| `Ctrl+W` or `Ctrl+Backspace` | Delete the word before the cursor |
| `Ctrl+U` | Clear the line the cursor is on |
| `Shift+Enter`, `Ctrl+J` or `Alt+Enter` | Add a line break. Shift+Enter works on Windows and in terminals with the kitty keyboard protocol (kitty, foot, Ghostty, Alacritty and others), which chatcli turns on when the terminal offers it; elsewhere the terminal sends it as Enter. `Ctrl+J` works in every terminal. The footer shows Shift+Enter where it works, else Ctrl+J. Alt+Enter is Windows Terminal's fullscreen key and never reaches chatcli there |
| `PgUp` / `PgDn` | Scroll messages. At the top, `PgUp` loads older ones |
| `Ctrl+E` | Emoji picker |
| `Ctrl+O` | Attach a file |
| `Alt+V` or `Ctrl+Shift+V` | Attach the picture on the clipboard (Windows Terminal keeps `Ctrl+Shift+V` for its own text paste, so use `Alt+V` there) |

Message selection (`Enter` on a chat, or `Esc` from the message box). The footer lists the actions: the ones that apply are bold, the rest are dimmed (plain text in the high-contrast theme):

| Key | Does |
|---|---|
| `Up` / `Down`, `k` / `j`, `PgUp` / `PgDn`, `g` / `G`, `Home` / `End` | Move the selection. Up from the oldest message held loads older ones. With the newest message selected, a new one takes the selection with it |
| `H` / `M` / `L` | The first / middle / last message on screen |
| `Ctrl+O` | Back to the message selected before the last big jump (`G`, `g`, `H`, `M`, `L`); again for the one before |
| `i` | To the message box |
| `Esc`, `h` or `Left` | Back to the chat list |
| `r` | Reply: quote this message |
| `e` | React: opens the emoji picker. Choose the emoji you already used to remove it |
| `c` | Change (edit) your own message (your unsent draft comes back afterwards) |
| `d` or `x` | Delete your own message for everyone, after a `y/n` |
| `u` | Open a link from this message |
| `o` | Open the attachment (downloads first if needed) |
| `v` | Show or hide a thumbnail of an image |
| `y` | Copy the message's text (for an attachment with no caption, its file name). chatcli uses the system clipboard (on Linux the Wayland one natively, on wlroots compositors and KDE) and also asks the terminal to copy it (OSC 52), which is what works over SSH; some terminals must be told to allow that |

Popups and pickers take `Enter` (not `Tab`) for the chosen item. `open_link` in the chat list (a link from the newest message) and `select_messages` in the message box (selection even while a reply is pending) have no default key: bind one if you want them.

The mouse works too: click a tab, click a chat (previews it, like moving the cursor), scroll the wheel. A preview does not mark the chat read: going into the chat, sending, or two seconds on it does. Unsent text is kept per chat. Times are local.

### Writing

- **Paste** inserts at the cursor and never sends. Line breaks stay, control characters are dropped, tabs become four spaces. Enter sends the whole multi-line message. Windows Terminal and the classic console have no bracketed paste, so Windows delivers a paste as key presses: chatcli treats several key presses that arrive together as a paste (line breaks stay in the message box, nothing is sent), and ignores such a burst when no text box is open. A terminal that does neither would still send each pasted line: test with a three-line paste before trusting it.
- **Too long:** over the limit (Teams 80 KB of escaped HTML, WhatsApp 60000 characters) the message is not sent and the text stays.
- **Emoji:** type `:thu` and `Enter` inserts one (`Up` / `Down` choose, `Esc` closes). It does not open inside `10:30` or `https://x`. `Ctrl+E` opens a picker with categories, search (type) and your last 30 emoji. Click or the wheel work in it. `[layout] emoji_picker` (Settings > Layout > Emoji picker) sets where it opens: `popup` in the middle of the screen (the default), `box` straight above the message box with the newest messages still in view, or `inline` as one row inside the message box under your draft (`Left` / `Right` move, typing filters, the search shows the category while empty). Where there is no room for `box` or `inline`, it opens in the middle. The reaction picker always opens in the middle.
- **Mentions:** in a group chat type `@` and a letter or two, then `Up` / `Down` and `Enter`. `Backspace` right after a name removes it whole. Chats where you were mentioned show `@` after the unread count, and those messages are bold. Not offered in one-to-one chats.
- **Replies** show the quoted message dimmed above the reply. An edited message shows `(edited)`, a deleted one `[message deleted]`. Reactions show as counts under a message, yours in bold.
- **Links** are underlined. `u` asks `Open <host>? y/n` (the real host, whatever the link text says); several links get a `1`-`9` choice first. Only `http` and `https` open.

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
chatcli --demo [--tour]                   the full screen with fake data; --tour plays a guided tour
chatcli teams setup <client-id> <tenant>  save your Entra app ids
chatcli teams login                       sign in through the browser
chatcli teams chats                       list chats, newest first
chatcli teams messages <chat-id> [n]      last n messages (default 20, max 50)
chatcli teams send <chat-id> <text...>    send a message (asks you to confirm)
chatcli teams logout                      forget the stored Teams sign-in
chatcli relay setup <url> [--rotate]      make the relay secrets (refuses to replace working ones)
chatcli relay url <url>                   change the relay URL, keep the secrets
chatcli relay status                      show the relay URL and whether its secrets exist
chatcli settings path                     print where settings.toml lives
chatcli settings check                    list every problem in settings.toml (exit 1 on an error)
chatcli doctor [--online] [--json]        check the setup and print fixes (exit 1 on a failure)
chatcli search <text...>                  search every stored message
chatcli media clear                       delete the downloaded media cache
chatcli --version
```

`chatcli --help` prints this list; any unknown arguments print it too. Commands that change the sign-in or the media cache (`teams ...`, `relay setup`, `relay url`, `media clear`) refuse to run while the full client is open: quit it first.

Command-line times are UTC.

## Settings

The settings screen (`F2` or `:settings`) edits this file for you. Editing it by hand is optional and still works. chatcli writes `settings.toml` only from that screen, one key at a time, keeping your comments and unknown keys; a file that does not parse is never written (the screen says `NOT SAVED` and the change applies for this session only: fix the file first). The commands write `config.json`: Teams ids and the relay URL.

- Where: `chatcli settings path` (by default `%LOCALAPPDATA%\chatcli\settings.toml`).
- Changes made in the settings screen apply at once. A hand edit is read at start only: restart chatcli after it. Two settings apply later: the image protocol (next start) and the Teams file permissions (`:signout`, then `:signin`).
- No file means the defaults.
- A broken section falls back to its defaults and shows one notice with the file, line and key. The other sections still load. An unknown key gives one warning.
- `chatcli settings check` prints every problem and exits 1 on any error, 0 on warnings only. `chatcli doctor` counts them too.

Every section and key, with its default:

```toml
[layout]
sidebar_width = 30          # columns 16-120, or a string "10%" to "70%". Default: 30% of the
                            # width, between 28 and 40 columns. The message pane keeps 30 columns
time_format = "24h"         # "24h" or "12h" (09:30 AM)
date_order = "dmy"          # "dmy", "mdy" or "ymd" for dates older than this week
density = "cozy"            # "cozy" or "compact" (compact: one row per chat)
show_previews = true        # false: one row per chat, no preview line
emoji_picker = "popup"      # where Ctrl+E opens: "popup" (middle of the screen), "box" (above the
                            # message box) or "inline" (one row inside the message box)

[notifications]
enabled = true              # desktop notification for a new message (Windows and Linux)
preview = "full"            # "full": "Name: text" (cut to 120 characters); "sender": "Name: new message";
                            # "off": chat title + "New message"
sound = true                # Linux: asks the daemon for the "message-new-instant" sound

[media]
cache_max_mb = 200          # 1-100000. Oldest files go first when the cache is over this
retention_days = 30         # 1-3650
max_file_mb = 25            # 1-2048, not above cache_max_mb. A bigger file is not downloaded
inline_images = true        # false: no thumbnails (the attachment chips stay)
image_protocol = "auto"     # "auto", "sixel", "kitty", "iterm2", "halfblocks" or "off"

[teams]
files_read = false          # Files.Read.All: download linked files (see Optional file permissions)
files_write = false         # Files.ReadWrite: send files through your OneDrive
photos = false              # User.ReadBasic.All: profile photos in avatars (needs no admin; sign in again)

[whatsapp]
send_read_receipts = true   # blue ticks for the sender when you read a chat
send_typing = true          # tell the other side when you type (showing theirs is always on)
download_images = true      # pictures from others in the newest 20 messages of an opened chat load as thumbnails

# [keys.list] and the other context tables: see Key bindings

[theme]
name = "default"            # "default", "high-contrast", "light" or "dark"
# role = colour overrides: see Themes
```

### Key bindings

The `:keys` page (or the Keys section of `F2`) lists every action with its keys: `/` filters it by an action's name, its `settings.toml` name, a key (`ctrl+o`) or a context (`chat list`), `Enter` keeps the filter and `Esc` clears it. `Enter` on an action, then press the new key. Every key you press is taken, `Esc`, `Backspace` and `Ctrl+C` included: no key cancels (press the key the action already has to leave it as it is, or delete the new key afterwards). If the key already runs another action you are asked whether to swap. An action can have several keys: `a` adds one and keeps the others (a key another action of the same context holds is refused with its name, and a key that would hide or be hidden by an `Everywhere` key, or a two-key sequence, asks first), and `d` lets you choose one of its keys with `Left` / `Right` and deletes it with `Enter`. The key you set last is the one the footer shows. `r` resets one action, and the last row resets them all. `Ctrl+C` and `Esc` are ordinary keys, but a change that would leave nothing to quit with somewhere, or nothing to close help, a popup or a `y/n` question with, is refused. What follows is the same thing as a file.

Your own bindings sit in one table per context, `key = "action"`, over the built-in keys:

```toml
[keys.list]
"ctrl+n" = "down"
"ctrl+p" = "up"
"z" = "unbound"        # removes the built-in binding of z (hide)

[keys.composer]
"ctrl+s" = "send"
```

- **Key strings:** a single character (`a`, `G`, `/`), or `enter` `esc` `tab` `up` `down` `left` `right` `home` `end` `pgup` `pgdn` `backspace` `delete` `insert` `space` `f1` to `f24`, with `ctrl+`, `alt+` or `shift+` in front (`shift+tab`).
- **Contexts:** `common` (see below), `panel` (sign-in), `list` (chat list), `filter`, `composer` (message box), `select` (message selection), `popup` (suggestions, emoji picker, file prompt, search box, command bar), `confirm` (the `y/n` questions), `help`.
- **`[keys.common]`** binds a key once for every context where its action makes sense: quitting everywhere; the tabs, help, settings, search and the command bar outside the overlays; the filter in the chat list; moving, `accept` and `back` in the chat list, message selection, the lists and help; the editing keys in every box you type in; any other action where chatcli itself gives it a key. Most default keys live here. A context's own table wins over `common`, `"key" = "unbound"` in a context's own table stops a `common` key there, and a `common` key that types a character (a letter, `?`, `/`, `:`) is skipped in the boxes you type in, so it never stops you typing.
- **Text contexts** (`filter`, `composer`, `popup`) refuse plain letters and `Backspace` in their own tables, because that would stop you typing. Use `ctrl+` or `alt+`, or `[keys.common]`.
- **Ways out:** `Ctrl+C` (`quit`) and `Esc` (`back`) are ordinary bindings: a context can give `Ctrl+C` to another action, or you can move `quit` to another key. chatcli only checks that some key still quits from every context (a `common` key that types a character does not count in the boxes you type in), and that `popup`, `confirm` and `help` each keep a key that backs out (`confirm_no` counts in `confirm`). If not, `Ctrl+C` quits and `Esc` backs out in the contexts named by the notice until you bind one.
- A bad binding (unknown action, unreadable key, the same key twice in one table) is skipped with one notice, and the rest load. The notice suggests the closest name. Some terminals send `Ctrl+I` as `Tab`, `Ctrl+M` and `Ctrl+J` as `Enter`, `Ctrl+[` as `Esc` and `Ctrl+H` as `Backspace`: these are warned about and still applied.
- Two-key sequences (like `gg`) are not supported in your own bindings.

Actions (the names are stable): `quit` `next_network` `prev_network` `goto_network_1` `goto_network_2` `help` `up` `down` `top` `bottom` `screen_top` `screen_middle` `screen_bottom` `page_up` `page_down` `accept` `back` `compose` `filter_start` `refresh` `send` `cursor_left` `cursor_right` `word_left` `word_right` `line_start` `line_end` `delete_back` `delete_forward` `delete_word_back` `clear_line` `insert_newline` `emoji_picker` `open_link` `confirm_yes` `confirm_no` `jump_back` `select_messages` `reply` `cancel_reply` `react` `edit` `delete` `open_media` `copy` `view_image` `attach` `paste_image` `next_unread` `prev_unread` `mark_read` `mark_all_read` `pin` `mute` `hide` `show_hidden` `search` `command_bar` `settings`, and `unbound`. `up`, `down`, `top`, `bottom`, `screen_top`, `screen_middle`, `screen_bottom`, `page_up`, `page_down`, `accept` and `back` do what they mean where they are pressed: `down` is the next chat in the list, the next message in selection, the next suggestion in a popup, and scrolls help; `accept` goes into a chat (its newest message selected), signs in, keeps the filter or takes a suggestion; `back` goes from the message box to selection and from selection to the chat list, closes the open chat, help or a popup, clears the filter, or answers no. `compose` goes to the message box. The `?` screen lists every action next to its keys. The footer hints follow your bindings too. A binding on an action that makes no sense in that context is accepted and does nothing.

### Themes

All colours are ANSI 16 by default, so your terminal's own light or dark theme decides, and every status also has a text label. `[theme] name` picks `default` (today's look), `high-contrast`, `light` or `dark`. You can override single roles:

```toml
[theme]
name = "high-contrast"
own_name = "#00aa00"
```

A value is an ANSI name (`black` `red` `green` `yellow` `blue` `magenta` `cyan` `gray` `dark_gray` `white` and the `light_` variants), a number 0-255, or `#rrggbb`. An override replaces only the colour: the role keeps its bold or reverse. Numbers from 16 up and `#rrggbb` are fixed colours that do not follow your terminal's palette and look wrong without 24-bit colour support. A bad value shows one notice naming the role and the theme's own colour is used.

Roles: `text` (messages, chat titles, your draft), `dim` (times, previews, separators), `hint` (key hints, empty-state notes), `accent` (SIGNING IN and SCAN QR chips, "is typing"), `own_name` (your name), `unread` (counts and unread titles), `error`, `warn`, `ok` (send progress), `selected` (selected tab, row or message), `chip_live`, `chip_attention` (LIVE and SIGN-IN NEEDED / NOT LIVE chips), `network_teams`, `network_whatsapp` (tab names).

The message status marks are `[theme]` keys too: `mark_sending` (default `◷`), `mark_sent` (`○`), `mark_delivered` (`●`) and `mark_read` (`✓`). Each is one character one column wide (an emoji takes two and is refused, keeping the default). A space (`" "`) means no mark for that state: set all four to a space to hide the marks altogether. Two the same are kept, with a warning (except spaces): you could no longer tell those states apart.

```toml
[theme]
mark_sent = "◌"
mark_read = "✔"
```

`NO_COLOR` set to any non-empty value removes all colour (bold, dim and reverse stay). Two exceptions: the WhatsApp sign-in QR stays black on white so a phone can scan it, and thumbnails are switched off (the attachment chips stay).

### Media

Nothing downloads until you press `o` or `v` on a message, except WhatsApp pictures: those from others in the 20 newest messages of the open chat download by themselves (`[whatsapp] download_images`, see WhatsApp above). `o` fetches the attachment into the media cache and opens it in the default app. `v` shows a thumbnail under the message.

- The cache is `media\<network>\` in the data folder, files named by a hash, never by the sender's file name. It is capped by `cache_max_mb` and `retention_days` (oldest files go first, checked at start and after each download). A file over `max_file_mb` is not downloaded: you get a notice that names the setting. `chatcli media clear` empties it.
- **Cached files are not encrypted on disk.** Use full-disk encryption.
- Executable file types are never opened (`BLOCKED: executable`). Sending one asks `Send EXECUTABLE <name>? y/n` first.
- Thumbnails are half-block characters, at most 40x20 cells, scaled to the pane. With `image_protocol = "auto"` chatcli asks the terminal once at start (200 ms) whether it draws Sixel, Kitty or iTerm2 graphics and, if so, draws the sharper picture over a fully visible thumbnail. It never uses graphics inside tmux or screen. A terminal that does not answer gets half-blocks; `"sixel"` and friends force a protocol. Pictures over 25 megapixels show `IMAGE TOO LARGE, press o to open`.
- **Sending:** `Ctrl+O` opens a path prompt (type or paste a path; `Tab` completes, `Up` / `Down` choose, `Enter` attaches a file or enters a folder). Dragging a file onto the terminal pastes its path and attaches it. `Alt+V` attaches the picture on the clipboard. A chip such as `[photo.png 240 KB]` shows above the box; `Backspace` at the start of the box or `Esc` removes it. Enter sends it, and the text in the box is its caption. A reply target waits for the next text message: a file cannot carry the quote. Pictures are re-encoded, which drops EXIF data such as location. Limits: Teams pictures 4 MB, WhatsApp pictures 16 MB, files 100 MB. A Teams document asks `Upload <name> to your OneDrive? The chat may not have access. y/n` first.
- Clipboard pictures are saved in `tmp\` in the data folder and deleted after the send or when you cancel. Anything older than 24 hours is removed at start.

### Notifications

A desktop notification appears for a new incoming message when chatcli's window is not focused or the chat is not the open one. Never for your own messages, the open chat while the window is focused, muted chats, loaded history or edits. On Windows it is a toast. On Linux it goes over the session D-Bus to whatever notification daemon runs (dunst, mako, swaync, GNOME Shell, KDE Plasma); with no daemon, nothing shows and the log says why once. macOS has no notifications yet.

- The toast shows the sender and the message text by default. `preview` decides how much text leaves chatcli (see the settings above): `sender` shows `Name: new message`, `off` shows the chat title and `New message`. The text can show on a locked screen: set `preview = "off"` or `"sender"` to keep message text off it.
- At most one toast per chat per 10 seconds; the rest fold into `N new messages`.
- chatcli learns about focus from the terminal. If your terminal never reports it, chatcli treats the window as unfocused and toasts for the open chat too.
- Windows' Focus Assist or Do Not Disturb silences toasts by design, as does a Linux daemon's do-not-disturb mode.
- Sound on Linux: chatcli asks the daemon for the sound theme's `message-new-instant` sound (`sound = false` asks for silence instead) and plays nothing itself. GNOME and KDE play it. dunst and mako play no sounds on their own: add a rule that runs a player, for example in `dunstrc` a `[chatcli]` section with `appname = chatcli` and `script = ~/.config/dunst/chatcli-sound.sh`, where the script runs `canberra-gtk-play -i message-new-instant` or `paplay` on a file of your choice. chatcli's notifications have category `im.received`.
- A toast that fails logs one line and never crashes or repeats.

### Search

`Ctrl+F` opens a search over every message stored on this PC, both networks, newest first. Type to search, `Up` / `Down` choose, `Enter` opens that chat, `Esc` closes. `chatcli search <text>` prints the hits. Only what is in `store.db` is searched: messages you have loaded or received, not the full history of a chat. Quotes, `*` and other odd input never raise an error.

### Doctor

`chatcli doctor` checks the setup and prints one line per check (`OK`, `WARN` or `FAIL`) with a fix line under any problem, then exits 1 if any check failed. It covers the config files, the data folder, stored sign-ins (names only), TLS roots and `SSL_CERT_FILE`, the Microsoft sign-in host, the relay (`/health` and a probe that proves the secrets match), the WhatsApp session file, and whether another chatcli is running. It never prints secrets, tokens, ids or message text. `--online` adds one Graph call that lists your subscriptions (a single run by you, not polling). It is skipped, with a warning, while chatcli is running, because two refreshes of one sign-in can undo each other. `--json` prints one line of JSON for scripts.

## Linux and macOS (experimental)

Only the Windows build is tested by hand. CI also builds and tests on Linux and macOS. Secrets use the macOS Keychain or the Linux Secret Service (GNOME Keyring or KWallet); on Linux with no Secret Service (a headless box, a minimal window manager), set `CHATCLI_SECRET_STORE=file` to keep them in `secrets.json` in the data folder instead. That file is readable by you only (mode 0600) but is not encrypted, so anyone who can read your files can read your sign-in. Desktop notifications work on Windows and Linux, not macOS. The data folder is `~/Library/Application Support/chatcli` on macOS and `$XDG_DATA_HOME/chatcli` (or `~/.local/share/chatcli`) on Linux.

There is no Linux or macOS release yet: build from source (see [Build](#build)). On Linux with Nix, the repository is a flake: `nix build` builds chatcli, `nix shell github:Page011/chatCLI` puts it on your `PATH`, and `nix develop` opens a shell with the Rust toolchain and Bun.

## Your data

Everything stays on your machine, under `%LOCALAPPDATA%\chatcli` (on Windows; see [Linux and macOS](#linux-and-macos-experimental) for the other folders). On Linux and macOS chatcli makes the folder readable by you only (mode 0700) and tightens an older one. Files you open from the media cache carry the Windows download mark, so Office opens them in Protected View.

| File | Holds | Notes |
|---|---|---|
| `config.json` | Teams client id and tenant id, relay URL | Not secret. |
| `settings.toml` | Your settings | Written by the settings screen, one key at a time, comments kept. Hand edits are optional. A file that does not parse is never written. |
| `store.db` | Chat list, message text, reactions, attachment details (no bytes), chat flags, recent emoji, the search index, and WhatsApp contact names (plus `store.db-wal` and `store.db-shm` while open) | Plain, unencrypted SQLite. Kept until you delete it. WhatsApp logout deletes the saved contact names and links; linking a different account clears that network's cached chats and messages. |
| `media\` | Attachments you opened, named by hash | Capped and aged out. Not encrypted. `chatcli media clear` empties it. |
| `tmp\` | Clipboard pictures waiting to be sent | Deleted after the send; anything older than 24 h is removed at start. |
| `whatsapp.db` (and `whatsapp.db-wal`, `whatsapp.db-shm`) | WhatsApp linked-device keys | Anyone with this file can act as your linked device. Never copy, share or commit it. |
| `chatcli.log` | Connection, sign-in, relay and send events (older entries in `chatcli.log.1`) | Rolls to `chatcli.log.1` at 5 MB. Never holds tokens, secrets or message text. |
| `chatcli.lock` | Single-instance lock | Empty. |
| `secrets.lock` | Stops two chatcli processes writing the credential store at once | Empty. |
| `secrets.json` | Linux only, with `CHATCLI_SECRET_STORE=file`: your secrets | Readable by you only, not encrypted. Absent otherwise. |

`store.db` holds your message text unencrypted, and `whatsapp.db` holds the keys of your linked device. Use full-disk encryption (BitLocker on Windows) so a lost laptop does not expose them. Do not keep the chatcli data folder, or a copy of it, in a cloud-synced folder (OneDrive, Dropbox, Google Drive): the files would be uploaded and copied to every device that syncs.

`CHATCLI_DATA_DIR` overrides the folder (used by the tests).

Secrets are in Windows Credential Manager (Control Panel > Credential Manager > Windows Credentials > Generic Credentials). Names end in `.chatcli`:

- `teams-refresh-token` (long tokens are split into numbered pieces: `teams-refresh-token-1`, ...)
- `relay-token`
- `relay-client-state`

On macOS they are in the Keychain (Keychain Access, search for `chatcli`). On Linux they are in the Secret Service under the service `chatcli` (Seahorse, KWalletManager, or `secret-tool search service chatcli`), or in `secrets.json` in the data folder with `CHATCLI_SECRET_STORE=file`. Secrets already in the keyring are not moved when you switch: sign in and run `chatcli relay setup` again afterwards.

### Remove everything

1. Quit chatcli with `Ctrl+C` or `:quit`. chatcli deletes its Teams subscription as it quits. Closing the window, killing the process or switching the machine off skips that, and the subscription stays alive until it expires (within 24 hours). Nothing breaks while it waits, but the next start may find an orphan; chatcli removes it then.
2. Sign out of Teams: `chatcli teams logout`
3. Delete the data folder: `%LOCALAPPDATA%\chatcli` on Windows (see [Linux and macOS](#linux-and-macos-experimental) for the other folders).
4. In Credential Manager, delete every Generic Credential whose name ends in `.chatcli`. On macOS, delete the `chatcli` items in Keychain Access. On Linux, run `secret-tool clear service chatcli` (with `CHATCLI_SECRET_STORE=file` the secrets went with the data folder).
5. On your phone, open **Linked devices** (as in WhatsApp setup, step 3) and log out chatcli.
6. Delete the Worker: Cloudflare dashboard > Workers & Pages > `chatcli-relay` > Settings > Delete. Or from the `relay` folder: `bunx wrangler delete`.
7. Delete the app registration: Entra admin center > App registrations > your app > Delete.
8. Delete `target\release\chatcli.exe` (`target/release/chatcli` on Linux and macOS), or the whole `chatCLI` folder.

## Troubleshooting

**Something is not working.** Run `chatcli doctor` and follow the fix lines.

**`chatcli is already running`.** Only one chatcli can run at a time, because two would fight over the Teams subscription and the relay connection. Close the other window. If you have none open, check Task Manager for `chatcli.exe` (on Linux or macOS: `pgrep -a chatcli`).

**`chatcli is running: quit it ...`.** `teams login`, `teams logout`, `relay setup`, `relay url` and `media clear` change what the running client uses, so they refuse to run beside it. Quit chatcli (`Ctrl+C` or `:quit`), run the command, start chatcli again.

**`Stored relay secrets exist. Use --rotate ...`.** `chatcli relay setup` was run a second time. Nothing was changed, and your relay still works. Use `--rotate` only when you want new secrets, then redo both `wrangler secret put` steps. To change only the URL, use `chatcli relay url <url>`.

**Replies say `REPLY NEEDS ChatMessage.Send, sign in again`.** Add `ChatMessage.Send` under API permissions (Teams setup, step 8), log out of Teams in chatcli and sign in again. chatcli never sends a reply as a plain message.

**`chatcli needs an interactive terminal`.** Run it from a terminal window (Windows Terminal, a console window, or any Linux or macOS terminal), not from a pipe or a scheduled task.

**TLS or certificate errors on a work network.** chatcli trusts the public root certificates built into it plus the roots in the system certificate store, so a network that re-signs HTTPS with a company root installed on your system normally works. One exception: when the `SSL_CERT_FILE` environment variable is set, chatcli reads extra roots from that file instead of the system store. If the file does not hold your company's root, connections fail with `UnknownIssuer` (chatcli adds a hint saying so). Unset it for chatcli:

```
set SSL_CERT_FILE=
chatcli
```

In PowerShell: `Remove-Item Env:SSL_CERT_FILE; chatcli`. On Linux or macOS: `env -u SSL_CERT_FILE chatcli`. If `bunx wrangler` fails with a certificate error on the same network, set `NODE_EXTRA_CA_CERTS` to a PEM file of your company root before running it.

**The sign-in page loads but chatcli never finishes signing in.** The browser must run on the same computer as chatcli, because the redirect goes to `http://localhost:<port>` (Teams setup, after the first sign-in). Over SSH or with the browser on another machine it fails.

**Sign-in fails with an `AADSTS` error.** The error text is shown in the Teams panel and written to the log. Common ones:

- `AADSTS65001` or "needs admin approval": a tenant admin must grant consent (Teams setup, step 9).
- `AADSTS50011` or a redirect URI error: check Teams setup, steps 6 and 7 (platform **Mobile and desktop applications**, URI `http://localhost`).
- `AADSTS700016` or "application not found": the client id or tenant id is wrong. Run `chatcli teams setup` again.
- `AADSTS7000218` ("must contain ... client_secret"): the `http://localhost` redirect was added under the **Web** platform. Remove it there and add it under **Mobile and desktop applications** (Teams setup, steps 6 and 7). Leave **Allow public client flows** off: chatcli does not need it.
- `AADSTS50020` ("user account ... does not exist in tenant"): you signed in with a personal Microsoft account or an account from another tenant. Pick your work or school account in the browser.
- `AADSTS90002` ("Tenant ... not found"): the tenant id is wrong. Copy **Directory (tenant) ID** again and run `chatcli teams setup` again.

The log is `chatcli.log` in the data folder (`%LOCALAPPDATA%\chatcli\chatcli.log` on Windows); `chatcli --help` prints its path.

**Teams shows NOT LIVE.** The reason is on the bottom line:

| Reason | Fix |
|---|---|
| `relay not set up` | Run `chatcli relay setup <url>`, or ignore it: Teams works, press `r` to refresh. |
| `RELAY SECRETS MISSING: quit chatcli, then run chatcli relay setup` | Quit chatcli, run `chatcli relay setup <url>` again, then redo both `wrangler secret put` steps. |
| `relay settings unreadable` | `config.json` or the credential store could not be read. The notice line says which. |
| `relay offline: relay answered HTTP 503` | The Worker does not have its secrets yet. Run both `wrangler secret put` steps, then restart chatcli. |
| `relay offline: relay answered HTTP 404` | The relay URL is wrong or the Worker was deleted. Check `chatcli relay status` and `/health`. |
| `relay offline: relay rejected the token (HTTP 401) ...` | The Worker's secrets do not match chatcli's. Run `chatcli relay setup <url>` again and put both new values on the Worker. |
| `relay offline: another chatcli took the relay connection ...` | A second copy is running, maybe on another machine. Close it, then press `r`. |
| `relay offline: ...` (anything else) | chatcli cannot reach the Worker. Check `/health` in a browser and your network. It retries on its own. |
| `Teams subscription failed: ...` | Microsoft refused the subscription. The text after the colon is Microsoft's reason. Most often the Worker was not reachable when Graph checked it. chatcli retries on its own. |
| `connecting to relay`, `no Teams subscription yet` | Normal for a few seconds after start. If it lasts, read the log. |

**The Worker shows an alert about a bad `clientState`.** Something sent the Worker a notification with the wrong secret. The Worker dropped it. If it keeps happening, redo the relay setup.

## Architecture

See [docs/DESIGN.md](docs/DESIGN.md): the crates, the relay protocol, the data layout, and the failure checks. The test harnesses and what they cannot prove: [docs/testing.md](docs/testing.md). A repeatable memory benchmark against the desktop apps (no figures published yet): [docs/benchmark.md](docs/benchmark.md).

## Contributing and security

See [CONTRIBUTING.md](../CONTRIBUTING.md) to build, test and send a change. Report security problems privately, as described in [SECURITY.md](../SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
