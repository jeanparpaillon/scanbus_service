# Rust implementation plan — scanbus service

The code-facing half of the plan: where crates live, what they depend on, in which order
to build them, how the result is packaged and how it is tested. What the daemon *does* —
the `ScannerBackend` trait, each backend's specifics, the pairing state machine and the
profile pipeline — is [scanbus-daemon-design.md](scanbus-daemon-design.md).

Scope of this first iteration: **Manager1 + Scanner1 + Button1 + Job1**, **image** and **document** profiles only (email/OCR in a later phase), **HP (hplip walk-up)** and **Brother (brscan4/5 + brscan-skey)** backends.

## 1. Workspace layout

The plan as first written. `README.md` § *Workspace layout* is authoritative for the
crate set that actually exists today (it has since grown `scanbus-client`,
`scanbus-cli`, `scanbus-gui` and `scanbus-backend-mobile`); what this section is here for
is the *reason* for the split, below the tree.

```
scanbus/
├── Cargo.toml                  (workspace)
├── scanbus-daemon/             # main binary, D-Bus service
│   └── src/
│       ├── main.rs
│       ├── dbus/                # zbus interface implementations
│       │   ├── manager.rs
│       │   ├── scanner.rs
│       │   ├── button.rs
│       │   └── job.rs
│       ├── registry.rs          # in-memory state: known scanners, active jobs
│       ├── persistence.rs       # saved pairings (sqlite or JSON files)
│       └── profiles/
│           ├── mod.rs
│           ├── image.rs
│           └── document.rs      # multi-page PDF assembly
├── scanbus-core/                # shared types, traits, errors — no zbus dependency
│   └── src/
│       ├── model.rs              # ScannerInfo, Capabilities, ButtonInfo...
│       ├── backend.rs            # ScannerBackend trait
│       └── error.rs
├── scanbus-backend-hplip/       # implements ScannerBackend for HP
└── scanbus-backend-brother/     # implements ScannerBackend for Brother
```

Keeping `scanbus-core` separate from the rest makes it possible to test the business logic (profile pipeline, `PairingState` state machine) without depending on D-Bus or on a real device — important since the hardware will not always be available in CI.

## 2. Main dependencies

| Crate | Usage |
|---|---|
| `zbus` (+ `zvariant`) | D-Bus service, `#[dbus_interface]` macros, async |
| `tokio` | Async runtime, one task per backend/listener |
| `serde` / `serde_json` | Config serialisation + pairing persistence |
| `thiserror` | Typed errors per crate, then mapped to `zbus::fdo::Error` |
| `tracing` + `tracing-subscriber` | Structured logs (essential for debugging proprietary backends) |
| `lopdf` | Multi-page PDF assembly (`document` profile), with direct JPEG embedding |
| `image` | Format conversion/normalisation (`image` profile) |
| `notify` | Watching config files in case of external edits (optional) |
| `rusqlite` (or JSON files through `serde`) | Persisting pairings across restarts |

## 3. Suggested development order

1. **`scanbus-core`**: types, `ScannerBackend` trait, state machine, without D-Bus or a real backend — testable in isolation with a `MockBackend`.
2. **D-Bus skeleton** (`zbus`): `Manager1`/`Scanner1` with a `MockBackend` only — validate the D-Bus contract (introspection, `ObjectManager`, `PropertiesChanged`) before touching the hardware.
3. **`scanbus-backend-brother`**: discovery + pairing (without button listening at first), then `brscan-skey` integration for real listening.
4. **`image`/`document` profiles**: full end-to-end pipeline on Brother.
5. **`scanbus-backend-hplip`**: same sequence.
6. **Persistence** of pairings + restoring them when the daemon restarts.
7. **Packaging**: D-Bus `.service` file (on-demand activation), systemd unit, D-Bus policy file (`/etc/dbus-1/system.d/` or the session bus depending on the choice — session bus recommended here since this is per user, and because the daemon must own `org.scanbus` on the user's bus whether or not a graphical session is currently active).

## 4. Concrete packaging (session bus)

### D-Bus activation file

`~/.local/share/dbus-1/services/org.scanbus.service` (or `/usr/share/dbus-1/services/` for a system-wide install):

```ini
[D-BUS Service]
Name=org.scanbus
Exec=/usr/bin/false
SystemdService=scanbus.service
```

`Exec` remains a theoretical fallback (required by the format), but with `SystemdService` set, `dbus-daemon` delegates startup to systemd rather than spawning the binary directly — that is what enables true on-demand activation plus supervision by systemd.

### systemd user unit

`~/.config/systemd/user/scanbus.service` (or `/usr/lib/systemd/user/` for a system-wide install):

```ini
[Unit]
Description=Scanbus document scanner service
Requires=dbus.socket
After=dbus.socket
StartLimitIntervalSec=30
StartLimitBurst=5

[Service]
Type=dbus
BusName=org.scanbus
ExecStart=/usr/bin/scanbus-daemon
Restart=on-failure
RestartSec=2
TimeoutStopSec=45

[Install]
WantedBy=default.target
```

`Type=dbus` + `BusName=`: systemd considers the service "ready" only once the `org.scanbus` name actually appears on the bus — this avoids race conditions where a client calls a method before the service has finished initialising (loading the registry, restoring persisted pairings).

`Restart=on-failure` covers the case where a backend (Brother/HP) crashes the process — the D-Bus service comes back, but **the button listeners must be restarted** on startup: this has to be coded explicitly in `main.rs` (re-read the persisted state, call `start_listening()` again for every scanner with `Paired=true` and `Connected=true`, before even starting to handle D-Bus calls).

`TimeoutStopSec=45` matches the daemon's shutdown path better than the default. On `SIGTERM`
the daemon stops discovery, stops listeners, unexports the object tree, and releases
`org.scanbus` before exiting; that is expected to be fast in the normal case, but the unit
still needs enough headroom for an in-flight scan to finish cleanly or fail cleanly rather
than being cut off by systemd mid-cleanup.

`StartLimit*` is there because this daemon supervises vendor tooling and shells out to backend
helpers. A persistent crash loop should stop and stay visible in `systemctl --user status`
rather than being restarted forever by the user manager.

### Registration at login (no forced launch)

Since the service is activated on demand, no `autostart` entry is needed for regular use. But given the "zero PC interaction" goal for the physical button, the service must run **continuously** (not just at the time of a D-Bus call), otherwise a button press while nobody has used the D-Bus bus since boot would never be caught. Two options:

- `systemctl --user enable scanbus.service` — start it from the user's **default target**. On this machine the user manager is shared between GNOME and sway and runs with lingering enabled, so `default.target` means "start for the user manager itself", not "start once per compositor". That is deliberate here: a button press with no graphical session must still be caught, and wiring the unit to `graphical-session.target` would make it start under both sessions while still not covering the "booted but nobody logged in yet" case.
- Or trigger the D-Bus activation at session start through an autostart `.desktop` that simply runs `dbus-send --session --print-reply --dest=org.scanbus /org/scanbus org.freedesktop.DBus.Peer.Ping` — more fragile, prefer a plain `enable`.

That `default.target` choice only holds because the daemon does not require a compositor. Its
state lives under `XDG_CONFIG_HOME`/`$HOME`, profile output roots are read from
`user-dirs.dirs` or fall back to `$HOME`, and there is no dependency on `DISPLAY` or
`WAYLAND_DISPLAY` in the daemon startup path. If that ever changes, this section must change
with it.

### Packaging the binary as `.deb`

- The `.service`/`.dbus-service` files should be shipped by the package in the system paths (`/usr/lib/systemd/user/`, `/usr/share/dbus-1/services/`) rather than `~/.config`/`~/.local` — this avoids every user having to install them by hand on a multi-account machine.
- Do not enable or mask anything from maintainer scripts in global scope. The package should
  install the files; `systemctl --user enable scanbus.service` is a per-user choice, and doing
  it globally is how you end up with root-owned links a user manager cannot clean up later.
- The Brother helper script is the one packaging exception: `brscan-skey.config` points at it by
  absolute path, so it must live at a stable system location. Package it under
  `/usr/libexec/scanbus/scanbus-scanimage`, and have the future config rewrite point there rather
  than into a user's home directory.
- The packaged build should enable the daemon's `brother,hplip,mobile` features. Shipping a `.deb`
  whose daemon cannot even instantiate the Brother backend would contradict the whole point of the
  package.
- `zbus` is kept on its native Rust transport (`default-features = false`, `features = ["tokio"]`),
  so the package should not grow a `libdbus-1-3` dependency unless the actual built binaries start
  linking it later. Derive the ELF dependencies from the release binaries (`dpkg-shlibdeps` /
  `ldd`), then add only the runtime tools the packaged helper actually spawns — today `sane-utils`
  for `scanimage`.
- Maintainer scripts may print guidance and warn about a stale `brscan-skey.config` helper path,
  but they must not enable the user unit, create global symlinks, or edit other users' session
  state on install or removal.

## 5. Testing strategy

- A `MockBackend` implementing `ScannerBackend` with synthetic events (`ButtonPressedEvent` triggered manually in tests) → covers the whole D-Bus/profile pipeline without hardware.
- Real integration tests on physical Brother/HP devices as the last step of each backend, run manually (not in CI without dedicated hardware).
- `cargo test` on `scanbus-core` in regular CI; backends excluded from CI by default (`brother`, `hplip` feature flags).
