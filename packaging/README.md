# Packaging: winget and Scoop

Files only. **Nothing here is submitted anywhere.** Submitting is a human step with the owner's accounts; the steps are below.

```
packaging/
  winget/manifests/p/Page011/chatcli/<version>/   three winget manifests (schema 1.6.0)
  scoop/bucket/chatcli.json                       Scoop manifest
```

Both install the release zip `chatcli-v<version>-x86_64-pc-windows-msvc.zip` from GitHub Releases. The zip holds a folder of that name with `chatcli.exe`, `README.md` and `LICENSE`, so both manifests name that folder (winget `RelativeFilePath`, Scoop `extract_dir`).

Both always check the hash: winget refuses a download that does not match `InstallerSha256`, and Scoop does the same with `hash`. The committed hash is a **placeholder (64 zeros)** until a release exists, so an install from these files fails closed rather than skipping the check.

Check the files against each other and `Cargo.toml` (no network):

```
bun scripts/check-packaging.ts            structure; a placeholder hash is a warning
bun scripts/check-packaging.ts --release  also fails on a placeholder hash: run before submitting
```

## Per release: fill the hash

The release publishes `<zip>.sha256` next to the zip. Its first field is the SHA-256 of the zip. Put it in both places:

- `winget/manifests/.../<version>/Page011.chatcli.installer.yaml`: `InstallerSha256` (the file may use upper or lower case; keep it lowercase here)
- `scoop/bucket/chatcli.json`: `architecture.64bit.hash`

For a new version, copy the winget folder to the new version number and change every `PackageVersion`, the URL and the folder name in `RelativeFilePath`. Run the check script, then submit.

## winget (community repository)

Package identifier: `Page011.chatcli` (publisher = the repository owner).

One-time setup, by the owner:

1. Fork [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) to the owner's GitHub account.
2. Install the tools: `winget install Microsoft.WingetCreate`.

First submission of a released version (the zip must already be public):

```
wingetcreate new https://github.com/Page011/chatCLI/releases/download/v<version>/chatcli-v<version>-x86_64-pc-windows-msvc.zip
```

It downloads the zip, computes the hash, and asks for the fields in the committed manifests (use them as the answers). It opens a pull request on `microsoft/winget-pkgs`. Moderators scan the zip; an unsigned exe can be slow to approve (see `docs/code-signing.md`).

Each later release, one command (the owner runs it; it needs a GitHub token):

```
wingetcreate update Page011.chatcli --version <version> --urls https://github.com/Page011/chatCLI/releases/download/v<version>/chatcli-v<version>-x86_64-pc-windows-msvc.zip --submit
```

To automate it, a workflow can run that command after a release is published. The token is the risk: use a **fine-grained token limited to the fork** (contents and pull requests), store it as a secret in a GitHub environment with a required reviewer, and never expose it to pull requests. Not set up here.

Test locally before submitting (needs `winget settings --enable LocalManifestFiles` once, in an admin window):

```
winget validate packaging\winget\manifests\p\Page011\chatcli\<version>
winget install --manifest packaging\winget\manifests\p\Page011\chatcli\<version>
chatcli --version
```

Then open a **new** terminal: `chatcli --version` must work (the portable alias is on `PATH` only in new windows).

## Scoop

Scoop wants a bucket: a Git repository with the manifest in a `bucket` folder.

1. Create a repository, for example `scoop-chatcli`, owned by the owner. Copy `scoop/bucket/chatcli.json` into its `bucket/` folder and commit.
2. Users then run:

   ```
   scoop bucket add chatcli https://github.com/Page011/scoop-chatcli
   scoop install chatcli
   ```

3. Updates: `checkver` reads the latest GitHub release and `autoupdate` rebuilds the URL and takes the hash from `<zip>.sha256`. The [BucketTemplate](https://github.com/ScoopInstaller/BucketTemplate) repository ships a GitHub Actions workflow that runs `checkver` on a schedule and commits the update: that is the "no hand edits" path. Without it, run `checkver.ps1 -Update` from a Scoop installation by hand.
4. Test: `scoop install .\bucket\chatcli.json` from a checkout, then `chatcli --version`.

## README "Download" section

Once both work, the README Download section lists, in this order: `winget install Page011.chatcli`, `scoop bucket add ...` plus `scoop install chatcli`, then the zip as the fallback. Keep the hash-check paragraph for the zip.

## Not done here

- Any submission, fork, token or bucket repository (the owner does these).
- A real hash: there is no release to hash yet.
- Chocolatey, MSI or MSIX, Linux and macOS package managers (out of scope).
