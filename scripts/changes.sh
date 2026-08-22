#!/usr/bin/env bash
set -euo pipefail

CHANGES_FILE="${CHANGES_FILE:-CHANGES.md}"
VERSION="${1:-}"

awk -v version="$VERSION" '
function is_release(line) {
    return line ~ /^#[[:space:]]+v[0-9]/
}

is_release($0) {
    if (printing)
        exit

    if (version == "" ||
        $0 ~ ("^#[[:space:]]+v" version "([[:space:]]|$)")) {
        printing = 1
        found = 1
        skip_blank = 1
    }
    next
}

printing {
    if (skip_blank && $0 ~ /^[[:space:]]*$/)
        next

    skip_blank = 0
    print
}

END {
    if (!found)
        exit 1
}
' "$CHANGES_FILE"