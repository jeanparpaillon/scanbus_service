
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

- `discover()`: classic SNMP/mDNS probe + model resolution → determines whether `brscan4` or `brscan5` is required.
- `ensure_installed()`: checks that the `brscan4`/`brscan5` + `brscan-skey` packages are present, otherwise starts the installation (`.deb` packages downloaded from the Brother website — no standard repository, so the download/checksum verification has to be handled manually).
- `start_listening()`: starts (or connects to, if already running as a systemd user service) the `brscan-skey` daemon, and parses its output/socket to detect key presses.
- `set_button_mapping()`: rewrites `/etc/brscan-skey/brscan-skey.conf` (or the user-space equivalent) and sends a reload signal to the daemon.
- Point to watch: `brscan-skey` was originally designed to be launched once per device through an environment variable — it will need to be wrapped cleanly in a tokio task (`Command::spawn` + supervision, restart on crash).
### `scanbus-backend-hplip`

- `discover()`: uses `hp-probe`/`hpmud` through FFI or by spawning the HPLIP binaries (easier to get started: a subprocess wrapper, with direct FFI later if performance requires it).
- `ensure_installed()`: checks that `hplip` is installed, otherwise installs the package.
- `start_listening()`: HPLIP exposes `hpssd` (HP System Service Daemon) over D-Bus **itself** — good news, it means we can subscribe directly to its D-Bus signals rather than parsing logs. Investigate `com.hplip.StatusService` (exact name to be checked depending on the version) for walk-up events.
- `set_button_mapping()`: the HP equivalent of key/menu mapping, through `hp-scan --register` or the `hpssd` API if it is exposed.
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
7. **Packaging**: D-Bus `.service` file (on-demand activation), systemd unit, D-Bus policy file (`/etc/dbus-1/system.d/` or the session bus depending on the choice — session bus recommended here since this is per user/graphical session, consistent with your `brscan-skey` setup as a systemd user service).
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

[Service]
Type=dbus
BusName=org.scanbus
ExecStart=/usr/bin/scanbus-daemon
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

`Type=dbus` + `BusName=`: systemd considers the service "ready" only once the `org.scanbus` name actually appears on the bus — this avoids race conditions where a client calls a method before the service has finished initialising (loading the registry, restoring persisted pairings).

`Restart=on-failure` covers the case where a backend (Brother/HP) crashes the process — the D-Bus service comes back, but **the button listeners must be restarted** on startup: this has to be coded explicitly in `main.rs` (re-read the persisted state, call `start_listening()` again for every scanner with `Paired=true` and `Connected=true`, before even starting to handle D-Bus calls).

### Registration at login (no forced launch)

Since the service is activated on demand, no `autostart` entry is needed for regular use. But given the "zero PC interaction" goal for the physical button, the service must run **continuously** (not just at the time of a D-Bus call), otherwise a button press while nobody has used the D-Bus bus since boot would never be caught. Two options:

- `systemctl --user enable scanbus.service` — explicit start at graphical login, recommended for this use case.
- Or trigger the D-Bus activation at session start through an autostart `.desktop` that simply runs `dbus-send --session --print-reply --dest=org.scanbus /org/scanbus org.freedesktop.DBus.Peer.Ping` — more fragile, prefer a plain `enable`.
### Packaging the binary as `.deb`

- The `.service`/`.dbus-service` files should be shipped by the package in the system paths (`/usr/lib/systemd/user/`, `/usr/share/dbus-1/services/`) rather than `~/.config`/`~/.local` — this avoids every user having to install them by hand on a multi-account machine.
- System dependencies to declare: `libdbus-1-3` (through `zbus`, which can work without `libdbus` depending on the transport chosen — check whether we keep zbus's native Rust transport precisely to avoid this dependency).
## 9. Testing strategy

- A `MockBackend` implementing `ScannerBackend` with synthetic events (`ButtonPressedEvent` triggered manually in tests) → covers the whole D-Bus/profile pipeline without hardware.
- Real integration tests on physical Brother/HP devices as the last step of each backend, run manually (not in CI without dedicated hardware).
- `cargo test` on `scanbus-core` in regular CI; backends excluded from CI by default (`brother`, `hplip` feature flags).
