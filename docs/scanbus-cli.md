# `scanbus` — command-line client for the scanbus D-Bus API

Companion to [scanbus-dbus-api.md](scanbus-dbus-api.md) (the contract) and
[scanbus-daemon-design.md](scanbus-daemon-design.md) (the daemon). This document
describes a **client**: a binary named `scanbus` that speaks the API of §1–§8 and nothing else.
It never opens a scanner, never touches `brscan-skey.config`, never installs a package. If the
CLI needs a capability the daemon does not expose over D-Bus, that is a daemon issue, not a CLI
one — §11 lists the three places where that is currently the case.

## 1. Why a CLI at all

Three needs the GUI client cannot cover:

- **Bring-up and debugging.** Workstreams 5 and 6 involve proprietary backends whose failure
  modes are opaque. `busctl`/`gdbus` can drive the API, but they cannot subscribe before calling,
  cannot follow `PairingState` through to a verdict, and print `a{sv}` as an unreadable variant
  soup. Every acceptance criterion in workstreams 2–6 that says "observe `PropertiesChanged`"
  is a `scanbus` invocation once this exists.
- **Headless and remote configuration.** The daemon runs on the session bus of a machine that
  may be reached over ssh. Assigning a profile to a physical key must not require a graphical
  session.
- **Scripting.** "Zero PC interaction" is the goal for the *user*; automation around the daemon
  (post-scan hooks, CI on real hardware, health checks) still needs a stable, parseable surface.

Non-goals: a SANE frontend (use `scanimage`), an interactive REPL in this iteration, and any
form of scanner access that bypasses the daemon.

## 2. Where the code lives

Two new crates, both outside `scanbus-core` — which stays free of `zbus` per
[1.1](todo/1_1.md):

```
scanbus-client/          # zbus proxies + selector resolution. Depends on scanbus-core, zbus.
└── src/
    ├── proxy/           # #[proxy] traits: Manager1, Scanner1, Button1, Job1, Profile1
    ├── connect.rs       # bus selection, and --no-activate as a question about the name
    ├── convert.rs       # a{sv} -> model, and back for what a client sends
    ├── scanner.rs       # ScannerState: every Scanner1 property of API §3, typed
    ├── profile.rs       # OptionsSchema: Profile1.OptionsSchema (API §6), typed
    ├── watch.rs         # subscribe -> call -> snapshot -> stream (§7)
    ├── select.rs        # selector -> object path resolution (§5)
    └── error.rs         # named D-Bus errors (API §8) as a Rust enum
scanbus-cli/             # binary `scanbus`. Depends on scanbus-client, clap, tokio, serde_json.
└── src/
    ├── main.rs          # runtime, tracing, and the exit code
    ├── cli.rs           # the whole clap surface of §3, in one place
    ├── context.rs       # the global options resolved once, and --timeout applied
    ├── duration.rs      # the `30s` of --timeout and --for
    ├── error.rs         # command failures, and the §8 exit code of each
    ├── cmd/             # one module per subcommand group
    └── output/          # human tables and the JSON renderer
```

Object paths are built and parsed by `scanbus_core::path`, not here: the daemon exports at
them and the client resolves them, so the one place both can reach is core. That is the
shape every future shared helper takes — down into `scanbus-core`, never up into
`scanbus-daemon`.

The split exists because the daemon's own conformance suite ([2.8](todo/2_8.md)) needs exactly
these proxies. Writing them once in `scanbus-client` and consuming them as a dev-dependency from
`scanbus-daemon` means the tests and the CLI cannot drift from each other, and a change to an
interface breaks both at compile time rather than at runtime in one of them.

Dependency direction stays one-way: `scanbus-cli` → `scanbus-client` → `scanbus-core`. Nothing
depends on `scanbus-daemon`, and `scanbus-client` must not depend on it either — the temptation
will be to share a helper from the daemon's registry; the answer is to move that helper into
`scanbus-core`.

## 3. Command surface

```
scanbus [GLOBAL] <command> [ARGS]

  completions <shell>                    emit a shell completion script on stdout
  manpage                                emit the `scanbus(1)` man page on stdout
  status                                 daemon presence, version, backends, profile types
  list [--paired|--unpaired]             scanners the daemon knows about right now
  show <scanner>                         every Scanner1 property, plus its buttons
  discover [--backend b,…] [--for D]     start a discovery session and stream what appears
           [--watch] [--keep]
  pair <scanner> [--no-wait]             pair, waiting for the verdict by default
  cancel-pairing <scanner>
  unpair <scanner> [--yes]
  connect <scanner> [--profile P]
  disconnect <scanner>
  scan <scanner> [--profile P]           host-driven scan (API §3, optional method — see §11)
       [--option k=v]… [--no-wait]
  button list <scanner>
  button set <scanner> <button>          [--profile P] [--label S] [--option k=v]
       [--option-json k=json]…           at least one of the four is required
  button clear <scanner> <button>
  job list [--scanner S]
  job show <job>
  job watch [--scanner S] [--until-done]
  profile list
  profile show <name>
  profile set <name> k=v…
  monitor [--path PREFIX]                raw signal firehose, for debugging
```

### Global options

| Option | Default | Effect |
|---|---|---|
| `--json` | off | machine output (§6); implies `--no-color` |
| `--no-color` | auto | also honours `NO_COLOR` and non-TTY stdout |
| `-v`, `-vv` | off | client-side `tracing` to stderr; does not change the daemon's log level |
| `--timeout D` | `30s` | per D-Bus call, and the ceiling for any `--wait` |
| `--bus session\|system\|ADDRESS` | `session` | the daemon is a session service ([7.1](todo/7_1.md)); the escape hatch matters for a private bus in tests |
| `--no-activate` | off | fail with exit 3 instead of triggering D-Bus activation of the daemon |

`--timeout` covers waiting, not the whole command: `discover --for 20s` with the default 30s
timeout is not a conflict, because `--for` bounds the session and `--timeout` bounds each call.

### Command notes

- **`status`** is the only command that tolerates an absent daemon: it reports
  `activatable`/`running`/`absent` and exits 0 for the first two. Everything else exits 3.
- **`completions`** is local output only: it prints the shell script to stdout and never
  touches D-Bus, so it works on a machine with no bus or daemon at all. For a one-shot
  load into the current shell, use `eval "$(scanbus completions bash)"` with the command
  substitution quoted.
- **`manpage`** is local output too: it renders the `scanbus(1)` document to stdout and
  never touches D-Bus, so packaging can generate it offline from the binary itself.
- **`list`** reads `GetManagedObjects` and prints what exists *now*. It never starts discovery —
  a freshly started daemon that has restored two paired scanners lists two scanners, and that is
  the honest answer. `discover` is the command that goes looking.
- **`discover`** holds the session for `--for` (default 10s), streaming each `InterfacesAdded`
  as it arrives, then stops discovery and exits — at which point the unpaired objects it printed
  cease to exist (API §1). `--watch` runs until interrupted. `--keep` leaves discovery running
  after exit, for the case where the next command needs those objects; see §7.
- **`pair`** waits for `PairingState` to reach `done` or `failed` by default, because a script
  that has to poll afterwards is exactly what API §9 warns against. `--no-wait` returns as soon
  as the method call returns, which is what the API itself does.
- **`unpair`** prompts when stdout is a TTY; `--yes` skips the prompt. It is the one command
  that destroys persisted state ([4.1](todo/4_1.md)).
- **`button set`** checks locally before writing where that is cheaper and clearer than a daemon
  refusal: `--label` is refused when `LabelConfigurable=false`, `--profile` is checked against
  `GetProfileTypes`, `true`/`false` and bare integers in `--option k=v` are typed, and
  `--option-json` is the escape hatch when that guess would be wrong.
- **`monitor`** prints `InterfacesAdded`/`InterfacesRemoved`/`PropertiesChanged` under
  `/org/scanbus` in a readable form. This is `dbus-monitor` with the variants decoded, and it is
  what an acceptance criterion in another workstream should reference instead of grepping raw
  bus traffic.

## 4. What a session looks like

```console
$ scanbus discover --for 15s
BACKEND   ID                             NAME                     STATUS   PAIRED
brother   brother_net_192_168_1_23       MFC-L2710DW              online   no
escl      escl_avahi_HP_OfficeJet_8010   HP OfficeJet 8010        online   no

$ scanbus pair MFC-L2710DW
pairing      brother_net_192_168_1_23
installing_backend  brscan4 (downloading brscan4-0.4.11-1.amd64.deb)
done         paired

$ scanbus connect MFC-L2710DW --profile document
$ scanbus button list MFC-L2710DW
IDX  DEVICE LABEL      CONFIGURABLE  LABEL            PROFILE   OPTIONS
0    Scan to File      no            Scan to File     document  dir=~/Documents/Scans
1    Scan to Image     no            Scan to Image    image     -
2    Scan to OCR       no            Scan to OCR      -         -
3    Scan to E-mail    no            Scan to E-mail   -         -

$ scanbus button set MFC-L2710DW 2 --profile document --option dir=~/Documents/Contracts
index         2
device label  Scan to OCR
configurable  no
label         Scan to OCR
profile       document
options       dir=/home/jean/Documents/Contracts
note  device label "Scan to OCR" still points at ocr, while this host will run document

$ scanbus job watch --until-done
job 4f2a  scanner=brother_net_192_168_1_23 button=2 profile=document  receiving  pages=1
job 4f2a  receiving  pages=2
job 4f2a  processing
job 4f2a  done  path=/home/jean/Documents/Contracts/2026-08-07-143002.pdf
```

The last block is the acceptance test of [5.5](todo/5_5.md) — press a key, get a file — run from
a terminal instead of from a log tail.

## 5. Addressing objects

Every command that takes a `<scanner>` accepts, in this order:

1. an object path, if the argument starts with `/`
2. an exact `Id`
3. a unique case-insensitive prefix of an `Id`
4. a unique case-insensitive substring of a `Name`

Nothing matched → exit 4 with the list of known ids. More than one matched → exit 4 with the
candidates and the advice to use the full id. `--id` forces interpretation 2, which is what
scripts should use: a `Name` is a human label and can change under you, an `Id` is contractually
stable ([1.2](todo/1_2.md)).

Buttons take an `Index` or a unique substring of the `DeviceLabel`, so
`scanbus button set MFC 2 …` and `scanbus button set MFC E-mail …` both work. Jobs take the
short job id printed by `job list`/`job watch`, or a full path.

Resolution costs one `GetManagedObjects` call, and every command that resolves a selector must
tolerate the object disappearing between resolution and use — an unpaired scanner has a lifetime
bounded by the discovery session, and jobs are transient by construction (API §4).

## 6. Output

**Human output** is a column-aligned table with a header, no borders, `-` for an empty value.
Widths are computed from the data. No table is printed when the result is empty; a one-line note
goes to stderr instead, so `scanbus list | wc -l` counts scanners.

**`--json`** renders one JSON document for single-shot commands, and **JSON Lines** for the
streaming ones (`discover`, `job watch`, `monitor`) so `jq --unbuffered` works on a live stream.

Property names are kept **exactly as they appear on the bus** — `Id`, `DeviceLabel`,
`PairingState`, `SupportedProfiles`. Not snake_case. A `jq` filter written against
`scanbus show --json` then transfers unchanged to a client reading `Properties.GetAll` from any
other language, and the CLI stops being a second vocabulary for the same objects. `a{sv}` values
are decoded to their natural JSON equivalent; a variant the CLI does not understand is rendered
as a string with its D-Bus signature rather than dropped.

Streaming events are one object per line, tagged by `event`:

```json
{"event":"added","path":"/org/scanbus/scanner/brother_net_192_168_1_23","interfaces":{"org.scanbus.Scanner1":{"Id":"…","Paired":false}}}
{"event":"changed","path":"/org/scanbus/scanner/…","interface":"org.scanbus.Scanner1","changed":{"PairingState":"installing_backend"},"invalidated":[]}
{"event":"removed","path":"/org/scanbus/scanner/…","interfaces":["org.scanbus.Scanner1"]}
```

Errors always go to stderr, in the form `scanbus: <what failed>: <D-Bus error name>: <message>`,
including under `--json` — stdout stays parseable.

## 7. Two race conditions, and the patterns that close them

**Subscribe before you call.** `pair --wait` that calls `Pair()` and *then* subscribes to
`PropertiesChanged` loses the transition on a scanner whose backend is already installed:
`PairingState` goes `pairing → done` before the subscription exists, and the CLI waits out its
timeout on a scanner that paired successfully. The pattern, used by every `--wait` and by
`discover`:

1. create the signal stream
2. make the call
3. read the current state once (`Properties.GetAll` / `GetManagedObjects`)
4. consume the stream, treating step 3's snapshot as the first event

Step 3 is what makes step 1 sufficient. Skipping it converts a race into a hang.

**Discovery is shared, and the CLI is not its only client.** `StartDiscovery`/`StopDiscovery`
(API §2) still carry no argument saying who asked, but the daemon reference-counts the session by
the caller's bus name ([2.9]). A `scanbus discover` that calls `StopDiscovery` on exit therefore
releases only its own share: it can no longer end the session of a GUI client that started one a
second earlier, nor take that client's unpaired scanner objects with it. It cannot leave a
session running past its own exit either — the reference goes when the process does — which is
what makes `discover`'s best-effort ownership guess, and `--keep`, vestigial rather than
load-bearing.

Ownership is also what gives `pair` its most annoying failure: pairing a scanner that only
exists because of a discovery session, while that session ends underneath it. `pair` therefore
holds a discovery session itself when the target is unpaired, for the duration of the pairing,
and releases it after the verdict — a reference of its own now, which no other client's
`StopDiscovery` can take away.

## 8. Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | unclassified failure — unknown D-Bus error, I/O, malformed reply |
| 2 | usage error (from `clap`) |
| 3 | daemon unavailable: not running under `--no-activate`, or activation failed |
| 4 | selector matched nothing, or matched more than one object |
| 5 | `org.scanbus.Error.NotReachable` |
| 6 | `org.scanbus.Error.AlreadyPaired` |
| 7 | `org.scanbus.Error.NotPaired` |
| 8 | `org.scanbus.Error.NotConnected` |
| 9 | `org.scanbus.Error.BackendInstallFailed`, or a wait that ended in `PairingState="failed"` |
| 10 | `org.scanbus.Error.UnsupportedProfile` |
| 11 | `org.scanbus.Error.Busy` |
| 12 | timed out waiting for the state a `--wait` was told to wait for |
| 130 | interrupted (SIGINT) |

Codes 5–11 are the named errors of API §8, one per code, so a script can branch on *why* without
parsing English. The mapping table lives in `scanbus-client` next to the error enum, and
[2.7](todo/2_7.md) owns the other half of it in the daemon: a named error the daemon can emit and
the CLI maps to 1 is a bug in one of the two, caught by a test that enumerates both.

Exit 12 is deliberately distinct from exit 1: "the daemon never told me it finished" is a
different operational fact from "the daemon said no".

## 9. Signals and cancellation

`SIGINT` during a streaming command stops the stream, releases whatever the command holds
(a discovery session, in practice) and exits 130. `SIGINT` during `pair --wait` does **not**
cancel the pairing — the daemon owns that process and it survives the client (API §9). The CLI
prints how to cancel it (`scanbus cancel-pairing <id>`) and exits 130, rather than silently
leaving the user thinking they aborted an install.

## 10. Testing

The CLI is tested against the same private-bus harness as [2.8](todo/2_8.md): a daemon with
`MockBackend` on a `dbus-daemon --session` started by the test, and the binary invoked as a
subprocess. Assertions are on stdout under `--json` and on the exit code, never on the human
table — which is free to change. This runs in CI, since no hardware is involved.

One test per exit code in §8, driven by a mock backend told to fail in the corresponding way, is
what keeps the table from rotting.

## 11. Deltas to the D-Bus API this design exposes

Four things the CLI needs that the contract does not currently guarantee:

1. ~~**Discovery has no owner**~~ (§7). **Settled by [2.9]:** the daemon reference-counts the
   session by caller bus name — a second `StartDiscovery` joins it, `StopDiscovery` releases only
   the caller's share, and a client that dies without calling it releases its share anyway (API
   §2). What the CLI still cannot do is hold a session open past its own exit, so `discover`'s
   guess and `--keep` are now redundant rather than best-effort.
2. **`Scan()` is optional** in API §3. `scanbus scan` is unimplementable on a daemon that omits
   it, and "optional" in a contract read by several clients means "absent in practice". Either
   the daemon commits to it in [2.4](todo/2_4.md) or the CLI must detect the missing method by
   introspection and say so; this design assumes the former and degrades to a clear
   `org.freedesktop.DBus.Error.UnknownMethod` message with exit 1 if not.
3. ~~**Finished jobs may vanish immediately.**~~ **Settled by [2.6]:** the daemon keeps a
   finished job's object for **60 seconds** after `State` reaches `done`/`error`, then
   unexports it (API §1, §4). `job list` therefore shows recently finished jobs and `job watch
   --until-done` cannot miss a terminal state it was subscribed for. What the CLI must still
   not do is assume a job it saw in one command is there in the next.
4. **`Manager1` reports neither a version nor its backends.** §2 gives the manager three methods
   and no properties, so `scanbus status` — the command §3 makes a health check — can print the
   name owner and `GetProfileTypes` and nothing else. Those two rows come out as `-`, and they
   are the first two questions asked when a scanner does not appear: *which build is answering*,
   and *was it compiled with the Brother backend at all*. Neither is derivable from the bus: the
   `Backend` property of the exported scanners answers a different question (which backends
   found something), and reading the binary or the unit file would be this client going around
   the API it exists to speak. Two read-only properties on `Manager1` — `Version` (`s`) and
   `Backends` (`as`, the ids that will actually be probed, i.e. what the daemon logs as
   `probing` at startup) — close it, and `status` fills the rows in the moment they exist.

[2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10
[2.9]: https://github.com/jeanparpaillon/scanbus_service/issues/34
