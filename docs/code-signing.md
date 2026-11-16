# Code signing for the Windows exe

**Status: decision note, nothing is signed yet.** Prices and eligibility below are from the providers' public pages as the author knew them; they change. Check each link before committing money or an identity document.

## The problem

The release `chatcli.exe` is unsigned. Windows SmartScreen shows "Windows protected your PC", and the README tells users to choose "Run anyway". That teaches a bad habit, and an unsigned exe can be slow to get through winget moderation (see `packaging/README.md`).

## What signing does and does not fix

- It puts a verified publisher name on the exe. SmartScreen then judges the publisher's reputation instead of the file's.
- **No option gives instant reputation.** It builds with downloads. A signed exe can still show a warning for the first weeks. Since 2024 an EV certificate no longer skips SmartScreen either.
- It does not make the code safe. It proves who built the file and that it was not changed afterwards.

## Options

| | SignPath Foundation | Azure Artifact Signing (was Trusted Signing) | OV or EV certificate from a CA |
|---|---|---|---|
| Cost | Free for open source | Monthly fee (a basic plan was about 10 USD a month at the last check) | Yearly: roughly 200 to 600 USD, EV at the top |
| Who can use it | Open-source projects with an OSI licence (this repo: Apache-2.0) | Identity validation by Microsoft; not open in every country, and individual developers have narrower eligibility than organisations | Anyone the CA validates |
| Publisher shown | "SignPath Foundation", not the project owner | The validated owner or organisation | The validated owner or organisation |
| Where the key lives | SignPath's HSM, never in the repo or CI | Microsoft's service; CI gets a short-lived token | Hardware token or cloud HSM (CAs require it); CI needs the CA's cloud signing service |
| CI fit | GitHub Action, one approval per release | GitHub Action, OIDC login (no stored secret) | Depends on the CA's cloud service |
| Main risk | Every release needs a manual approval and the project page needs a code-signing policy | Eligibility and a recurring bill | Cost, and the key handling |

Links: [SignPath Foundation terms](https://signpath.org/terms), [Artifact Signing pricing](https://azure.microsoft.com/pricing/details/artifact-signing/).

## Recommendation

1. **Apply to SignPath Foundation first.** It is free, chatcli is open source under an OSI licence, the build is already automated in GitHub Actions, and the manual release approval fits how releases already work (they land as drafts and the owner publishes by hand). The publisher name will be the foundation's: say so in the README so users are not surprised.
2. **If SignPath refuses or the wait is long, use Azure Artifact Signing**, but only after the owner checks that their country and identity type are eligible. Its OIDC login means no signing secret is stored in GitHub.
3. **Skip a CA certificate** unless both of the above fail: it costs the most and the key handling is the heaviest.

Whichever option is chosen, SmartScreen reputation will still take downloads to build. State that in the README paragraph, and keep the hash check next to it.

## Work to do once an option is chosen

Not started. These are the steps, in order:

1. Apply (SignPath) or validate identity (Azure). Add the code-signing policy section SignPath asks for to the README.
2. In `.github/workflows/release.yml`, sign `chatcli.exe` **before** zipping it, so the `.sha256` covers the signed file.
3. Put the signing credentials in a GitHub **environment** that needs a required reviewer, and sign **only on tags**. Signing secrets in CI are a new attack surface: a pull request must never reach them.
4. Add a step that runs `signtool verify /pa chatcli.exe` and fails the job on a bad signature.
5. Update the README SmartScreen paragraph, and the winget and Scoop notes.

## Interim wording for the README

Until signing exists the current paragraph is right: check the hash, then choose More info and Run anyway only if you trust the download. Building from source remains the alternative.
