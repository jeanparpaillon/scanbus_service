# `scanbus-gui` — GNOME client for the scanbus D-Bus API

Companion to [scanbus-dbus-api.md](scanbus-dbus-api.md) (the contract),
[scanbus-cli.md](scanbus-cli.md) (the other client) and
[scanbus-rust-implementation.md](scanbus-rust-implementation.md) (the daemon). This document
describes a **client**: a GTK4/libadwaita application that speaks the API of §1–§9 and nothing
else. It never opens a scanner, never touches `brscan-skey.config`, never installs a package.
Where it needs a capability the daemon does not expose over D-Bus, that is a daemon issue —
§11 lists the five places where that is currently the case.

The mockups this design implements are [design/main.png](design/main.png) and
[design/buttons.png](design/buttons.png).

## 1. Why a GUI, and what it must not become

`TODO.md` splits the job in two, and the split is the architecture:

> - See `docs/design` for discovery, configuration
> - For jobs, use Desktop Notification Specs

Those two halves have opposite lifetimes. **Discovery and configuration** are rare, deliberate
and visual: you pair a scanner once, you assign a profile to key 3 once, and you want the
mockups' side-by-side list and detail pane while you do it. **Jobs** are the normal case, and
the entire premise of the project — "the user picks an entry on the device screen, zero PC
interaction required from here on" (API §7) — is that nobody is looking at a computer when one
happens. A job notification that only appears when the window is open notifies the one person
who did not need telling.

So the application outlives its window (§4), and the window is a view onto a running
application rather than the application itself. Everything else in this document follows from
that: the object store is owned by the application (§6), not by the window; the notification
path (§5) has no widget in it at all.

Non-goals, for the same reason the CLI has them: this is not a SANE frontend (that is Simple
Scan's job and it is a better one), not a scan-preview or editing surface, and not a second
implementation of anything the daemon does. A GUI that shells out to `scanimage`, edits a
vendor config file, or installs a `.deb` has stopped being a client.

## 2. Where the code lives

One new crate, a workspace member beside the CLI:

```
scanbus-gui/             # binary `scanbus-gui`. Depends on scanbus-client, gtk4, libadwaita, tokio.
└── src/
    ├── main.rs          # AdwApplication, the two entry modes of §4
    ├── bus.rs           # the tokio side: connection, ObjectManager, the reconnect loop
    ├── store.rs         # the object store of §6 — scanners, buttons, jobs, profiles
    ├── notify.rs        # §5, and the only module that may run with no window
    ├── window.rs        # AdwApplicationWindow, sidebar, navigation stack
    ├── scanners/        # list rows, discovery, the detail pane, the pairing flow
    ├── buttons/         # the Configure buttons page of design/buttons.png
    ├── profiles/        # the Profiles view and the option widgets of §11.1
    └── error.rs         # named D-Bus errors (API §8) as user-visible text
```

Rust and `scanbus-client`, rather than GJS or PyGObject, for one reason that outweighs the
faster prototype: **`scanbus-client` already is the client**. `ScannerState` decodes eleven
properties into typed values and rebuilds the `PairingState`/`PairingError` invariant that no
D-Bus type can express; `watch.rs` implements the subscribe → call → snapshot → stream sequence
that a GUI needs even more than the CLI does, because a GUI subscribes to everything and never
polls; `error.rs` maps the seven named errors of API §8. A GJS client rewrites all three, and
rewrites them *silently* — the failure mode of a second client implementation is not a compile
error, it is a property the daemon renamed six months ago that the GUI still reads as empty.
Taking `scanbus-client` means the GUI breaks at `cargo build` alongside the CLI and the daemon's
own conformance tests.

Dependency direction is unchanged and `scripts/check-deps.sh` gains one rule: `scanbus-gui` →
`scanbus-client` → `scanbus-core`, and **`scanbus-gui` must not depend on `scanbus-daemon`**.
The pull is the same one the CLI had — the daemon knows how to compute a default output folder
(§11.1), and the answer is the same: the helper moves down into `scanbus-core` or it becomes a
D-Bus property.

The crate is **not** in `default-members`. `cargo build` on a headless CI box or a server should
not require the GTK4 development libraries, and the daemon must stay buildable by someone who
will never run a desktop. `make gui` and an explicit `-p scanbus-gui` build it.

## 3. The window

`AdwApplicationWindow` with an `AdwNavigationSplitView`: a sidebar (Scanners, Profiles, and
Settings pinned to the bottom) and a content pane that is an `AdwNavigationView`, so
*Configure buttons* pushes a page with a real back button rather than opening a dialog.

### Scanners — [design/main.png](design/main.png)

Two `AdwPreferencesGroup`s, **Paired scanners** and **Discovered scanners**, one row per
`Scanner1` object, split on the `Paired` property. Each row shows `Name` and a status line
built from `Status` and `Connected` — *Online • Connected*, *Offline*, *Discovered • Not
paired* — and the discovered rows carry a **Pair** button, which is the only affordance that
differs between the two groups. `Find scanners…` in the header bar starts a discovery session
(§4 of this document for who owns it, [10.3](todo/10_3.md) for the mechanics).

A third pane on the right shows the selected scanner: `Status`, `Connected`, `Address`,
`Backend`, `DefaultProfile`, and a **Configure buttons** row that pushes the buttons page. The
mockup shows this pane read-only apart from the navigation row; the connection toggle lives on
the pushed page, where there is room to say what it means.

Rows are never rebuilt from a list — they are bound to the store (§6) and update in place, so a
`Status` change from `online` to `busy` does not move the selection or collapse the pane.

### Configure buttons — [design/buttons.png](design/buttons.png)

A status banner (`Online and connected`, `Address`) with the **Connection** switch —
`Connect()`/`Disconnect()`, and the one control on this page that changes the daemon's
behaviour rather than its configuration — then one row per `Button1` object showing, left to
right: `Index`, `DeviceLabel`, the assigned `Profile`, and a summary of the `ProfileOptions`
that differ from the profile's own (`Output folder: Documents/Scans`, `Format: PNG`). An
unassigned button shows **Assign profile**. Below, a **Default profile** group for
`Scanner1.DefaultProfile`, described exactly as the API describes it: what runs when a button
has none.

Two things this page must get right, and they are the reason it is its own issue
([10.6](todo/10_6.md)):

- **`LabelConfigurable=false` is the common case.** On the Brother MFC of API §5 all four keys
  are fixed. The label is therefore rendered as text, not as an entry — API §9 says the point
  of the property is to "prevent a UI client from offering a rename action that would silently
  fail", and an insensitive `AdwEntryRow` is that action with a grey tint.
- **A device label and its profile are allowed to diverge**, and the API says warning about it
  is the UI's job. Assigning `document` to the key the firmware calls "Scan to OCR" is legal
  and sometimes wanted. The row shows both, and a divergence gets an inline caption — not a
  dialog, not a refusal.

### Profiles

One page per `Profile1` object, showing its `Options`. This view is blocked on §11.1: without
an option schema there is nothing to build the widgets from. [10.7](todo/10_7.md) owns both
halves.

### Settings

Daemon presence and version, which backends are compiled in, default output folders, and
whether the background service (§4) is enabled. Two of those rows are blocked on §11.2.

## 4. The application is not the window

`AdwApplication` with two entry modes, both of the same binary:

| Invocation | Effect |
|---|---|
| `scanbus-gui` | Activates the application: raises the existing window or creates one. |
| `scanbus-gui --background` | Runs with no window, holding the application alive for §5. |

`ApplicationFlags::empty()` plus `hold()` in the background mode, and single-instance behaviour
from `AdwApplication` itself: the second invocation talks to the first over
`org.freedesktop.Application`, so opening the window from the launcher while the background
service runs gives one process with one bus connection and one store, not two clients racing on
the same discovery session.

Closing the window in background mode hides it and drops back to no-window; closing it when
nothing is holding the application quits. **Quit is a menu item, not a window close** — a user
who closes the window has not asked to stop being told when their scans finish, and this is the
one place where GNOME's usual "closing the window quits the app" is wrong for this application.

The background mode is started by an XDG autostart entry ([10.8](todo/10_8.md)), not by D-Bus
activation: nothing on the bus ever calls the GUI, so there is no activation trigger to hang it
on. Two packaging facts constrain how it is wired, both already recorded for the daemon in
[7.1](todo/7_1.md) and in `~/CLAUDE.md`:

- The systemd user manager on the development machine is **shared between two sessions** and
  runs with `Linger=yes`. A unit wired to `graphical-session.target` starts under both, and the
  project has already paid for that mistake once.
- **sway does not process XDG autostart at all**; GNOME does. An autostart `.desktop` file is
  therefore the *narrower* wiring, not the sloppier one — it starts the GUI in exactly the
  session it targets and nowhere else. A user on another compositor wires a unit into their own
  session target by hand, which is the pattern that already works there.

Note the daemon (`Type=dbus`, running under lingering, possibly since boot) and this
application have genuinely different lifetimes, and that is correct: the daemon must catch a
button press with nobody logged in; the notifier can only notify somebody who is.

## 5. Notifications

The path from a button press to a banner, with no window anywhere in it:

1. `InterfacesAdded` on `/org/scanbus/scanner/{id}/job/{jobid}` — the first page arrived
   (API §4). Nothing is shown yet: a one-page flatbed scan would put up a "scanning…" banner
   for under a second.
2. `State` reaches `processing` and `PageCount > 1`, or capture takes longer than ~2 s: post a
   low-urgency progress notification, replaced in place as `PageCount` climbs.
3. `State` reaches `done`: replace it with the result — *Scan saved · 3 pages · Documents/Scans*
   — carrying **Open** and **Open folder** actions built from `Result` (`{"path": s}` for
   `document`, `{"paths": [as]}` for `image`).
4. `State` reaches `error`: an urgent notification carrying `Error`, with no action but a
   **Details** that opens the window.

`GNotification` with `Gio.Application.send_notification` and a per-job id, so step 3 *replaces*
step 2's banner rather than stacking a second one. Actions are `app.` actions on the
application (not the window), because the application is what is running.

Three constraints the API imposes on this, all of them from §4:

- **The 60-second retention window is the deadline.** A terminal transition carries `State` and
  `Result`/`Error` in a *single* `PropertiesChanged` precisely so a client does not have to
  follow up with a `Get` it might lose the race on. The notifier reads the payload it was
  handed and never re-reads the object.
- **A job whose transfer fails before the first page publishes no object at all**, so there is
  nothing to notify about, and the GUI must not invent a failure banner from a `Button1` press
  it never saw — it does not see button presses at all.
- **Notifications survive the object.** `Result` paths are copied into the notification when it
  is posted; clicking **Open** twenty minutes later opens a file, not a vanished object path.

**Open** launches the file through `gio open` semantics (`gtk_file_launcher_launch`), which is
also the one place the GUI touches the filesystem — and it can, because GUI and daemon share
one. §11.5 is what that assumption costs if the GUI is ever sandboxed.

## 6. One store, fed by `ObjectManager`

The application owns a store built from exactly one `GetManagedObjects` at startup, then kept
current by `InterfacesAdded`, `InterfacesRemoved` and `PropertiesChanged`. Nothing polls, and
no view issues its own `Get`.

This is not a GUI convenience, it is API §2 taken at its word: "No custom `ScannerFound` signal
is needed: `ObjectManager` is enough". A GUI that re-reads properties when a view opens will,
sooner or later, read them in the middle of a pairing and render a state the daemon has already
left.

The store holds `ScannerState` values from `scanbus-client` (not raw `a{sv}`), keyed by object
path, plus buttons and jobs resolved to their owning scanner through
`scanbus_core::path::owning_scanner`. Views are `gio::ListStore` models bound to it, so a
scanner appearing during discovery inserts a row rather than rebuilding a list.

**Subscribe before you call**, everywhere. `watch.rs` exists because `pair --wait` in the CLI
could miss `pairing → done` on a scanner whose backend was already installed. The GUI has the
same race on every button it has, and one more the CLI does not: it holds subscriptions across
a daemon restart. When the bus name disappears the store is cleared and the views show
"Scanbus service is not running"; when it comes back the store is rebuilt from a fresh
`GetManagedObjects`, because a daemon that restarted has restored its paired scanners
([4.2](todo/4_2.md)) and its object paths are new.

## 7. Two runtimes, one direction of travel

GTK owns the main thread and the GLib main context; `zbus` runs on tokio (the workspace pins
`default-features = false, features = ["tokio"]` for exactly this reason — a second reactor
serving one connection is how zbus deadlocks). So: a tokio runtime on a worker thread owns the
connection and every stream, and hands the UI thread already-decoded values through an
`async_channel`, consumed in a `glib::spawn_future_local`.

The rule is that **nothing in `scanbus-client` is called from the UI thread and no GTK object
is touched from tokio**. Calls go the other way as messages (`Pair(path)`, `SetProfile(path,
kind)`), and their outcome comes back as a store update or an error message — which also means
a two-minute `installing_backend` cannot make the UI unresponsive, because the UI never waits
on it. It renders `PairingState`.

## 8. Errors

Every named error of API §8 gets one user-visible sentence and, where it exists, one action:

| Error | Shown as |
|---|---|
| `NotReachable` | "Scanner is not reachable" + **Retry** |
| `AlreadyPaired` | not shown — the store already says so; the button is gone |
| `NotPaired` / `NotConnected` | not shown — the affordance is insensitive instead |
| `BackendInstallFailed` | inline on the scanner row, with `PairingError` verbatim under **Details** |
| `UnsupportedProfile` | the profile is not offered for that scanner (`SupportedProfiles`) |
| `Busy` | "Scanner is busy" toast; the action stays available |

The pattern: an error the store could have predicted is a *disabled control*, not a dialog. An
`AdwToast` for the transient ones, an inline `AdwBanner` for a state the user must resolve, and
a dialog only for `Pair` and `Unpair`, which are the two irreversible things here.
`PairingError` is shown verbatim behind a disclosure — it is a package manager's output and
the user is the only one who can act on it.

## 9. Packaging

Ships in the same `.deb` as the daemon and the CLI ([7.2](todo/7_2.md)): the binary in
`/usr/bin`, `org.scanbus.Gui.desktop` in `/usr/share/applications`, an autostart entry in
`/etc/xdg/autostart` (§4), the icon in `/usr/share/icons/hicolor/scalable/apps`, and a GSettings
schema if window geometry ends up persisted. `Depends:` gains the GTK4 and libadwaita runtime;
the daemon must remain installable without them, so a `scanbus-gui` binary package split from
`scanbus` is the packaging shape, with the daemon's package not depending on it.

Flatpak is not the first target, and §11.5 says why it is not free.

## 10. Testing

Two layers, split where the toolkit starts:

- **Everything in §6, §7 and §8 is tested without GTK.** The store, the reducer that applies a
  `PropertiesChanged` to it, the daemon-restart path and the error mapping are ordinary Rust
  tests against the private-bus harness of [2.8](todo/2_8.md) — a daemon with `MockBackend` on a
  `dbus-daemon --session` started by the test. This is where the real logic is and it runs in
  CI with no display and no hardware.
- **The widget layer is tested by being thin.** Rows bind to store values; the tests that
  matter are that the binding exists, run under `xvfb-run` with the GTK offscreen backend for
  the handful of cases (a `Status` change updating a row in place, `LabelConfigurable=false`
  rendering as a label) that are worth the flakiness budget.

The notification path (§5) is tested by standing a stub `org.freedesktop.Notifications` on the
private bus and asserting on what the GUI sends: one notification per job, replaced not
stacked, actions carrying the paths from `Result`. That test is what stops step 3 from
regressing into a second banner.

## 11. Deltas to the D-Bus API this design exposes

Five things the GUI needs that the contract does not currently guarantee. The first is the only
one that blocks a whole view.

1. **`Profile1` has no option schema, and a GUI cannot render `a{sv}` blind.** §6 gives a table
   of "typical options" in prose and the proxy is deliberately `Name` + `Options`, which is
   enough for `profile set format=png` and not enough for a combo box: the GUI cannot know that
   `format` is one of `jpeg|jpg|png` for `image` and exactly `pdf` for `document`, that
   `quality` is `1..=100`, or that `multi_page` is a boolean — all four facts exist today, in
   `validate_options` in `scanbus-daemon/src/profiles.rs`, where no client can see them.
   Worse, `output_folder` **is normally unset**, and the daemon then computes
   `<XDG dir>/scanbus/<profile>`; the mockup's `Output folder: Documents/Scans` row is
   therefore unrenderable — a GUI either shows an empty value where the user expects a path, or
   it reimplements `default_output_root` and drifts from it. A read-only `OptionsSchema a{sv}`
   on `Profile1`, carrying per-key type, constraint and **effective default**, closes all of
   it, and it is the same information the daemon already has to have in order to reject bad
   input. [10.7](todo/10_7.md) owns it; it revises [3.1](todo/3_1.md) and API §6.
2. **`Manager1` reports neither a version nor its backends** — the same delta the CLI raises in
   [scanbus-cli.md](scanbus-cli.md) §11.4, with the same two read-only properties as the fix
   (`Version s`, `Backends as`). The Settings page of §3 has two rows that read `–` until they
   exist, and they are the first two questions anyone asks when a scanner does not appear.
3. **Discovery has no owner** ([2.9](todo/2_9.md)), and the GUI holds a session for far longer
   than the CLI does — minutes, while a user reads a list — and can be killed at any point in
   them. Until 2.9 lands, a crashed GUI leaves discovery running in the daemon forever. The
   GUI is best-effort until then: `StopDiscovery` on window close, on background transition and
   in a shutdown handler, and none of those run for a `SIGKILL`.
4. **`PairingState` cannot express "a human is looking at a phone right now"**
   ([9.3](todo/9_3.md) adds `awaiting_confirmation` and `PairingInfo a{sv}`). The GUI is the
   client that most needs it — showing six digits next to a phone screen is the whole point of
   numeric comparison — and it should render the state generically, keyed off `PairingInfo`
   rather than off the mobile backend, exactly as 9.3 argues.
5. **The GUI reads files the daemon wrote, and only a shared filesystem makes that legal.**
   `Result` carries host paths; **Open** and **Open folder** (§5) resolve them directly, and a
   thumbnail in a notification would read the image. That is fine today and it is the thing a
   Flatpak breaks: a sandboxed GUI would need the portal, and `output_folder` — a host path the
   GUI writes into the daemon's configuration — would have to survive being chosen through a
   file chooser that returns a document-portal handle. Stated here so that "let's also ship a
   Flatpak" is a decision with a known cost rather than a build-system change.
