# D-Bus API for document scanners

Namespace used in this example: `org.scanbus` (adapt it to your organisation, e.g. `io.github.<project>`). The pattern follows BlueZ: `ObjectManager`, versioned interfaces (`Xxx1`), objects that appear and disappear dynamically.

## 1. Object tree

```
/org/scanbus                              (Manager1, ObjectManager)
/org/scanbus/scanner/{id}                 (Scanner1)
/org/scanbus/scanner/{id}/button/{n}       (Button1)       -- mirrors the device's physical menu
/org/scanbus/scanner/{id}/job/{jobid}      (Job1)          -- transient
/org/scanbus/profile/{name}                (Profile1)      -- image, document, email, ocr
```

- `{id}`: stable identifier derived from the backend (e.g. `sane_epson2_net_192_168_1_50`, `escl_avahi_HP-OfficeJet`).
- Scanners that are *discovered but not paired* also show up as temporary objects (lifetime = discovery session), with `Paired=false`. This avoids having two different representations (struct vs object) for the same scanner.
- `Job1` objects are created when data is received and destroyed (or kept in a short history) once the post-processing pipeline is finished.
## 2. `org.scanbus.Manager1` interface

Object: `/org/scanbus`

### Methods

| Method | Signature | Description |
|---|---|---|
| `StartDiscovery` | `(a{sv} filters) → ()` | Starts discovery through every active backend (SANE, Avahi/eSCL, proprietary backends). `filters` is optional: `{"backends": ["sane","avahi"]}`. |
| `StopDiscovery` | `() → ()` | Stops the discovery in progress. |
| `GetProfileTypes` | `() → (as)` | Returns the available profile types: `["image","document","email","ocr"]`. |

### Signals

Uses the standard `org.freedesktop.DBus.ObjectManager`:
- `InterfacesAdded(o, a{sa{sv}})` — a scanner appears (discovery, or added after pairing)
- `InterfacesRemoved(o, as)` — a scanner disappears from the discovery session
No custom `ScannerFound` signal is needed: `ObjectManager` is enough and is the standard idiom.

## 3. `org.scanbus.Scanner1` interface

Object: `/org/scanbus/scanner/{id}`

### Properties (via `org.freedesktop.DBus.Properties`)

| Property | Type | Access | Description |
|---|---|---|---|
| `Id` | `s` | read | Stable identifier |
| `Name` | `s` | read | Human-readable name |
| `Backend` | `s` | read | `"sane"`, `"escl"`, `"proprietary:brother"`, ... |
| `Address` | `s` | read | Connection URI/path (USB, IP, etc.) |
| `Capabilities` | `a{sv}` | read | e.g. `{"resolutions":[100,200,300,600],"color_modes":["color","gray","bw"],"sources":["flatbed","adf"],"duplex":true,"buttons":{"count":4,"label_configurable":false}}` — `buttons` mirrors the device's physical menu (number of entries, labels editable or not); see §5 |
| `SupportedProfiles` | `as` | read | Subset of `["image","document","email","ocr"]` |
| `Paired` | `b` | read | Paired with this host |
| `Connected` | `b` | read | Ready to receive data |
| `Status` | `s` | read | `"offline"`, `"online"`, `"busy"`, `"error"` |
| `DefaultProfile` | `s` | read/write | Profile applied by default to data received without an explicit profile |
| `PairingState` | `s` | read | `"none"`, `"pairing"`, `"installing_backend"`, `"done"`, `"failed"` — see §6 |
| `PairingError` | `s` | read | Failure details when `PairingState="failed"`, empty otherwise |

Changes (notably `Status` and `PairingState`) are notified through the standard `PropertiesChanged` signal — no custom signals needed.

### Methods

| Method | Signature | Description |
|---|---|---|
| `Pair` | `(a{sv} options) → ()` | **Asynchronous**: starts the pairing process and returns immediately (`PairingState` moves to `"pairing"`). Installing the backend, if needed, moves `PairingState` to `"installing_backend"`. The client tracks progress through `PropertiesChanged` and reads `Paired`/`PairingState` at the end (see §6). Never blocks, however long the installation takes. |
| `CancelPairing` | `() → ()` | Cancels a pairing in progress (`PairingState` at `"pairing"` or `"installing_backend"`). Resets `PairingState` to `"none"`. |
| `Unpair` | `() → ()` | Removes the association and the persistent object. |
| `Connect` | `(a{sv} options) → ()` | Declares the host ready to receive. Fails if `Status="offline"`. `options` may include `{"profile": "document"}` to set the session profile. |
| `Disconnect` | `() → ()` | Stops listening on the host side. |
| `Scan` | `(a{sv} options) → (o job_path)` *(optional)* | Host-driven scan trigger (pull), complementing the physical trigger on the device (push). Not explicitly requested, but a useful extension. |

## 4. `org.scanbus.Job1` interface

Transient object: `/org/scanbus/scanner/{id}/job/{jobid}`, created when data is received ("Receive data" event).

### Properties

| Property | Type | Description |
|---|---|---|
| `Scanner` | `o` | Path of the source Scanner1 object |
| `Button` | `i` | Index of the physical key/menu entry that started the job, `-1` when triggered by `Scan()` on the host side |
| `Profile` | `s` | Applied profile (`"image"`, `"document"`, `"email"`, `"ocr"`, or `""` for raw) — copied from `Button1.Profile` at trigger time |
| `State` | `s` | `"receiving"` → `"processing"` → `"done"` / `"error"` |
| `PageCount` | `u` | Number of pages received (relevant for ADF/multi-page) |
| `Result` | `a{sv}` | Profile-specific, see §5 |
| `Error` | `s` | Error message when `State="error"` |

### Signals
- `PropertiesChanged` (standard) to follow progress, notably `State` and `PageCount`.
The Job appears/disappears through `InterfacesAdded`/`InterfacesRemoved` on the Manager — a client subscribed to the Manager naturally sees every job in progress without polling.

## 5. `org.scanbus.Button1` interface

Object: `/org/scanbus/scanner/{id}/button/{n}`, one object per entry of the device's physical menu (dedicated key or touchscreen entry). Created automatically at pairing time, from `Capabilities.buttons.count`.

This interface explicitly separates **what the firmware imposes** (read-only) from **what the host can assign** (read/write):

### Properties

| Property | Type | Access | Description |
|---|---|---|---|
| `Index` | `u` | read | Position in the physical menu (0-based) |
| `DeviceLabel` | `s` | read | Label as imposed by the firmware, e.g. `"Scan to E-mail"` on Brother devices with fixed keys. Empty when the device exposes no label of its own (generic touchscreen). |
| `LabelConfigurable` | `b` | read | `true` when the host can push a custom label displayed on the device (the case for some HP touchscreen models) |
| `Label` | `s` | read/write | Label actually displayed. Writes are ignored/error out when `LabelConfigurable=false` — in that case it simply mirrors `DeviceLabel`. |
| `Profile` | `s` | read/write | Profile triggered when this key is selected (`"image"`, `"document"`, `"email"`, `"ocr"`, or `""` when unassigned) |
| `ProfileOptions` | `a{sv}` | read/write | Options specific to this profile for this key (e.g. a different output folder per key) |

Writing `Profile`/`Label` makes the service rewrite the relevant backend's configuration (e.g. the `brscan-skey` config file) and reload it — invisible to the D-Bus client, which only sees the updated property.

### Concrete example (Brother MFC, 4 fixed keys)

| Index | DeviceLabel | LabelConfigurable | Assignable profile |
|---|---|---|---|
| 0 | "Scan to File" | false | `document` |
| 1 | "Scan to Image" | false | `image` |
| 2 | "Scan to OCR" | false | `ocr` |
| 3 | "Scan to E-mail" | false | `email` |

Here the physical label already implies an intent (the firmware calls it "OCR"), but nothing prevents the host from assigning a different profile (e.g. assigning `document` to key 2) — the device label and the profile actually executed may diverge, and it is up to the user interface client (config UI) to warn the user about that potential inconsistency.

## 6. Post-processing profiles

The `org.scanbus.Profile1` interface on `/org/scanbus/profile/{name}` describes the configurable options. The content of `Job1.Result` depends on the profile:

| Profile | Typical options (`Profile1` config) | `Job1.Result` |
|---|---|---|
| `image` | format (`jpeg`,`png`), quality, output folder | `{"paths": [as]}` — one file per page |
| `document` | format (`pdf`), multi-page (`b`), output folder | `{"path": s}` — a single PDF |
| `email` | preferred client, subject/body template | `{"draft_created": b, "client": s}` — no guarantee about the content, just confirmation that the draft was opened |
| `ocr` | language(s), output (`text` / `searchable-pdf`) | `{"path": s, "text_preview": s}` |

## 7. Typical flow

```
Client                          D-Bus service                     Backends
  │  StartDiscovery()                │
  │──────────────────────────────────▶│──── probe SANE/Avahi/USB ──▶
  │◀── InterfacesAdded (scanner X) ───│  (Paired=false, PairingState="none")
  │
  │  Pair(scanner X)                  │
  │──────────────────────────────────▶│
  │◀── immediate return (Pair() ) ────│
  │◀── PropertiesChanged ─────────────│  PairingState="pairing"
  │◀── PropertiesChanged ─────────────│──── installs backend ──────▶
  │                                   │  PairingState="installing_backend"
  │◀── PropertiesChanged ─────────────│
  │      Paired=true, PairingState="done"
  │      (or PairingState="failed" + PairingError filled in)
  │
  │  Connect()                        │
  │──────────────────────────────────▶│──── enables listening ─────▶
  │
  │  [initial config, once: Button1[2].Profile = "ocr"]
  │──────────────────────────────────▶│──── writes backend conf ───▶
  │
  │  [the user picks an entry on the device screen/key,
  │   zero PC interaction required from here on]
  │◀── InterfacesAdded (Job, Button=2)─│◀──── data received ────────│
  │◀── PropertiesChanged (State) ─────│──── applies profile ───────▶
  │      (profile read from Button1[2].Profile = "ocr")
  │◀── PropertiesChanged (done) ──────│
```

## 8. Error handling

Named D-Bus errors, returned through the standard `org.freedesktop.DBus.Error` mechanism:

- `org.scanbus.Error.NotReachable`
- `org.scanbus.Error.AlreadyPaired`
- `org.scanbus.Error.NotPaired`
- `org.scanbus.Error.NotConnected`
- `org.scanbus.Error.BackendInstallFailed`
- `org.scanbus.Error.UnsupportedProfile`
- `org.scanbus.Error.Busy`
## 9. Points to watch

- **`Pair()` is asynchronous by design**: no D-Bus call blocks for the duration of a package download/installation. The contract is: "I start the process, I notify you through `PropertiesChanged`". A naive client can still wait for `Paired` by polling `Properties.Get`, but the correct idiom is to subscribe to `PropertiesChanged` on the `Scanner1` object right after the call.
- **Idempotency**: calling `Pair()` on a scanner already being paired (`PairingState≠"none"`) does not restart the process — it simply returns with no effect (or raises `AlreadyPaired` if `Paired=true` already).
- **Cleanup on failure**: when `PairingState="failed"`, the Scanner1 object stays present (coming from discovery) with `Paired=false`, allowing another attempt without re-running discovery.
- **Reachable ≠ paired**: `Paired` and `Status` are independent; a paired scanner can be `"offline"` (powered off, off the network) without losing its pairing — consistent with the initial requirement.
- **Multi-page**: the `Job1` stays open (`State="receiving"`) as long as the backend reports additional pages (ADF); moving to `"processing"` marks the end of capture and the start of post-processing (e.g. PDF assembly).
- **The physical menu is not always fully controllable**: on devices with fixed keys (classic Brother), only the profile↔key mapping is configurable, not the displayed label — `LabelConfigurable=false` signals this and prevents a UI client from offering a rename action that would silently fail.
- **Multi-backend discovery**: the `Backend` property on `Scanner1` lets the client know which subsystem detected the device, useful in case of duplicates (e.g. the same scanner seen by both SANE and Avahi/eSCL) — deduplicate by physical address if needed.
- **Deduplication is the service's job, and its rule is fixed**: one device found by several backends becomes **one** `Scanner1` object, keyed by physical address (the host of a URL, a `usb:bus:device` pair, or the address verbatim when neither can be extracted). Which sighting becomes the object is decided by the daemon's backend list, ordered *most specific first* — vendor backends (`brother-skey`, `hplip`), then generic network backends (eSCL/Avahi), then `sane` last, because only a vendor backend can deliver a button press for the device it claims. A paired scanner always wins, and an object already published is never replaced by a better-ranked sighting arriving later: a client may already have a `Pair()` in flight against the path it saw. A client that wants the losing sighting anyway can restrict `StartDiscovery` with `{"backends": [...]}`; a name in that array that no backend answers to is `org.freedesktop.DBus.Error.InvalidArgs`, while a backend that is present but fails to probe is logged and skipped.
