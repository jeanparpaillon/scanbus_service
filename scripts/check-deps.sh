#!/bin/sh
# The two dependency edges that are design decisions rather than accidents.
#
# 1. scanbus-core must not depend on zbus or dbus, directly or transitively.
#
#    The split between core and the rest is load-bearing: everything worth testing (the
#    pairing state machine, the profile pipeline, the button -> profile mapping) has to
#    be reachable without standing up a bus connection, because CI has neither a bus nor
#    a scanner. This is what keeps the split from eroding the first time someone reaches
#    for `zbus::fdo::Error` inside core because it was convenient.
#
# 2. scanbus-client must not depend on scanbus-daemon, directly or transitively.
#
#    The direction is scanbus-cli -> scanbus-client -> scanbus-core, one way
#    (scanbus-cli.md §2). The pull the other way is real, because the daemon has helpers
#    the client wants — object paths, capability decoding — and the answer is to move
#    such a helper *down* into scanbus-core, as `scanbus_core::path` was. A client crate
#    that depended on the service would be one that cannot be used to test it, and would
#    make the CLI's build pull in every backend the daemon has.
#
# Run from anywhere in the workspace; exits non-zero on a violation.
set -eu

status=0

# Fails the check if `cargo tree -p $1 $3...` mentions $2.
#
# The tree is captured in its own step so that a cargo failure aborts under `set -e`
# instead of being masked by grep's exit status at the end of a pipeline. cargo tree
# includes dev-dependencies by default, so a dependency pulled in "only for tests" is
# caught too — which matters here, since scanbus-daemon takes scanbus-client as one.
check() {
    crate=$1
    forbidden=$2
    shift 2

    tree=$(cargo tree -p "$crate" "$@")
    if hits=$(printf '%s\n' "$tree" | grep -E "$forbidden"); then
        echo "error: $crate depends on what it must not (cargo tree -p $crate $*):" >&2
        echo "$hits" >&2
        status=1
    fi
}

check scanbus-core '\bzbus\b|\bdbus\b'
check scanbus-core '\bzbus\b|\bdbus\b' --all-features

# Dev-dependencies included, as above: a `scanbus-daemon` the client pulled in "only to
# test against" is the violation, not a lesser form of it — it is what would make
# `cargo test -p scanbus-client` build the service. The daemon's own dev-dependency on
# the client is not visible here and is not meant to be: `cargo tree -p scanbus-client`
# walks what the client depends on, not what depends on it.
check scanbus-client '\bscanbus-daemon\b'
check scanbus-client '\bscanbus-daemon\b' --all-features
check scanbus-gui '\bscanbus-daemon\b'
check scanbus-gui '\bscanbus-daemon\b' --all-features

if [ "$status" -eq 0 ]; then
    echo "ok: core's tree has no D-Bus, and the client/gui trees have no daemon"
fi

exit "$status"
