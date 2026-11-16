#!/usr/bin/env bash
# Checks scripts/release-notes.sh on a fixture and on the real CHANGELOG.md.
# usage: scripts/test-release-notes.sh   (from the repository root)
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
check() { if [ "$2" = "$3" ]; then echo "ok    $1"; else echo "FAIL  $1: got [$2] want [$3]"; fail=1; fi; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/c.md" <<'MD'
# Changelog

## [Unreleased]

### Added

- Not released yet.

## [1.2.0] - 2030-01-02

### Added

- Thing one.

### Fixed

- Thing two.

## [1.1.0] - 2030-01-01

- Older.

## [1.0.0] - 2029-12-31

Last section, followed by link references.

[Unreleased]: https://example.invalid/compare/v1.2.0...HEAD
[1.0.0]: https://example.invalid/releases/tag/v1.0.0
MD

out=$(bash scripts/release-notes.sh v1.2.0 "$tmp/c.md")
check "middle section, v prefix, stops at the next heading" "$out" "### Added

- Thing one.

### Fixed

- Thing two."

out=$(bash scripts/release-notes.sh 1.0.0 "$tmp/c.md")
check "last section loses the link references" "$out" "Last section, followed by link references."

out=$(bash scripts/release-notes.sh 1.1.0 "$tmp/c.md")
check "no-heading body" "$out" "- Older."

bash scripts/release-notes.sh v9.9.9 "$tmp/c.md" >/dev/null 2>"$tmp/err"; code=$?
check "missing version exits 1" "$code" "1"
check "missing version says what to do" "$(grep -c "add '## \[9.9.9\]" "$tmp/err")" "1"

printf '## [2.0.0] - 2030-02-01\n\n## [1.9.0] - 2030-01-01\n\n- x\n' > "$tmp/e.md"
bash scripts/release-notes.sh 2.0.0 "$tmp/e.md" >/dev/null 2>&1; check "empty section exits 1" "$?" "1"

# The existing 0.1.0 section must extract whole (the issue's own test case).
out=$(bash scripts/release-notes.sh v0.1.0 CHANGELOG.md)
check "real 0.1.0 starts with its intro" "$(printf '%s' "$out" | head -n 1)" "First release. Early: it works day to day, with rough edges."
check "real 0.1.0 has no link reference" "$(printf '%s' "$out" | grep -c '^\[.*\]: ')" "0"
check "real 0.1.0 ends with its last bullet" "$(printf '%s' "$out" | tail -n 1)" "- The executable is not code-signed."

exit $fail
