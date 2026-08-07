Scanbus D-BUS service
=====================

This application implements scanbus D-Bus API, as described in
`docs/scanbus-dbus-api.md`.

## Workspace layout

| Crate | Role |
|---|---|
| `scanbus-core` | Domain model, `ScannerBackend` trait, state machines. **No `zbus`, no `dbus`.** |
| `scanbus-daemon` | The binary: D-Bus service, registry, persistence, profile pipeline |
| `scanbus-backend-brother` | Brother backend (`brscan4`/`brscan5` + `brscan-skey`) |
| `scanbus-backend-hplip` | HP backend (HPLIP walk-up, `hpssd`) |

The dependency direction is one-way: backends depend on `scanbus-core`, the daemon
depends on all three, and nothing depends on the daemon.

`scanbus-core` staying free of D-Bus is what makes the pairing state machine, the
profile pipeline and the button→profile mapping testable without a bus connection or a
physical scanner — neither of which CI has. `scripts/check-core-deps.sh` enforces it and
runs as its own CI job.

## Building

MSRV is **1.93** (`rust-version` in the workspace `Cargo.toml`; cargo enforces it). There
is deliberately no `rust-toolchain.toml`: the development machine runs rustc 1.93.1
installed from a source tarball rather than rustup, so a toolchain file would only ever
be a request rustup is not there to honour. Ubuntu's `rustfmt` and `rust-clippy`
packages match that compiler.

```sh
cargo build                                     # core + daemon, no backend
cargo build --workspace                         # everything, including the backend crates
cargo build -p scanbus-daemon --features brother,hplip   # daemon with the backends linked
```

Both backends are behind cargo features and off by default, because both shell out to
hardware-specific tooling. `cargo build` with no arguments builds only
`default-members` — `scanbus-core` and `scanbus-daemon`.

## Checks

The same three things CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-core-deps.sh
```

## Running

The daemon reads `RUST_LOG`; without it, `info` for the scanbus crates and `warn`
elsewhere. It exits 0 on SIGTERM, which is what systemd sends (see
`docs/todo/7_1.md`).

```sh
RUST_LOG=debug cargo run
```

## Documentation

- `docs/scanbus-dbus-api.md` — the D-Bus API this service implements
- `docs/scanbus-rust-implementation.md` — implementation plan
- `docs/todo/` — the issue backlog, one file per GitHub issue
