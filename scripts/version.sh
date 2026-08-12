#!/bin/sh
# VERSION is the source of truth for the release version, but cargo cannot read a
# package version out of a file: `[workspace.package] version` has to be a literal in
# the manifest, and a build script cannot change it after the fact. So the number is
# copied into Cargo.toml by `sync` and the copy is held to the original by `check`,
# which CI runs — the manifest is a generated value that happens to be committed.
#
# Everything else already derives from one of those two: the member crates say
# `version.workspace = true`, debian/rules and the release workflow read VERSION.
#
#   scripts/version.sh            print the version
#   scripts/version.sh check      fail if Cargo.toml or debian/changelog has drifted
#   scripts/version.sh sync       rewrite Cargo.toml from VERSION
#   scripts/version.sh set X.Y.Z  write VERSION, then sync
set -eu

cd "$(dirname "$0")/.."

usage() {
    cat >&2 <<'EOF'
usage: scripts/version.sh [print|check|sync|set <version>]
EOF
    exit 2
}

# Trailing newline optional, so `printf %s` and `echo` both work for whoever edits it.
read_version() {
    tr -d '[:space:]' <VERSION
}

# The `[workspace.package]` version, i.e. the one every member inherits.
manifest_version() {
    sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' Cargo.toml |
        head -n 1
}

# The `version =` pins on the path dependencies in `[workspace.dependencies]`. They
# duplicate the number a sixth time and are just as easy to forget on a bump.
pinned_versions() {
    awk '
        /^\[/ { section = $0 }
        section == "[workspace.dependencies]" && /^scanbus-[a-z-]+ = \{ .*version = "/ {
            match($0, /version = "[^"]*"/)
            print substr($0, RSTART + 11, RLENGTH - 12)
        }
    ' Cargo.toml
}

# Upstream part of the top changelog entry: `scanbus (0.1.0-1) unstable; …` -> 0.1.0.
changelog_version() {
    sed -n '1s/^[^(]*(\([^)]*\)).*/\1/p' debian/changelog | sed 's/-[^-]*$//'
}

check() {
    version=$1
    status=0

    manifest=$(manifest_version)
    if [ "$manifest" != "$version" ]; then
        echo "Cargo.toml [workspace.package] version is $manifest, VERSION says $version" >&2
        status=1
    fi

    for pin in $(pinned_versions); do
        if [ "$pin" != "$version" ]; then
            echo "Cargo.toml [workspace.dependencies] pins $pin, VERSION says $version" >&2
            status=1
            break
        fi
    done

    if [ "$status" -ne 0 ]; then
        echo "run scripts/version.sh sync" >&2
    fi

    # Not fixable by `sync`: a changelog entry needs a date and a maintainer, and the
    # Debian revision is not ours to invent.
    changelog=$(changelog_version)
    if [ "$changelog" != "$version" ]; then
        echo "debian/changelog is at $changelog, VERSION says $version" >&2
        echo "run: dch --newversion $version-1 --distribution unstable" >&2
        status=1
    fi

    return $status
}

sync() {
    version=$1
    awk -v v="$version" '
        /^\[/ { section = $0 }
        section == "[workspace.package]" && /^version = "/ {
            print "version = \"" v "\""
            next
        }
        section == "[workspace.dependencies]" && /^scanbus-[a-z-]+ = \{ .*version = "/ {
            sub(/version = "[^"]*"/, "version = \"" v "\"")
        }
        { print }
    ' Cargo.toml >Cargo.toml.tmp
    mv Cargo.toml.tmp Cargo.toml

    # Cargo.lock records the workspace members' versions too; this rewrites it without
    # touching any other dependency.
    cargo update --workspace --offline >/dev/null 2>&1 ||
        cargo update --workspace >/dev/null

    echo "Cargo.toml and Cargo.lock at $version"
}

case "${1:-print}" in
    print)
        [ $# -le 1 ] || usage
        read_version
        echo
        ;;
    check)
        [ $# -le 1 ] || usage
        check "$(read_version)"
        ;;
    sync)
        [ $# -le 1 ] || usage
        sync "$(read_version)"
        ;;
    set)
        [ $# -eq 2 ] || usage
        case "$2" in
            [0-9]*.[0-9]*.[0-9]*) ;;
            *)
                echo "not a version: $2" >&2
                exit 2
                ;;
        esac
        printf '%s\n' "$2" >VERSION
        sync "$2"
        ;;
    *) usage ;;
esac
