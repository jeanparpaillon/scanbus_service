#!/bin/sh
# scanbus-core must not depend on zbus or dbus, directly or transitively.
#
# The split between core and daemon is load-bearing: everything worth testing (the
# pairing state machine, the profile pipeline, the button -> profile mapping) has to be
# reachable without standing up a bus connection, because CI has neither a bus nor a
# scanner. This check is what keeps the split from eroding the first time someone
# reaches for `zbus::fdo::Error` inside core because it was convenient.
#
# Run from anywhere in the workspace; exits non-zero on a violation.
set -eu

status=0

check() {
    # Captured in its own step so that a cargo failure aborts under `set -e` instead of
    # being masked by grep's exit status at the end of a pipeline.
    # cargo tree includes dev-dependencies by default, so a `zbus` pulled in "only for
    # tests" is caught too.
    tree=$(cargo tree -p scanbus-core "$@")
    if hits=$(printf '%s\n' "$tree" | grep -E '\bzbus\b|\bdbus\b'); then
        echo "error: scanbus-core depends on D-Bus (cargo tree -p scanbus-core $*):" >&2
        echo "$hits" >&2
        status=1
    fi
}

check
check --all-features

if [ "$status" -eq 0 ]; then
    echo "ok: scanbus-core's dependency tree is free of zbus and dbus"
fi

exit "$status"
