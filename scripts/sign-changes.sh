#!/bin/sh
# Clearsign the .changes files a binary build produced, from outside the build
# container.
#
# dpkg-buildpackage is run with --no-sign, so this is what actually signs a release.
# It is a separate step because build-deb-action hands the whole step environment to
# `docker run` as an --env-file: a key exported there would be readable by every build
# script cargo executes. Signing in-place also fails open — dpkg-buildpackage only
# warns when it cannot sign, which is how a release goes out unsigned and green — so
# every failure here is fatal instead.
#
# `gpg --clearsign` over the .changes is exactly what dpkg-buildpackage's own signing
# does. The .buildinfo is left unsigned: the .changes carries its checksum, so the one
# signature already covers it.
#
# debian/scanbus-ci.pub.asc is the key users verify with, which makes it the authority
# on what may be signed with — the wrong secret has to fail the build rather than ship
# a signature nobody can check.
#
#   GPG_PRIVATE_KEY=... scripts/sign-changes.sh [artifacts-dir]
set -eu

artifacts=${1:-artifacts}
pubkey="$(dirname "$0")/../debian/scanbus-ci.pub.asc"

# Under Actions the annotation is what surfaces the reason in the job summary; run by
# hand it would just be noise in front of the message.
die() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$1"
    else
        echo "$1" >&2
    fi
    exit 1
}

[ -n "${GPG_PRIVATE_KEY:-}" ] ||
    die "GPG_PRIVATE_KEY is empty — refusing to ship unsigned packages"

# The glob is not a loop guard: with no match `sh` leaves it literal, and gpg would
# fail on a file named `*.changes` instead of on the real problem, which is a build
# that produced nothing.
set -- "$artifacts"/*.changes
[ -e "$1" ] || die "no .changes files in $artifacts"

GNUPGHOME=$(mktemp -d)
export GNUPGHOME
chmod 700 "$GNUPGHOME"
trap 'gpgconf --kill all >/dev/null 2>&1 || true; rm -rf "$GNUPGHOME"' EXIT

expected=$(gpg --batch --show-keys --with-colons "$pubkey" |
    awk -F: '/^pub:/ { print $5; exit }')
[ -n "$expected" ] || die "no key id in $pubkey"

printf '%s\n' "$GPG_PRIVATE_KEY" | gpg --batch --quiet --import
keyid=$(gpg --batch --list-secret-keys --with-colons |
    awk -F: '/^sec:/ { print $5; exit }')
[ -n "$keyid" ] || die "GPG_PRIVATE_KEY did not import a secret key"

[ "$keyid" = "$expected" ] ||
    die "GPG_PRIVATE_KEY is key $keyid, but $(basename "$pubkey") is $expected"

for changes in "$@"; do
    gpg --batch --yes --local-user "$keyid" \
        --clearsign --output "$changes.signed" "$changes"
    mv "$changes.signed" "$changes"
    gpg --batch --verify "$changes"
    echo "signed $(basename "$changes") with $keyid"
done
