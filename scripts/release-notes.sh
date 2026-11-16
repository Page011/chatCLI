#!/usr/bin/env bash
# Prints the CHANGELOG.md section for one version, for the release body.
# usage: scripts/release-notes.sh <tag or version> [CHANGELOG.md]
# Exit 1 with a one-sentence message when the section is missing or empty, so a tag without
# release notes fails the release job instead of publishing an empty body.
set -euo pipefail

version="${1:-}"
version="${version#v}"
file="${2:-CHANGELOG.md}"

if [ -z "$version" ]; then
  echo "usage: release-notes.sh <tag or version> [CHANGELOG.md]" >&2
  exit 2
fi
if [ ! -f "$file" ]; then
  echo "$file not found: run this from the repository root." >&2
  exit 1
fi

# From the "## [version]" heading to the next "## [" heading. The link-reference lines at the
# very bottom of the file ("[0.1.0]: https://...") belong to the last section in this scan, so
# they are dropped. Blank lines at both ends are trimmed.
body=$(tr -d '\r' < "$file" | awk -v v="$version" '
  index($0, "## [" v "]") == 1 { on = 1; next }
  on && /^## \[/               { exit }
  on && /^\[[^]]+\]: /         { next }
  on                           { print }
' | awk 'NF { if (!seen) seen = 1; for (i = 0; i < blank; i++) print ""; blank = 0; print; next } seen { blank++ }')

if [ -z "$body" ]; then
  echo "CHANGELOG.md has no section (or an empty one) for version $version: add '## [$version] - YYYY-MM-DD' with the changes, then tag again." >&2
  exit 1
fi
printf '%s\n' "$body"
