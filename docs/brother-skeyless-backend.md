# Brother without brscan-skey

Background on the vendor stack is in [brother-brscan-arch.md](brother-brscan-arch.md). This
document is the design that follows from it: a Brother backend that needs **no package from
Brother's website** — no `brscan4`/`brscan5`, no `brscan-skey` — for either half of walk-up
scanning.

It supersedes the Brother half of
[scanbus-rust-implementation.md](scanbus-rust-implementation.md) §4, which describes
`start_listening()` as spawning `brscan-skey` and `set_button_mapping()` as rewriting
`brscan-skey.config`.

## 1. What the two Brother packages actually buy us, and what they cost

`brscan-skey` does exactly two things scanbus cares about: it **registers this host with the
MFP over SNMP**, and it **listens on UDP/54925** for the panel event. It does not carry the
image; it launches a shell script, and that script runs `scanimage` against the Brother SANE
backend. `brscan4`/`brscan5` are that SANE backend.

Both are `.deb` files from Brother's website, in no apt repository. That is why
[`ensure_installed`](../scanbus-backend-brother/src/lib.rs) verifies and refuses rather than
installing (issue 5.2, and §4 of the implementation plan): downloading and `dpkg -i`-ing an
executable from a session daemon is a supply-chain decision the user was never shown. The
refusal is right, but it leaves the Brother backend **unusable out of the box on a stock
system** — the user has to go and find two proprietary packages before a button press can ever
reach scanbus.

The cost is not only the user's. Building on `brscan-skey` also forced two mechanisms that are
pure accident of the vendor daemon:

- **A callback channel invented out of nothing** (issue 5.3). `brscan-skey` reports events to
  nobody; it runs four commands. So we would install four helper scripts, point the vendor
  config at them, and have them call back into `org.scanbus` over the session bus — a round
  trip through the filesystem and a shell to deliver a datagram that was already addressed to
  this host.
- **Rewriting a file under `/opt` that belongs to another package** (issue 5.4), with a backup
  file, own-edit detection, a re-apply-on-upgrade check, and a degraded path for when it is
  root-owned. All of it to express "button 2 is mapped", which is a fact about our state, not
  about Brother's.

Both disappear if we speak the protocol ourselves.

## 2. The replacement, in two independent halves

```text
Brother backend (no vendor package)

├── push-button service            ← replaces brscan-skey
│     ├── register     SNMP SET → UDP/161, lease refreshed
│     ├── listen       UDP/54925, one socket for all devices
│     └── translate    datagram → ScanTrigger { button index }
│
└── scan acquisition               ← replaces brscan4/brscan5
      eSCL / sane-airscan, through the packaged scanbus-scanimage helper
```

The halves are independent on purpose: the registration protocol works whether the scan is
acquired over eSCL or over a Brother SANE driver that happens to be installed, and eSCL pull
scanning works whether or not the device will ever push a button event.

### 2.1 Acquisition: eSCL, not the vendor SANE driver

`sane-airscan` is in the distribution archive (`apt install sane-airscan`), and every Brother
network model of the last several years speaks eSCL. On the development machine's MFC-J5335DW,
`scanimage -L` reports the same device three ways:

```text
device `brother4:net1;dev0' is a Brother MFC-J5335DW MFC-J5335DW
device `escl:http://192.168.1.3:80' is a Brother MFC-J5335DW adf,platen scanner
device `airscan:e1:Brother MFC-J5335DW' is a eSCL Brother MFC-J5335DW ip=192.168.1.3
```

The eSCL sighting advertises `adf,platen`, which is the whole capability set the `image` and
`document` profiles need. So acquisition needs a **precedence inversion** in discovery: today
[`scanners_from_sightings`](../scanbus-backend-brother/src/lib.rs) prefers the vendor transport
(`precedence = 0` for `brother4`/`brother5`); the eSCL URI must become the preferred
acquisition path, with the vendor URI kept only as a fallback for a model that offers no eSCL.

This does not make the Brother backend redundant with a generic eSCL backend. eSCL has no
scan-to-PC notification at all — pressing the panel key is what only this backend can deliver,
and it is the entire reason the backend exists.

### 2.2 Registration: SNMP SET, and a lease

The reverse-engineered registration is an SNMP SET to UDP/161 of a semicolon-delimited string
under Brother's private enterprise tree:

```text
TYPE=BR;BUTTON=SCAN;USER="jean";FUNC=IMAGE;HOST=192.168.1.20:54925;APPNUM=1;DURATION=360;BRID=;
```

with one registration per function (`IMAGE`/1, `EMAIL`/2, `OCR`/3, `FILE`/5). `DURATION` is a
lease in seconds, so registration is a **repeating task, not a one-off call** — the host
disappears from the panel on its own if the daemon dies, which is the behaviour we want and is
also how a `Disconnect()` is implemented: stop refreshing.

**What is verified on real hardware.** Against the MFC-J5335DW (192.168.1.3), a hand-built
SNMPv1 `GetRequest` — read-only, no state changed on the device — answers:

| OID | community | response |
| --- | --- | --- |
| `1.3.6.1.2.1.1.1.0` (sysDescr) | `public`, `internal` | `Brother NC-390w, Firmware Ver.Y  ,MID 8CH-213-002` |
| `1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0` | `public`, `internal` | OctetString `TRUE` |

The second row is the one that matters: the OID the `brscan-skey` reverse-engineering names
**exists on this device and returns a value** rather than `noSuchName`. That is the single
riskiest assumption in this design, and it survives first contact.

### 2.2.1 What the vendor binary settled

The three open questions above were expected to need a packet capture. Most of them did not:
`/opt/brother/scanner/brscan-skey/brscan-skey-exe` (0.3.4-0) **ships unstripped**, symbol table
and all, down to the names of its translation units — `snmp_encode.c`, `snmp_decode.c`,
`udp_agent.c`, `registerpc.c`, `scramble.c`. Reading the encoder is strictly better evidence
than reading a capture, because it covers every input rather than the one that happened to be
recorded. Everything below is cited to the function it came from, and implemented in
[`skey/`](../scanbus-backend-brother/src/skey/).

**The message.** `BerEncode1` (`0x406536`) writes ordinary SNMPv1 —

```text
SEQUENCE { INTEGER version, OCTET STRING community, [0xA0|op] {
    INTEGER request-id, INTEGER error-status, INTEGER error-index,
    SEQUENCE { SEQUENCE { OID, value } … } } }
```

— with three departures worth knowing, all reproduced:

- **The varbind value is always an OCTET STRING**, tag `0x04`, *including in a
  `GetRequest`*, where the RFC uses `05 00`. `BerEncNull` is present at `0x405da6` and is
  never called. That answers "the exact varbind type": there is no type negotiation, the
  registration string goes in as an octet string.
- **Lengths cap at `u16`** — `BerEncLen` (`0x405de9`) takes a `short`, so `81 xx` and
  `82 hi lo` are the only long forms it can emit.
- **The version field's width is assumed to be three bytes** when the outer length is
  computed.

**The community is `internal`, not `public`.** `InitSnmpMess` (`0x407255`) reads
`CommunityName=` from `/etc/opt/brother/scanner/brscan-skey/brscan-snmp.cfg` and falls back to
an immediate `"internal"` — and the file as shipped has that line commented out, so `internal`
is what every registration on this machine has ever used. Reads answer on both; nothing says
writes do, so scanbus registers with `internal`.

**`BRID` is the panel password, obfuscated.** The sixth field is not a device id. It is the
four-digit password from `brscan-skey.config` run through Brother's own scrambler (`enc_main`
at `0x406aaa`, tables `shuffletbl`/`hextbl`, key blob at `0x414a70`). scanbus does not
implement that obfuscation and does not offer the feature: it always writes `BRID=`, which is
what the vendor writes too when no password is set, and `password=` empty is how the config
ships. A device with a panel password set is a degraded case (§4), not a reason to
reimplement an obfuscation.

**Newer firmware does not use SNMP at all.** `register_pc` (`0x40cd24`) branches on
`ISPhoenixFirmware` between `register_pc_legacy` — the SNMP path above — and
`register_pc_phoenix`, which `POST`s *the same registration string* as JSON to
`https://<ip>/phoenix/mib` with digest auth `Public:0000`
(`{"request":[{"key":"1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0","string_value":"TYPE=BR;…"}]}`,
template at `0x415a90`). Only the SNMP path is implemented. Which branch the MFC-J5335DW
takes is **not known**, and it is now the most likely reason for registration to fail on this
machine — worth checking before blaming the BER.

**The datagram.** `udp_sent` (`0x40884f`) frames every message on 54925 as four binary bytes
then a NUL-terminated ASCII payload, and sends `4 + strlen(payload)` — the terminator is not
on the wire:

```text
byte 0   id            byte 1   length >> 8
byte 2   length        byte 3   code           byte 4…  payload
```

`check_udp_data` (`0x408134`) validates `(b[1] << 8) | b[2] == strlen(b + 4)` and dispatches on
`(b[0], b[3])`. A **panel key press is `0x01`/`0x01`**; every other pair is an internal command
the vendor CLI posts to its own daemon over the same socket (`0x80`/`0x80…0x84` for add,
delete, terminate, list, refresh). scanbus recognises those and declines them, because binding
the port means receiving them whenever someone runs `brscan-skey --refresh`.

The payload is the same `KEY=VALUE;` grammar as the registration. `decode_key_data`
(`0x40bad1`) reads `USER=`, `TYPE=` (must be `BR`), `HOST=`, `CLIENT=` — the device's own
address, and the only field that says *which* scanner this is — and `FUNC=`. The sample
datagram Brother compiled into `.data` at `0x61b480` shows the fuller set:

```text
TYPE=BR;BUTTON=SCAN;USER="idevd101";FUNC=IMAGE;HOST=10.136.150.6:54925;APPNUM=1;
P1=0;P2=0;P3=0;P4=0;REGID=756;SEQ=1;;CLIENT=10.136.41.234
```

It is committed as a fixture. Note the stray empty field before `CLIENT`: field order and the
field set are not something to depend on, and the parser is order-independent and ignores what
it has no use for.

**No reply is expected on that socket.** After `check_udp_data`, `udp_agent` (`0x407b15`) calls
only `set_sync_event`, `get_last_recv_ip_address`, `sprintf` and `strcpy` — there is no path
from a received datagram to `udp_sent`. The only thing the daemon writes is a `Refresh Device
List` command to itself when the receive loop times out.

### 2.2.2 What is still unverified

Two of the original three questions are answered. What is left needs hardware, and
[`scripts/capture-skey.sh`](../scripts/capture-skey.sh) is the one command that gets it:

- **Whether the device accepts a `SetRequest` on that OID.** The OID exists and reads back
  `TRUE`; that a write takes is a different claim, and no amount of reading the vendor's
  encoder can make it. This is now the riskiest remaining assumption.
- **Whether the MFC-J5335DW is a Phoenix-firmware model**, in which case registration is an
  HTTPS `POST` and the SNMP path will silently register nothing.
- **The `(id, code)` a real device puts in front of a key press.** `check_udp_data` requires
  `0x01`/`0x01` and the parser enforces exactly that, so a device using another pair fails
  loudly with the bytes in the message rather than silently.
- **Whether the device holds state between the notification and the scan** that eSCL
  acquisition would bypass or upset — the failure mode to watch for is a panel that reports an
  error after a scan that nonetheless succeeded. Unchanged from the original list; this one was
  never going to be answerable from a disassembly.

## 3. Shape in code

A `skey` module tree inside `scanbus-backend-brother`, parsed and tested before it is ever a
socket — the same order as the mobile backend (issue 9.1), for the same reason: the protocol is
the part that can be wrong, and hardware is the slowest possible place to find that out.

```text
scanbus-backend-brother/src/
├── lib.rs           discovery, ensure_installed, the ScannerBackend impl
└── skey/
    ├── snmp.rs      BER encode/decode for GetRequest/SetRequest/Response
    ├── register.rs  the registration string, the lease, the refresh task
    ├── event.rs     the UDP/54925 datagram → button index
    └── fields.rs    the KEY=VALUE; grammar both of the above are written in
```

As built, the three protocol modules open nothing — no socket, no file, no clock — and a test
in `lib.rs` holds them to it. The lease refresh task named above is the first thing that will
need one, and it belongs with the listener rather than here.

**No SNMP crate.** Two PDU types, one transport, no MIB parsing, no v3 crypto — the BER encoder
and decoder are roughly two hundred lines with exhaustive round-trip tests, which is a smaller
surface than auditing and pinning a dependency to get `SetRequest`. The prototype used to
produce the table in §2.2 is that code, in Python, in forty lines.

**One socket, not one per scanner.** UDP/54925 is a fixed well-known port: exactly one process
can bind it, and every registered Brother device sends to it. The listener is therefore a
process-wide singleton owned by the backend, demultiplexing by source address to a `ScannerId`,
with per-scanner subscription streams handed to `start_listening()` — `MobileBackend`'s
`ListenerBinding` is the precedent to copy. Two consequences that must be handled rather than
discovered: `EADDRINUSE` means **an installed `brscan-skey` is already running**, and must be
reported with that sentence rather than as a generic bind failure; and the host address put in
`HOST=` must be the address of the interface that routes to *that device*, not the first
non-loopback one, or a machine on a VPN registers an address the printer cannot reach.

**Buttons are registrations.** With no config file in the picture, `set_button_mapping()` stops
being a file rewrite and becomes the natural operation of the protocol:

| `Button1` index (API §5) | `FUNC` | `APPNUM` | `DeviceLabel` |
| --- | --- | --- | --- |
| 0 | `FILE` | 5 | Scan to File |
| 1 | `IMAGE` | 1 | Scan to Image |
| 2 | `OCR` | 3 | Scan to OCR |
| 3 | `EMAIL` | 2 | Scan to E-mail |

Assigning a profile registers that function; clearing it (`Profile = ""`) stops refreshing it,
and the entry **disappears from the printer's panel** within one lease. That is strictly better
than the `brscan-skey` design, where an unmapped key stayed on the panel and ran a script that
did nothing. `LabelConfigurable` stays `false`: the labels are the firmware's.

## 4. Degradation

- **No eSCL and no Brother SANE driver** — discovery still reports the scanner, pairing fails
  with a message naming `sane-airscan` and the fact that it is one `apt install` away. scanbus
  still installs nothing itself; the difference from today is that what it names is in the
  distribution archive rather than behind a vendor download form.
- **The device rejects the registration OID** — an older or newer generation (the arch notes
  mention models documenting TCP 5566 and 54921 instead). The scanner stays usable as a pull
  scanner, `buttons.count` is 0, and nothing pretends a panel entry exists.
- **`brscan-skey` is installed and running** — refuse to bind, name it, and leave the scanner
  pull-scannable. Do not try to coexist on the port.
- **A vendor driver is installed** — it is used for acquisition only when the device offers no
  eSCL. Its presence is never a requirement.
- **A panel password is set on the device** — the registration needs it in `BRID=`, scrambled
  with an obfuscation scanbus does not implement (§2.2.1). Registration will not take. Report
  it as such rather than as a timeout, and leave the scanner pull-scannable.
- **The device runs Phoenix firmware** — registration is an HTTPS `POST` rather than an SNMP
  `SetRequest` (§2.2.1). Not implemented; the same degraded path as a device that rejects the
  OID.

## 5. What this supersedes

- **Issue 5.3** (`brscan-skey` gives no event channel; our scripts are the channel) — no vendor
  daemon, no helper scripts, no callback interface, no supervision of somebody else's process.
- **Issue 5.4** (rewriting `brscan-skey.config`, reversibly) — no file under `/opt` is touched,
  so the backup, the own-edit detection and the not-writable degraded path all cease to exist.
- **Issue 5.5** (Brother end to end) is reframed as 5.12: the same acceptance run, with the
  proprietary packages removed from the machine rather than required on it.
- `ensure_installed`'s dependency set changes (5.10). The invariant it exists to enforce does
  not: this backend still cannot install anything, and the guard test still holds it to a
  read-only program allowlist.
