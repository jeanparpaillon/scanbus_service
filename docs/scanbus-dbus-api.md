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
- `Job1` objects are created when the **first page is received** and destroyed once the post-processing pipeline is finished — after a **60-second retention window**, so that a client reacting to `PropertiesChanged` can still read `Result` before the object goes (see §4). A trigger that delivers no data at all publishes no object: "created when data is received" is meant literally.
## 2. `org.scanbus.Manager1` interface

Object: `/org/scanbus`

### Methods

| Method | Signature | Description |
|---|---|---|
| `StartDiscovery` | `(a{sv} filters) → ()` | Starts discovery through every active backend (SANE, Avahi/eSCL, proprietary backends). `filters` is optional: `{"backends": ["sane","avahi"]}`. |
| `StopDiscovery` | `() → ()` | Releases the caller's share of the discovery in progress; the probing stops once nobody holds one. |
| `GetProfileTypes` | `() → (as)` | Returns the available profile types: `["image","document","email","ocr"]`. |

### Properties

| Property | Type | Access | Description |
|---|---|---|---|
| `Version` | `s` | read | The daemon package version answering on this bus. |
| `Backends` | `as` | read | The backend ids this daemon will probe, in precedence order. |

**Discovery is shared between clients, and reference-counted by caller.** One session at a time, owned by every client that asked for it — tracked by unique bus name, so neither method needs an argument for it:

- A second `StartDiscovery` **joins** the running session: it restarts nothing, applies none of its `filters` to what is already running — restarting would remove and re-add every unpaired object the first client is watching — and returns successfully.
- `StopDiscovery` releases only the caller's own reference. The probing stops, and the unpaired `Scanner1` objects go with it (§1), when the **last** reference is released, not the first. Paired scanners are untouched either way.
- `StopDiscovery` from a client that never called `StartDiscovery` succeeds and changes nothing: stopping what you do not own is not an error, so a client may call it unconditionally on the way out.
- A client that disappears without calling `StopDiscovery` — killed, crashed, connection lost — releases its reference anyway; the service watches `org.freedesktop.DBus.NameOwnerChanged` for the names holding one. A `Ctrl-C` therefore cannot leave the daemon probing the network forever, and cannot take away a surviving client's objects either.

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
| `Result` | `a{sv}` | Profile-specific, see §6 |
| `Error` | `s` | Error message when `State="error"` |

### Signals
- `PropertiesChanged` (standard) to follow progress, notably `State` and `PageCount`.
The Job appears/disappears through `InterfacesAdded`/`InterfacesRemoved` on the Manager — a client subscribed to the Manager naturally sees every job in progress without polling.

A transition to a terminal state carries `State` **and** whatever moved with it (`Result` on `done`, `Error` on `error`) in a *single* `PropertiesChanged`: a client that had to follow up with a `Get` would be racing the retention window.

### Lifetime

| Event | Effect |
|---|---|
| First page received | Object exported, `State="receiving"`, `PageCount=1`, `InterfacesAdded` |
| Each further page | `PageCount` incremented, `PropertiesChanged` |
| Page stream ends | `State="processing"` — end of capture, start of the profile pipeline (§9) |
| Pipeline finished | `State="done"` with `Result`, or `State="error"` with `Error` |
| 60 s later | Object unexported, `InterfacesRemoved` |

The page transfer reports its own failures: a device that stops answering after page 3 lands the job in `"error"` and is *not* the same event as an ADF that ran out of sheets, which ends the capture normally. A trigger whose transfer fails before the first page publishes no object at all.

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
| `ProfileOptions` | `a{sv}` | read/write | Options specific to this profile for this key (e.g. a different output folder per key). Same key space as `Profile1.Options`, validated against `Profile1.OptionsSchema` — see §6 |

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

The `org.scanbus.Profile1` interface on `/org/scanbus/profile/{name}` carries the host-wide defaults for one profile **and** the machine-readable description of the options that profile accepts.

### Properties

| Property | Type | Access | Description |
|---|---|---|---|
| `Name` | `s` | read | Profile name, identical to the last element of the object path (`"image"`, `"document"`, ...) |
| `Options` | `a{sv}` | read/write | The options **stored** for this profile. A key absent here is not "no behaviour": the daemon falls back to the effective default published by `OptionsSchema`. A write **replaces the whole map**, it does not merge: a client changing one option sends the other keys back with it, or they revert to their effective defaults. Writing an unknown key, a wrong type or an out-of-range value fails with `org.freedesktop.DBus.Error.InvalidArgs` and changes nothing. |
| `OptionsSchema` | `a{sv}` | read | One entry per accepted option key, describing its type, its constraints and its effective default. This is the contract clients build editors from. |

**`OptionsSchema` is normative; the profile table at the end of this section is not.** A client that wants to render a picker, a numeric range or a folder row must read `OptionsSchema` — it must not hard-code the daemon's accepted values, and must not derive the key set from the prose here. Anything a client needs in order to send a *valid* `Options` write is in that property, by construction: it is generated from the same declaration the daemon validates writes against, so the two cannot drift.

### Shape of a schema entry

Each value of `OptionsSchema` is itself an `a{sv}`, keyed by option name:

| Field | Type | Presence | Meaning |
|---|---|---|---|
| `type` | `s` | always | One of `"string"`, `"integer"`, `"boolean"`, `"path"`. This is a widget-level vocabulary, not a D-Bus signature: `path` is a string that carries a local filesystem path, named separately so a client can offer a folder chooser rather than a text entry. |
| `default` | `v` | always | The **effective** default — see below. Its variant type matches `type`. |
| `values` | `as` | closed-set options only | Every value the daemon accepts. Absent means any value of `type` is accepted, subject to `min`/`max`. |
| `min`, `max` | `v` | bounded numeric options only | Inclusive bounds, same variant type as `default`. |
| `description` | `s` | always | One short untranslated English line, usable as a fallback label or tooltip. A client that ships its own translations should prefer them, keyed by option name. |

Two rules keep clients working across daemon versions:

- **Ignore fields you do not recognise**, and do not treat an unknown option key as an error — a generic row driven by `type` alone is a correct rendering of an option added after the client was written.
- **Do not assume an integer width.** `"integer"` values (`default`, `min`, `max`, and what `Options` carries back) may arrive as any D-Bus integer type; a client that only accepts `u` will break the day a value needs `x`.

### `default` is the effective default, not the stored one

`default` is the value the daemon will *actually use* when the key is absent from `Options` — not the literal that happens to sit in the profile store. So the rule for displaying an option is one line:

> show `Options[key]` when the key is present and non-empty, otherwise show `OptionsSchema[key].default`.

`output_folder` is the case that forces this. It is normally **unset**, and the daemon then computes the destination itself: `<XDG user dir>/scanbus/<profile>` — `XDG_PICTURES_DIR` for `image`, `XDG_DOCUMENTS_DIR` for `document`, falling back to `$HOME` when the user dirs are not configured. A client that showed an empty path there would be lying about where the next scan lands, and one that recomputed the path itself would be a second implementation of the daemon's directory logic, free to drift from it. `OptionsSchema` therefore publishes the resolved, expanded path as `output_folder.default`, and **that is what a client displays for an unset `output_folder`** — as a value inherited from the daemon, not as a value the user chose.

An `output_folder` that is present but empty or whitespace-only is treated by the daemon exactly like an absent one, so the same display rule covers it.

That computed part is also why `OptionsSchema` is read-only but **not constant**: the effective default can change without any client writing anything (the user reconfigures `XDG_PICTURES_DIR`, `$HOME` resolves elsewhere). Changes are announced with the standard `PropertiesChanged`; a client may cache the schema, but must refresh it on that signal rather than reading it once at startup.

### Aliases in `values`

`values` lists every accepted value, and the daemon may accept several spellings of the same thing — `image.format` accepts `jpeg`, `jpg` and `png`, where `jpg` is an alias of `jpeg` and produces identical output. A client may collapse aliases into a single entry in a picker, and it must accept any of the spellings coming back in `Options`; it must not present an alias as a distinct outcome. Nothing in the schema marks aliases today: an option whose distinct behaviours must be machine-distinguishable is a schema change, not a client heuristic.

### The two profiles currently exported

Illustrative snapshot of what a daemon publishes, shown as JSON for readability — the property is the contract, this is not:

```jsonc
// /org/scanbus/profile/image
{
  "format":        {"type": "string",  "default": "jpeg", "values": ["jpeg","jpg","png"],
                    "description": "Encoding of the written page files"},
  "quality":       {"type": "integer", "default": 90, "min": 1, "max": 100,
                    "description": "JPEG quality; ignored when format is png"},
  "output_folder": {"type": "path",    "default": "/home/user/Pictures/scanbus/image",
                    "description": "Directory the pages are written to"}
}

// /org/scanbus/profile/document
{
  "format":        {"type": "string",  "default": "pdf", "values": ["pdf"],
                    "description": "Output document format"},
  "multi_page":    {"type": "boolean", "default": true,
                    "description": "Assemble every page into one PDF instead of one PDF per page"},
  "output_folder": {"type": "path",    "default": "/home/user/Documents/scanbus/document",
                    "description": "Directory the document is written to"}
}
```

### Per-button overrides use the same schema

`Button1.ProfileOptions` (§5) is the same key space as `Profile1.Options`, for the profile named by `Button1.Profile`, and is validated against the same schema. The fallback chain has one more link: a key absent from `ProfileOptions` falls back to `Profile1.Options`, and only then to `OptionsSchema[key].default`. A client editing a button therefore renders `default` as "inherited from the profile", not as "the daemon's built-in value".

### Why `a{sv}` entries rather than a typed D-Bus struct

A closed struct — say `(sysvv)` for name/type/description/default/values — would be smaller on the wire and would let a Rust client decode into one type with no map lookups. It is rejected for the same reason `Scanner1.Capabilities` (§3) is a map: the option set is not closed. `ocr` alone brings a language list that is visibly not a fixed enum (it depends on the installed tesseract language packs), and `email` brings template strings with no numeric bounds and no value set at all. Every one of those additions would mean a new struct member, i.e. a new signature, i.e. a breaking change for every existing client — while with `a{sv}` entries an old client simply keeps ignoring the fields it does not know, as the rules above require it to. The cost is real and accepted: nothing in the type system stops a daemon from publishing an entry with no `type`, so the daemon is responsible for generating the schema from its validator rather than hand-writing it.

### Options and the `Job1.Result` they produce

Explanatory only — for the accepted keys and their constraints, read `OptionsSchema`. `email` and `ocr` are design targets, not yet exported as `Profile1` objects.

| Profile | Typical options | `Job1.Result` |
|---|---|---|
| `image` | format (`jpeg`,`png`), quality, output folder | `{"paths": [as]}` — one file per page |
| `document` | format (`pdf`), multi-page (`b`), output folder | `{"path": s}` for a single PDF, `{"paths": [as]}` when `multi_page=false` splits the scan into one PDF per page |
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
