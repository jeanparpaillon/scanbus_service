# Daemon design — the backend trait, pairing, profiles

What the daemon *is*: the abstraction every backend implements, what each backend does
behind it, and the two state machines (pairing, profile pipeline) that sit between a
button press and a file. The code-facing companion —  workspace layout, dependencies,
development order, packaging, testing — is
[scanbus-rust-implementation.md](scanbus-rust-implementation.md).

Scope of this first iteration: **Manager1 + Scanner1 + Button1 + Job1**, **image** and **document** profiles only (email/OCR in a later phase), **HP (hplip walk-up)** and **Brother (brscan4/5 + brscan-skey)** backends.

## 1. The central trait: `ScannerBackend`

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

## 2. Backend specifics

### `scanbus-backend-brother`

- `discover()`: first implementation wraps `scanimage -L`, parses the Brother and eSCL sightings it already exposes on this machine, and resolves the model from there. That is the pragmatic starting point for the same reason as HP: it reuses the probe the user already has installed, it tells us immediately whether the required driver is already present, and it avoids inventing a parallel discovery path before we have acceptance coverage for the basics. Native SNMP/mDNS remains a likely optimisation later, because it can be faster and can expose model detail SANE omits; when it lands it should be justified as an optimisation rather than replacing a working discovery baseline by default.
- `ensure_installed()`: **verifies, or refuses — it never installs.** It checks that the `brscan4`/`brscan5` + `brscan-skey` packages are present and returns `BackendInstallFailed` naming the missing package and where it comes from when they are not. An earlier version of this plan said it should "start the installation" of the `.deb` packages downloaded from Brother's website, since no standard repository carries them and the download/checksum verification would have to be handled manually. That sentence describes a user-session daemon fetching an executable package over the network and installing it as root, and it is deliberately not implemented: it would make every scanbus install a supply-chain decision the user was never shown, and would make the daemon a privilege-escalation surface reachable from the session bus by any process. Assisted installation stays possible as a later, separate step, but it needs the pieces that make it defensible — URLs and checksums pinned in *our* package rather than fetched, an explicit confirmation through a UI, and PackageKit or a `pkexec` policy file the distribution can audit; HP's `com.hp.hplip`/`hp-pkservice` is the precedent to study when that day comes.
  - Detection uses **two** signals, in this order: the well-known file paths, then `dpkg-query`. Files first, because a driver whose files are on disk is usable on a machine with no `dpkg` at all; the packaging query second, because a package can be registered as installed and have had its files removed, and pairing on that answer defers the failure to `start_listening()` where it looks like a SANE problem. The package's own file list (`dpkg-query -L`) is consulted rather than trusting the hard-coded paths, since the SANE library directory is architecture-dependent.
  - The success path still emits `PairingProgress::Installing` per dependency, so `PairingState` passes through `installing_backend` even when nothing is installed. The observable sequence is what clients are written against; it must not change if assisted installation is added later.
  - The prohibition is enforced in code, not in prose: every subprocess in the crate goes through a `CommandRunner` seam, and the crate's tests assert that the only programs it can run are `scanimage` and `dpkg-query` (`-W`/`-L`, both read-only — `dpkg-query` is not `dpkg` and has no mode that unpacks an archive), and that the non-comment source contains no `pkexec`, `apt`, `dpkg -i` or HTTP client.
- `start_listening()`: starts (or connects to, if already running as a systemd user service) the `brscan-skey` daemon, and parses its output/socket to detect key presses.
- `set_button_mapping()`: rewrites `/etc/brscan-skey/brscan-skey.conf` (or the user-space equivalent) and sends a reload signal to the daemon.
- Point to watch: `brscan-skey` was originally designed to be launched once per device through an environment variable — it will need to be wrapped cleanly in a tokio task (`Command::spawn` + supervision, restart on crash).

> The `brscan-skey` half of this section is superseded by
> [brother-skeyless-backend.md](brother-skeyless-backend.md), which drops the vendor
> packages entirely.

### `scanbus-backend-hplip`

- `discover()`: uses `hp-probe`/`hpmud` through FFI or by spawning the HPLIP binaries (easier to get started: a subprocess wrapper, with direct FFI later if performance requires it).
- `ensure_installed()`: checks that `hplip` is installed, otherwise installs the package.
- `start_listening()`: on this machine `/usr/share/hplip/hpssd.py` owns `com.hplip.StatusService` on the **session** bus at `/com/hplip/StatusService`, but the walk-up path is not a raw "button pressed" signal. `hpssd` listens for `com.hplip.StatusService.Event` on both the session and system buses, keeps the resulting per-device history itself, and then emits `com.hplip.Toolbox.Event` on the session bus so the UI can call `GetHistory()`. The usable walk-up trigger is therefore the latest history entry becoming `EVENT_SCAN_WAITING_FOR_PC` (`2006`, mapped from `scanWaitingForPC` in `base/status.py`), identified by the HPLIP `device_uri` carried in the history tuple. The startup dependency now needs one more explicit piece of packaging: if `com.hplip.StatusService` is absent, scanbus should ask the session bus to activate the canonical owner and only fail if no owner appears afterwards; it must not spawn `hpssd.py` directly and become a second process manager for HPLIP internals.
- `start_listening()` **also registers this host on the device**, and that half is not HPLIP's: an HP network MFP offers a host under *Scan → Computer* only once that host is in `/WalkupScanToComp/WalkupScanToCompDestinations`, a LEDM collection served over plain HTTP, and nothing in HPLIP ever writes to it (`grep -rl walkup /usr/share/hplip` matches nothing in 3.24.4). Without the write, the `hpssd` listener above is waiting for an event the device does not emit — `base/status.py` turns `scanWaitingForPC` into `EVENT_SCAN_WAITING_FOR_PC`, and the device reports that state only once a destination exists. The registration is a `POST` of a small XML document whose exact namespaces and element order are recorded in `scanbus-backend-hplip/src/walkup.rs`; the device answers `415` for a `Content-Type` other than `text/xml` and a bare `400` for anything it dislikes about the body.
- **The lifetime model is the opposite of Brother's**, which is why registration lives in `start_listening()`/`stop_listening()` rather than in `ensure_installed()`. Brother's panel entry is a lease (`DURATION=360`) that lapses on its own; an HP destination is a REST resource that persists until it is `DELETE`d — across crashes, suspends and reboots — and `WalkupScanToCompCaps` caps the collection at 15 (`MaxNetworkDestinations`). A leaked entry is therefore both permanent and a bounded resource a crash loop exhausts, so registering **sweeps this host's own stale entries first**, matched on `dd3:Hostname`, and disconnecting deletes the URI the device returned in `Location`. A USB-attached HP serves no `/WalkupScanToComp` at all: a `404` on the capability document is a skip, not a failure, because `hpssd` reports the same event off the USB status channel with no destination involved.
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

### `scanbus-backend-mobile`

The phone dials the daemon rather than the other way round, which inverts most of the
trait above. Its design is [scanbus-mobile-backend.md](scanbus-mobile-backend.md).

## 3. `PairingState` state machine on the daemon side

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

## 4. Profile pipeline (image + document only for now)

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
