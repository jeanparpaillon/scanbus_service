
# Rust implementation plan — scanbus service

Scope of this first iteration: **Manager1 + Scanner1 + Button1 + Job1**, **image** and **document** profiles only (email/OCR in a later phase), **HP (hplip walk-up)** and **Brother (brscan4/5 + brscan-skey)** backends.

## 1. Workspace layout

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

## 3. The central trait: `ScannerBackend`

This is the abstraction point that allows adding HP/Brother and then others without touching the D-Bus code.

```rust
#[async_trait]
pub trait ScannerBackend: Send + Sync {
    /// Backend identifier, e.g. "hplip", "brother-skey"
    fn id(&self) -> &'static str;

    /// Network/USB discovery specific to this backend.
    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError>;

    /// Installs/checks the required system dependencies (packages, udev rules...).
    /// Emits progress events through the provided channel.
    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError>;

    /// Starts listening for physical events (button) for this scanner.
    /// Returns a stream of ButtonPressed emitted as long as the listener runs.
    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError>;

    async fn stop_listening(&self, scanner_id: &str) -> Result<(), BackendError>;

    /// Writes the key→profile mapping into the backend config (e.g. brscan-skey.conf).
    async fn set_button_mapping(
        &self,
        scanner_id: &str,
        button_index: u32,
        profile: ProfileKind,
        options: HashMap<String, Value>,
    ) -> Result<(), BackendError>;

    /// Fetches the raw data once a scan has been triggered.
    async fn fetch_pages(
        &self,
        scanner_id: &str,
        job_id: &str,
    ) -> Result<BoxStream<'static, RawPage>, BackendError>;
}
```

`ButtonPressedEvent { scanner_id, button_index, timestamp }` makes the registry create a `Job1`, which then reads the `Button1.Profile` already known on the service side (the backend does not need to know the profile, only the index of the key pressed).

## 4. Backend specifics

### `scanbus-backend-brother`

- `discover()`: first implementation wraps `scanimage -L`, parses the Brother and eSCL sightings it already exposes on this machine, and resolves the model from there. That is the pragmatic starting point for the same reason as HP: it reuses the probe the user already has installed, it tells us immediately whether the required driver is already present, and it avoids inventing a parallel discovery path before we have acceptance coverage for the basics. Native SNMP/mDNS remains a likely optimisation later, because it can be faster and can expose model detail SANE omits; when it lands it should be justified as an optimisation rather than replacing a working discovery baseline by default.
- `ensure_installed()`: **verifies, or refuses — it never installs.** It checks that the `brscan4`/`brscan5` + `brscan-skey` packages are present and returns `BackendInstallFailed` naming the missing package and where it comes from when they are not. An earlier version of this plan said it should "start the installation" of the `.deb` packages downloaded from Brother's website, since no standard repository carries them and the download/checksum verification would have to be handled manually. That sentence describes a user-session daemon fetching an executable package over the network and installing it as root, and it is deliberately not implemented: it would make every scanbus install a supply-chain decision the user was never shown, and would make the daemon a privilege-escalation surface reachable from the session bus by any process. Assisted installation stays possible as a later, separate step, but it needs the pieces that make it defensible — URLs and checksums pinned in *our* package rather than fetched, an explicit confirmation through a UI, and PackageKit or a `pkexec` policy file the distribution can audit; HP's `com.hp.hplip`/`hp-pkservice` is the precedent to study when that day comes.
  - Detection uses **two** signals, in this order: the well-known file paths, then `dpkg-query`. Files first, because a driver whose files are on disk is usable on a machine with no `dpkg` at all; the packaging query second, because a package can be registered as installed and have had its files removed, and pairing on that answer defers the failure to `start_listening()` where it looks like a SANE problem. The package's own file list (`dpkg-query -L`) is consulted rather than trusting the hard-coded paths, since the SANE library directory is architecture-dependent.
  - The success path still emits `PairingProgress::Installing` per dependency, so `PairingState` passes through `installing_backend` even when nothing is installed. The observable sequence is what clients are written against; it must not change if assisted installation is added later.
  - The prohibition is enforced in code, not in prose: every subprocess in the crate goes through a `CommandRunner` seam, and the crate's tests assert that the only programs it can run are `scanimage` and `dpkg-query` (`-W`/`-L`, both read-only — `dpkg-query` is not `dpkg` and has no mode that unpacks an archive), and that the non-comment source contains no `pkexec`, `apt`, `dpkg -i` or HTTP client.
- `start_listening()`: starts (or connects to, if already running as a systemd user service) the `brscan-skey` daemon, and parses its output/socket to detect key presses.
- `set_button_mapping()`: rewrites `/etc/brscan-skey/brscan-skey.conf` (or the user-space equivalent) and sends a reload signal to the daemon.
- Point to watch: `brscan-skey` was originally designed to be launched once per device through an environment variable — it will need to be wrapped cleanly in a tokio task (`Command::spawn` + supervision, restart on crash).
### `scanbus-backend-hplip`

- `discover()`: uses `hp-probe`/`hpmud` through FFI or by spawning the HPLIP binaries (easier to get started: a subprocess wrapper, with direct FFI later if performance requires it).
- `ensure_installed()`: checks that `hplip` is installed, otherwise installs the package.
- `start_listening()`: on this machine `/usr/share/hplip/hpssd.py` owns `com.hplip.StatusService` on the **session** bus at `/com/hplip/StatusService`, but the walk-up path is not a raw "button pressed" signal. `hpssd` listens for `com.hplip.StatusService.Event` on both the session and system buses, keeps the resulting per-device history itself, and then emits `com.hplip.Toolbox.Event` on the session bus so the UI can call `GetHistory()`. The usable walk-up trigger is therefore the latest history entry becoming `EVENT_SCAN_WAITING_FOR_PC` (`2006`, mapped from `scanWaitingForPC` in `base/status.py`), identified by the HPLIP `device_uri` carried in the history tuple. The startup dependency now needs one more explicit piece of packaging: if `com.hplip.StatusService` is absent, scanbus should ask the session bus to activate the canonical owner and only fail if no owner appears afterwards; it must not spawn `hpssd.py` directly and become a second process manager for HPLIP internals.
- Packaging for that activation, shipped as `packaging/dbus-1/services/scanbus-hplip-status.service` and installed by `make install-services` whenever `hplip` is in `BACKENDS`:
  ```
  [D-BUS Service]
  Name=com.hplip.StatusService
  Exec=/usr/bin/hp-systray --force-startup
  ```
  This file is not optional garnish — **no HPLIP package ships one**, so without it the session bus answers `StartServiceByName` with `ServiceUnknown` and HP pairing fails at `start_listening()`. The file name is ours rather than `com.hplip.StatusService.service`: `dbus-daemon` indexes on `Name=`, not on the file name, so keeping out of HPLIP's file namespace costs nothing and cannot collide with a future hplip package. The executable must stay `hp-systray --force-startup`, not `hpssd.py`: HPLIP's own startup path goes through `/usr/share/hplip/base/device.py` into `hp-systray`, and `hp-systray` is what forks `hpssd` plus its helper pipes before the child claims `com.hplip.StatusService`. There is deliberately no `SystemdService=`: `hp-systray`'s *parent* process is the Qt tray UI and `hpssd` is its forked child, so a `Type=dbus` unit would bind the name to the tray and kill `hpssd` whenever the tray exits.
- The activator inherits the bus activation environment, and `hp-systray` exits immediately when `DISPLAY` is unset (`canEnterGUIMode4()` in `base/utils.py`). That is the one failure this packaging cannot fix, so `start_listening()` reports it separately from a missing activation file and from a failed spawn.
- `set_button_mapping()`: the event path above does **not** expose an entry index or any writable menu object — only the generic "scan waiting for PC" history entry. So the honest implementation is `buttons.count = 1`, `LabelConfigurable = false`, and a daemon-side-only mapping for `Button0.Profile`/`Button0.ProfileOptions`; there is no device label to push and no multi-entry touchscreen to pretend exists until HPLIP exposes one concretely.
- `fetch_pages()`: run the packaged `scanbus-scanimage` helper against the HPLIP `device_uri`, using `scanimage --batch` into a private spool directory and streaming the generated PNM pages back into the daemon. This is the first implementation of the shared `scanimage` path that the Brother backend should reuse when its own `fetch_pages()` lands, so the ADF/error handling lives in one crate rather than being copied.
I recommend starting with Brother: `brscan-skey` behaves in a more documented and predictable way (a simple config file) than digging into the internal D-Bus API of `hpssd`.

## 5. `PairingState` state machine on the daemon side

```rust
enum PairingState { None, Pairing, InstallingBackend, Done, Failed(String) }
```

Implemented as a tokio task spawned by `Scanner1::pair()` (a zbus method):
1. moves the state to `Pairing`, emits `PropertiesChanged`
2. calls `backend.ensure_installed()`, relays its progress → `InstallingBackend`
3. calls `backend.start_listening()`, stores the stream in the registry (a background task consumes the events)
4. final state `Done`/`Failed`, `Paired` updated accordingly
5. immediate persistence of the successful pairing (sqlite/JSON) so it survives a service restart
`CancelPairing()`: an `AbortHandle` on the tokio task of the current step.

## 6. Profile pipeline (image + document only for now)

```rust
#[async_trait]
trait ProfileProcessor {
    async fn process(&self, pages: BoxStream<'static, RawPage>, options: &HashMap<String, Value>)
        -> Result<ProfileResult, ProfileError>;
}
```

- `ImageProcessor`: writes each received page straight to a file (jpeg/png), returns the list of paths.
- `DocumentProcessor`: spools pages to a per-job temp directory while capture is running (ADF-safe memory profile), then assembles the PDF with `lopdf` and returns a path result. `lopdf` was chosen because it allows embedding backend-provided JPEG bytes directly (`DCTDecode`) without re-encoding, preserving quality and file size.
`Job1` ties it together: `State` moves to `"processing"` once every page has been received, the `ProfileProcessor` matching `Job.Profile` is invoked, and the result is stored in `Job1.Result`.

## 7. Suggested development order

1. **`scanbus-core`**: types, `ScannerBackend` trait, state machine, without D-Bus or a real backend — testable in isolation with a `MockBackend`.
2. **D-Bus skeleton** (`zbus`): `Manager1`/`Scanner1` with a `MockBackend` only — validate the D-Bus contract (introspection, `ObjectManager`, `PropertiesChanged`) before touching the hardware.
3. **`scanbus-backend-brother`**: discovery + pairing (without button listening at first), then `brscan-skey` integration for real listening.
4. **`image`/`document` profiles**: full end-to-end pipeline on Brother.
5. **`scanbus-backend-hplip`**: same sequence.
6. **Persistence** of pairings + restoring them when the daemon restarts.
7. **Packaging**: D-Bus `.service` file (on-demand activation), systemd unit, D-Bus policy file (`/etc/dbus-1/system.d/` or the session bus depending on the choice — session bus recommended here since this is per user, and because the daemon must own `org.scanbus` on the user's bus whether or not a graphical session is currently active).
## 8. Concrete packaging (session bus)

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
## 9. Testing strategy

- A `MockBackend` implementing `ScannerBackend` with synthetic events (`ButtonPressedEvent` triggered manually in tests) → covers the whole D-Bus/profile pipeline without hardware.
- Real integration tests on physical Brother/HP devices as the last step of each backend, run manually (not in CI without dedicated hardware).
- `cargo test` on `scanbus-core` in regular CI; backends excluded from CI by default (`brother`, `hplip` feature flags).
