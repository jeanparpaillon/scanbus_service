Scanbus D-BUS service
=====================

This application implements scanbus D-Bus API, as described in
`docs/scanbus-dbus-api.md`.

## Workspace layout

| Crate | Role |
|---|---|
| `scanbus-core` | Domain model, `ScannerBackend` trait, state machines, object paths. **No `zbus`, no `dbus`.** |
| `scanbus-client` | The consumer side: `zbus` proxies, named-error decoding, property watching |
| `scanbus-daemon` | The binary: D-Bus service, registry, persistence, profile pipeline |
| `scanbus-backend-brother` | Brother backend (`brscan4`/`brscan5` + `brscan-skey`) |
| `scanbus-backend-hplip` | HP backend (HPLIP walk-up, `hpssd`) |
| `scanbus-backend-mobile` | Mobile protocol backend |

The dependency direction is one-way: backends and `scanbus-client` depend on
`scanbus-core`, the daemon depends on the backends, and nothing depends on the daemon.
`scanbus-client` is the daemon's *dev*-dependency, so the tests drive the D-Bus surface
through the same proxies the CLI will — an interface changed on one side and not the
other is a compile error rather than a stale client found by a user. When both sides
need a helper it moves *down* into `scanbus-core`, never upward.

`scanbus-core` staying free of D-Bus is what makes the pairing state machine, the
profile pipeline and the button→profile mapping testable without a bus connection or a
physical scanner — neither of which CI has. `scripts/check-deps.sh` enforces that and
the client-never-depends-on-the-daemon rule, and runs as its own CI job.

## Building

MSRV is **1.93** (`rust-version` in the workspace `Cargo.toml`; cargo enforces it). There
is deliberately no `rust-toolchain.toml`: the development machine runs rustc 1.93.1
installed from a source tarball rather than rustup, so a toolchain file would only ever
be a request rustup is not there to honour. Ubuntu's `rustfmt` and `rust-clippy`
packages match that compiler.

```sh
cargo build                                     # core + client + daemon, no backend
cargo build --workspace                         # everything, including the backend crates
cargo build -p scanbus-daemon --features brother,hplip,mobile   # daemon with the backends linked
```

Both backends are behind cargo features and off by default, because both shell out to
hardware-specific tooling. `cargo build` with no arguments builds only
`default-members` — `scanbus-core`, `scanbus-client` and `scanbus-daemon`.

## Checks

The same three things CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-deps.sh
```

## Running

The daemon reads `RUST_LOG`; without it, `info` for the scanbus crates and `warn`
elsewhere. It exits 0 on SIGTERM, which is what systemd sends; the packaged user unit
therefore uses `Type=dbus`, `BusName=org.scanbus`, and a generous `TimeoutStopSec`
so shutdown can unexport objects and release the name cleanly.

```sh
RUST_LOG=debug cargo run
```

## Completion

`scanbus` can generate shell completion scripts locally, without talking to D-Bus:

```sh
scanbus completions bash > ~/.local/share/bash-completion/completions/scanbus
```

For a one-shot load in the current shell, keep the command substitution quoted:

```sh
eval "$(scanbus completions bash)"
```

Packaging assets for issue 26 live under `packaging/`:

- `packaging/dbus-1/services/org.scanbus.service`
- `packaging/systemd/user/scanbus.service`
- `packaging/libexec/scanbus/scanbus-scanimage`
- `debian/control`
- `debian/rules`
- `debian/postinst`
- `debian/postrm`

Build the `.deb` from a clean checkout with the backend features enabled in the daemon:

```sh
make deb
```

`make deb` drives the packaged build through `debian/rules`: `make release`
builds `scanbus` and `scanbus-daemon`, `make install DESTDIR=...` stages the package
layout under `target/debian/`, `dpkg-shlibdeps`/`dpkg-gencontrol` derive the binary's
shared-library dependencies from `debian/control`, and `dpkg-deb` emits
`scanbus_<version>_<arch>.deb`. It intentionally does **not** enable `scanbus.service`;
that remains a per-user action:

```sh
systemctl --user enable --now scanbus.service
```

## Documentation

- `docs/scanbus-dbus-api.md` — the D-Bus API this service implements
- `docs/scanbus-rust-implementation.md` — implementation plan
- `docs/todo/` — the issue backlog, one file per GitHub issue
