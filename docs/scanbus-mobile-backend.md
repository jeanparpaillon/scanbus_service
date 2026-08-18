# The mobile backend — a scanner that dials us

Host-side design for `scanbus-backend-mobile`, the backend that makes a phone running
the scanbus mobile app appear as a `Scanner1`. The device-side protocol is specified in
[app-specs.md](https://github.com/jeanparpaillon/scanbus_android_app/blob/master/docs/app-specs.md)
(the `scanbus_android_app` repository); this document is what the daemon does with it,
which of its assumptions do not survive contact with the D-Bus API of
[scanbus-dbus-api.md](scanbus-dbus-api.md), and what has to change on either side.

Read §10 first if you are working on the app: it is the list of things the app has to do
that its own spec does not say.

## 1. Why it looks nothing like Brother or HP

Every assumption baked into `ScannerBackend`
([scanbus-daemon-design.md](scanbus-daemon-design.md) §1) is inverted here,
and each inversion is load-bearing rather than cosmetic:

| | Brother / HP | Mobile |
|---|---|---|
| Who is discovered | the device, by us | the device advertises, we browse |
| Who opens the connection | the host, to the device | **the app, to the host** |
| What triggers a scan | a physical key | an upload that arrives with its profile already chosen |
| Where the data comes from | we pull it after the trigger | it is already on the wire behind the trigger |
| Listener lifetime | one per paired scanner | **one for all of them**, for the life of the daemon |
| `ensure_installed` | downloads a `.deb` | there is nothing to install; the handshake happens here instead |
| Buttons | 1–4 fixed keys | none |

The consequence worth stating up front: **after pairing, the host never dials the phone
again.** Not to check it is alive, not to fetch pages, not to configure it. That single
fact removes an entire class of problem the other two backends have — a stale IP address
does not matter, NAT does not matter, the phone sleeping does not matter — and creates
exactly one new one, which is that the host cannot tell whether a paired phone is
reachable (§7).

## 2. Naming

The backend is `mobile`, not `android`. app-specs.md §4 calls it `AndroidBackend` with
`Backend="android"`; the protocol has nothing Android-specific in it — no ADB, no
Play-services dependency, no `NsdManager` detail on the wire — and an iOS or a desktop
implementation of the same three messages would be indistinguishable to this daemon. A
`Backend` property that names an OS would then be a lie we could not fix without
breaking clients that match on it.

| Thing | Value |
|---|---|
| Crate | `scanbus-backend-mobile` |
| Daemon feature | `mobile` |
| `ScannerBackend::id()` | `"mobile"` |
| `Scanner1.Backend` | `"mobile"` |
| `ScannerId` | `ScannerId::from_backend("mobile", <TXT id>)` |
| mDNS service type | `_scanbus-mobile._tcp.local.` (unchanged — already correct) |
| Host mDNS service type | `_scanbus-host._tcp.local.` (§12) |

## 3. The wire protocol

One framing rule for everything: a big-endian `u32` byte count, then that many bytes.
Control frames are UTF-8 JSON; page frames are the encoded image. This is app-specs.md
§2 unchanged, and it is the right call — it is four lines of Kotlin and four lines of
Rust, and no sub-protocol needs a second parser.

What the spec leaves out, and what the host imposes:

- **A length prefix is an allocation request from an unauthenticated peer.** Validate
  before allocating: control frames are capped at 64 KiB, page frames at a configurable
  `max_page_bytes` (default 64 MiB), and a zero length is a protocol error. Read the
  cap, then `read_exact`, never `read_to_end`.
- **Version goes in the frame, not only in TXT.** The mDNS TXT record carries `v=1`, but
  the upload connection (§5) has no mDNS context at all — the app dials a remembered
  address. Both `pair_request` and `upload` carry `"v":1`, and a version this daemon
  does not implement is refused with a named reason rather than a parse failure. This is
  errata against app-specs.md §2 and §3.
- **Unknown fields are ignored, unknown `type` is refused.** Serde defaults on the way
  in, so a later app may add fields; an unrecognised `type` gets
  `{"type":"ack","status":"error","reason":"unsupported"}` and the connection closes.
- **Every read has a deadline.** 5 s for the handshake frames, 30 s per frame during an
  upload. A peer that opens a connection and says nothing must not hold a slot.

Named `reason` values on an error ack — `unauthorized` is the only one app-specs.md
defines, and the app needs to distinguish "re-pair me" from "try again later":

| `reason` | Meaning | What the app should do |
|---|---|---|
| `unauthorized` | unknown `device_id`, or the token does not match | discard the stored pairing, ask the user to pair again |
| `unsupported_version` | `v` is not one this host speaks | tell the user to update the host |
| `not_connected` | paired, but the daemon is not currently accepting for it | retry later |
| `malformed` | framing or JSON the host could not parse | a bug on one side; log it |
| `too_large` | a frame exceeded the cap | scale the image down and retry |

## 4. Pairing, and the six digits the API cannot currently show

`Scanner1.Pair()` is what starts it (app-specs.md §2). The host generates the nonce, so
the host has to display it — and `Scanner1` as specified has no property that can carry
it and no state that means "waiting for a human to look at the phone".

### 4.1 The D-Bus delta

Two additions to [scanbus-dbus-api.md](scanbus-dbus-api.md) §3, deliberately generic
rather than mobile-specific:

```
PairingState : "none" | "pairing" | "awaiting_confirmation"
             | "installing_backend" | "done" | "failed"

PairingInfo  a{sv}  read   {"code": "482913"}   — empty in every other state
```

`awaiting_confirmation` is not a mobile concept. Any backend that needs a human to do
something on the device — a PIN on an eSCL panel, a physical confirm button — lands in
the same state, and a client that renders it correctly once renders all of them. A
mobile-only second interface would have made every client special-case phones in order
to show a code at all.

`PairingInfo` is `a{sv}` and not a bare `Code` string for the same reason `Capabilities`
is: the next backend to reach this state will want to say something slightly different,
and adding a key is not a breaking change.

This revises issue 2.3, which owns the `Scanner1` properties, and §6/§9 of the API
document.

### 4.2 The handshake, host side

1. `Pair()` → `PairingState="pairing"`. The address comes from a discovery record; if
   there is none — the phone was never seen, or the session that saw it ended (2.9) —
   fail with `org.scanbus.Error.NotReachable` rather than dialling a remembered address.
   Pairing is the one moment the host initiates, and it must not initiate at a guess.
2. TCP connect, 5 s timeout. Send `pair_request` with a nonce drawn from a CSPRNG,
   uniform over `000000`–`999999`, formatted with leading zeros.
3. `PairingState="awaiting_confirmation"`, `PairingInfo={"code":…}`,
   `PropertiesChanged`. This is where a UI shows the code next to the phone's name.
4. Wait for `pair_response`, 120 s. Somebody has to pick up a phone and read a screen;
   anything under a minute turns a normal pairing into a failure. On timeout:
   `PairingState="failed"`, `PairingError="the phone did not confirm within 120 s"`.
5. `accepted:false` → `failed`, `PairingError="rejected on the device"`.
6. **`device_id` in the response must equal the `id` in the TXT record.** They come over
   different paths and a mismatch means the thing that answered is not the thing that
   advertised. Without this check a phone on the network could answer a pairing meant
   for another and take over its `ScannerId` — and with it, its token slot.
7. Store `device_id`, the token and `capabilities.profiles` (§8), clear `PairingInfo`,
   `Paired=true`, `PairingState="done"`.

`CancelPairing()` closes the socket. The app sees the connection drop and dismisses its
dialog; there is no cancel message, and adding one would only cover the case where the
host is still alive to send it.

### 4.3 Where the handshake lives in the trait

`ScannerBackend` has no `pair()` — pairing is `ensure_installed` followed by
`start_listening` (implementation plan §3, issue 1.4). For mobile there is nothing to
install, and app-specs.md §4 concludes from that that `ensure_installed` returns
`Ok(())` immediately. That leaves the handshake with no home.

So it goes *in* `ensure_installed`, which is already the method that takes an
`mpsc::Sender<PairingProgress>` and is already the step the pairing state machine treats
as "the slow part that must not block `Pair()`". Waiting on a human is exactly that. It
needs one new `PairingProgress` variant:

```rust
PairingProgress::AwaitingConfirmation { code: String }
```

which is what moves 1.4's state machine into `awaiting_confirmation`. This is a smaller
change than a `pair()` method on the trait, and it keeps one answer to "which call can
take two minutes".

### 4.4 `Unpair()` needs a trait method that does not exist

Revoking the token is backend state, and nothing on `ScannerBackend` says "forget this
scanner". `stop_listening` is not it — a paired phone whose listener is stopped must
still be recognised when it uploads.

Add `async fn forget(&self, scanner_id: &ScannerId) -> Result<(), BackendError>`, with a
default `Ok(())`. Brother needs the same hook to drop its `brscan-skey.config` entry
(5.4), so this is two callers, not one.

After `forget`, an upload bearing the old token gets `unauthorized` and no `Job1`. The
phone is not notified — there is no channel to notify it on — and learns at its next
upload. That is the intended design: `Unpair()` on the host must work with the phone
switched off.

## 5. One listener, on a port that must outlive the daemon

There is no per-scanner `start_listening` in the network sense: a single TCP listener
serves every paired phone (app-specs.md §4). It is bound once, by the backend, at
construction — before the bus name appears (4.2) — and `start_listening(scanner)` only
subscribes to a `device_id`-filtered view of it.

**The port cannot be ephemeral.** app-specs.md §1 says "ephemeral" and §2 says
`upload_port` is communicated once during pairing so the app never needs discovery
again. Both cannot be true: a port picked afresh at each start silently breaks every
phone paired before the last restart, and the failure looks like "the app says sent, the
computer has nothing".

So:

- `mobile.upload_port` in the daemon config. `0` — the default — means *pick one now and
  write it down*, not *pick one each time*. The chosen port is persisted with the device
  table (§8) and reused forever after.
- If the persisted port is taken at startup, that is a hard, loud failure: log it, and
  every mobile scanner comes up `Status="offline"`. **Do not silently re-pick.** A
  re-pick trades one visible failure for N invisible ones.
- Bind `[::]` with dual-stack when available, `0.0.0.0` otherwise. It has to be
  reachable from the LAN; there is no useful loopback-only mode except in tests.

Caps, because this socket is open to the local network:

| | Default |
|---|---|
| Concurrent authenticated uploads | 8 |
| Connections awaiting their first frame | 16, 5 s deadline each |
| Control frame | 64 KiB |
| Page frame | 64 MiB (`mobile.max_page_bytes`) |
| Pages per job (`of`) | 200 |

## 6. From an upload to a `Job1`

An upload arrives with its profile chosen and its bytes on the same connection. The
trait expects `start_listening` to yield `ButtonPressedEvent` and `fetch_pages` to be a
pull the daemon makes afterwards. Two amendments to issue 1.3 reconcile them.

### 6.1 The trigger is not always a button

```rust
pub struct ScanTrigger {
    pub id: TriggerId,
    pub scanner_id: ScannerId,
    pub kind: TriggerKind,
    pub timestamp: SystemTime,
}

pub enum TriggerKind {
    /// A physical key. The profile lives in `Button1.Profile` on the host.
    Button { index: u32 },
    /// The device chose the profile and is sending the data now.
    Push { profile: ProfileKind },
}
```

Issue 1.3 argues that a backend reporting a profile "is telling us something it learned
from a config file *we* wrote". That argument is exactly right for Brother and does not
apply here: the profile in an upload was chosen by a human in the app, on the phone, and
the host has never written anything about it. `Push` carries a profile because the
profile genuinely originates there.

`Job1.Button = -1` for `Push`, which app-specs.md §3 already requires and which §4 of
the API already defines as "not triggered by a key".

Profile precedence for a mobile job: the upload's profile wins over the session profile
from `Connect(options)` and over `DefaultProfile`. It is the most specific and the most
recent statement of intent. Precedence for the other cases is 2.4's.

### 6.2 `fetch_pages` should take the trigger, not a job id

`fetch_pages(scanner_id, job_id)` asks the backend to key a page stream by an identifier
the backend has never seen — `job_id` is minted by the daemon after the trigger arrives.
For Brother that works by accident, because there is one scan in flight per scanner. For
mobile, two uploads from one phone would race, and the backend's only recovery would be
FIFO guesswork.

`fetch_pages(&self, trigger: &TriggerId)` removes the guess. The backend hands out the
id, the daemon hands it back, and the correlation is exact. 1.3's "callable exactly once
per job id" becomes "exactly once per trigger id" and stays true.

### 6.3 Ack semantics

The ack for page *n* is sent once its bytes are in the daemon's page stream — not once
the profile pipeline has finished with them. PDF assembly happens after `page == of` and
can take seconds; an app whose progress bar waits on it looks hung.

The gap this leaves is real and is not closed here: a job that fails during processing
(no disk space, unwritable output directory) has already been acked as `ok`. The app
reports success, the host reports an error, and only the host is right. Closing it needs
a fourth message type — see §10.

### 6.4 The failure cases

- Connection drops mid-job → `Job1.State="error"`,
  `Error="connection lost after page 2 of 3"`, **pages received so far are discarded.**
  A three-page PDF containing two pages is worse than no PDF, because it looks fine.
- `page` not `1`-based and strictly incrementing, or `of` changing between frames →
  error ack `malformed`, close, job errors.
- Upload for a `device_id` that is paired but has no active subscriber → `not_connected`
  and no `Job1`. Reachable when an upload lands during daemon startup.
- Backpressure is TCP's: the page channel is bounded, and a stalled pipeline stops the
  socket being read. This is the correct behaviour — the phone waits.

## 7. `Status`, `Connect`, and reachability the host cannot observe

`Connect()`/`Disconnect()` are functional no-ops (app-specs.md §4): they set `Connected`
and nothing goes on the wire. That part is straightforward.

`Status` is not. The API says `Status` is reachability of the *device* (§3, §9), and for
a paired phone the host has no way to know it: it never dials, and mDNS is not a proxy
for it — Android stops advertising when the app is not in the foreground, so a perfectly
usable phone in a pocket would read `offline`, and a client would grey out the scanner
that is about to send a scan.

So, for mobile only, and documented as such: **`Status` reports the host's readiness to
receive.** `online` whenever the shared listener is bound; `offline` when it is not.
`busy` while an upload is in flight for that device. An unpaired phone seen during
discovery is `online` because it is, by definition, advertising right now.

This is a deviation from §9's "reachable ≠ paired" reading, and the honest alternative —
inventing a heartbeat so the host can observe the phone — is a protocol addition that
buys a status field and costs battery.

## 8. Persistence: the token is the backend's, not the daemon's

The pairing store of 4.1 holds the host's half of a pairing — `Paired`, button
assignments, profile options — as `scanbus-core`'s model documentation already puts it:
the backend's `ScannerInfo` and the daemon's registry state are deliberately separate,
so that a rediscovery cannot reset a pairing.

The token is the backend's half, and it stays there:
`$XDG_DATA_HOME/scanbus/mobile/devices.json`, mode `0600`, holding per device the
`device_id`, the profiles it advertised, a timestamp, and the token — **stored as a
SHA-256 hash, not in clear.** The host only ever needs to compare, comparison is against
a hash just as easily, and a readable file is one backup or one screen-share away from a
leaked credential. Compare in constant time regardless: the token is a bearer secret.

The same file holds the chosen `upload_port` (§5).

Two stores means they can disagree. The reconciliation rule at startup:

- daemon says paired, backend has no token → `Paired=false`, `PairingState="failed"`,
  `PairingError="the pairing secret is missing; pair the phone again"`. Silently
  reporting `Paired=true` would produce a scanner that can never receive anything and
  never says why.
- backend has a token, daemon knows no such scanner → drop the token. It is unreachable
  state.

The alternative — a `PairingProgress::BackendState(json)` variant the daemon persists
opaquely on the backend's behalf — keeps one store and puts a secret through the
daemon's serialiser and into its log statements. It is worth revisiting if a third
backend needs persistent state, and not before.

## 9. Testing without a phone

The entire protocol is loopback-testable, which makes this the first backend whose
acceptance criteria do not read "plug in a printer":

- The codec and the message types are pure and unit-tested (9.1) — truncated frames,
  oversized lengths, unknown `type`, `v` of 2.
- Integration tests drive both sides over a loopback socket with mDNS bypassed by
  injecting the address, covering rejection, timeout, bad token, dropped connection
  mid-page, and a 3-page document.
- `scanbus-mobile-sim`, a dev binary that advertises over mDNS and plays the app: pair,
  then upload N pages of a fixture image. This is what lets the daemon, the CLI (8.x)
  and the GNOME extension be exercised end to end while the Android app is still being
  written, and what the app itself can be diffed against when it is.
- The conformance suite (2.8) gains a mobile scenario on its private bus.

CI compiles and tests this backend by default, unlike `brother` and `hplip`: it shells
out to nothing and needs no hardware. The `mobile` feature exists to allow it to be
turned *off*, not because it is exotic.

## 10. Errata and open questions for app-specs.md

Things the app has to do that its own specification does not say, or says wrongly. Each
needs agreement in the `scanbus_android_app` repository before the app implements it.

1. **The port in §1 must not be ephemeral** in the sense of "different each run" — see
   §5. The pairing port genuinely can be ephemeral; the host's `upload_port` cannot.
2. **`"v":1` in `pair_request` and `upload`** — §3 above.
3. **The named `reason` values** an app must handle — §3 above. In particular
   `unauthorized` means *discard the stored pairing*, not *retry*.
4. **Which host address the app dials.** §3 says `{host_ip}` without saying where it
   comes from. It is the source address of the pairing connection — the interface the
   host actually reached the phone on. A `host_ip` field in `pair_request` would be
   wrong for exactly the multi-homed and VPN cases that make the question interesting.
5. **A host whose DHCP lease changes breaks every paired phone**, permanently, because
   §2 removes discovery from the upload path. Agreed, and specified in §12: the host
   advertises `_scanbus-host._tcp` with `id=<host_id>`, and the app browses for its
   paired `host_id` only after a stored address has refused a connection. This entry
   used to call it recommended and out of the base protocol; §12 is what both sides are
   building.
6. **`page`/`of` bounds**: 1-based, strictly incrementing, `of` constant for the life of
   the connection, `of` ≤ 200.
7. **A per-job final status.** §6.3 above: the ack means "received", and there is no
   message that means "your document was written". A `{"type":"job","status":…}` frame
   the host sends after the pipeline finishes would close it. Deferred, because it turns
   a fire-and-forget upload into a session the app has to keep alive.
8. **TLS** — agreed, and specified in §11. This entry used to propose a fingerprint in a
   `cert_sha256` field of `pair_response`; that is *not* what either side is building,
   and §11.1 says why. Until a host implements §11 the security model is "a trusted home
   network", and it should be said out loud in the app's pairing screen rather than only
   in a specification.

## 11. TLS, and the one moment a fingerprint can be believed

§10.8 used to describe this as "a self-signed certificate generated by the host, its
fingerprint in `pair_response`, pinned by the app". The app repository worked it through
in its issue 5.2 and arrived somewhere better; what follows is the agreed shape and it
supersedes that entry.

The whole of it in one sentence: **the host holds one self-signed certificate, presents
it as a client certificate on the pairing connection it dials and as the server
certificate on the upload listener, and the app pins the SHA-256 it saw during the
pairing handshake.**

### 11.1 Why the fingerprint is not a field in `pair_response`

A fingerprint in a JSON field is an *assertion*: the peer states which certificate it
holds. A certificate presented in a TLS handshake is a *proof*, because the peer had to
sign with the matching private key to finish the handshake. Both cost about the same to
implement and only one of them is evidence.

The pairing connection has to be TLS regardless — it is the connection the token travels
on, and a token crossing the LAN in clear is the entire weakness app-specs.md §5 admits
to. And that connection is dialled *by the host* (§1), which makes the host the TLS
**client** on it. So the certificate the app needs to pin can arrive at the one place it
is provable, as a client certificate, before the first frame is read and therefore before
the six digits reach a screen.

A `cert_sha256` field would then be a second, weaker copy of something the handshake has
already established, with a new failure mode nobody has an answer for — the field and the
certificate disagreeing. It is not in the protocol, and `pair_response` is unchanged by
this section.

The reason pinning happens *here* rather than at first upload is the same reason the
token is believed: pairing is the only moment a human confirms that the machine on the
other end is the one in front of them. A certificate learned at first upload is pinned to
whatever answered.

### 11.2 One certificate, two roles

This is the load-bearing requirement, and it is easy to get wrong from the host's side
because the two uses are in different parts of the backend:

| Connection | Who dials | Host's TLS role | What the host presents |
|---|---|---|---|
| Pairing (§4.2) | the host | client | the certificate, as a **client** certificate |
| Upload (§5) | the app | server | **the same** certificate |

The pin the app stores is the SHA-256 of the DER of the leaf certificate it saw while
pairing. If the listener later presents a different one — a second certificate generated
for the server role, a rotated one, a per-connection one — then every phone paired over
TLS refuses to upload, permanently and by design, and the user is told to pair again. One
certificate, one key, loaded once at startup, handed to both configurations.

`ECDSA P-256` with `SHA-256`, because that is what the phone's own key is and what every
Android JSSE and every `rustls` build can verify. Ed25519 is not universally available on
the Android versions this protocol supports.

### 11.3 Dialling the pairing port

The phone advertises **`tlsport=<port>`** in its mDNS TXT record when it has a keystore
identity to serve. Absent means it has none — there is no `tls=0` — so a record from a
phone written before this change reads exactly as it always did.

- `tlsport` present → dial that port and speak TLS. Absent → dial the SRV port in
  cleartext, as before.
- **The SRV port stays cleartext, always.** It is the only port a host built before this
  section knows how to find, and such a host will dial it and expect the framing it has
  always found there.
- **No retry in either direction.** A phone that advertised `tlsport` and then cannot
  complete a handshake is broken, and a silent cleartext retry would hide that. More to
  the point, an automatic downgrade path is a thing an attacker gets to trigger.
- The `v` in TXT stays `1`. TLS is not a protocol version: every combination of old and
  new host and app interoperates through the port choice alone, and burning a version
  number would strand phones for no gain.

**Why a port and not a `tls=1` flag on the existing one.** The neater design is a single
port that reads the first byte — `0x16` is a TLS `ContentType.handshake`, `0x00` is the
top byte of our length prefix — and hands the socket and that byte to the TLS layer. It is
what the host's own upload listener does (§11.4), and **Android cannot do it**: the
`SSLSocketFactory.createSocket(Socket, InputStream, boolean)` overload that takes already
consumed bytes is Java SE only and has never existed on Android, and the obvious
workaround — wrapping the socket so its `getInputStream()` prepends the byte — is unsound
there too, because on the Android versions where Conscrypt selects its file-descriptor
socket the TLS code reads the descriptor directly and the byte is silently dropped. So the
phone binds two ports and the one that was dialled is what says which protocol is in use.
This is a constraint of the platform on the phone's side of the connection only; nothing
about it applies to §11.4.

**The phone's certificate is not verified, and must not be.** It is a self-signed
certificate from a keystore the host has never seen, with `CN=scanbus` and no name that
could match the address being dialled. `rustls` needs a custom `ServerCertVerifier` here
that accepts anything. Two constraints on that verifier, both worth a comment at the site:

- it is scoped to the pairing `ClientConfig` alone, never to a config anything else can
  reach;
- it is *not* the security hole it looks like. Nothing on this host could vouch for a
  phone it has never spoken to, the phone is not the identity being established — the
  host is — and what authenticates the pairing in both directions is the six digits.

The host's client certificate is sent in response to the phone's `CertificateRequest`,
which carries no `certificate_authorities` because the phone accepts any issuer. A TLS
client library that only offers a certificate when it recognises an advertised CA will
silently send nothing here, the phone will refuse the pairing with "the computer sent no
certificate to pin", and the failure will look like a phone bug. Verify this against the
real app, not only against a Rust peer.

TLS 1.3 where both ends have it, **1.2 accepted**: the app enables the best version its
platform offers, and Android 8 and 9 offer 1.2. Requiring 1.3 would refuse those phones
outright for a benefit the pin already provides.

**An EOF in this handshake is an app-side key, until proved otherwise** (#78). The phone
serves this port with an `AndroidKeyStore` key, which is opaque: Conscrypt cannot give it
a message to hash, so it hashes the TLS transcript itself and signs the digest raw, as
`NONEwithECDSA`. A key generated without `DIGEST_NONE` refuses that operation — keystore2
answers `INCOMPATIBLE_DIGEST` — and it refuses it while producing `CertificateVerify`,
past `ServerHello`, where no alert can still be sent. The phone's socket simply closes and
this host has nothing to report but `tls handshake eof`. The requirement belongs to the
app's spec and is recorded there; what belongs here is that the host's own diagnosis of
that error string is "the phone could not sign", not "the phone does not speak TLS", and
that a Rust-to-Rust test cannot reach the failure at all — a keystore key is the only
thing that has it.

### 11.4 The upload listener takes both, and the first byte says which

The listener cannot simply become TLS: every pairing made before this change has no
fingerprint, so those phones dial cleartext and must keep working (§11.6). One port
serves both, decided by one byte:

| First byte | What it is |
|---|---|
| `0x16` | TLS `ContentType.handshake`; every TLS connection there is opens with it |
| `0x00` | the top byte of our `u32` length prefix — the first frame is a control frame capped at 64 KiB (§3), so it cannot be anything else |
| anything else | not this protocol; close the connection |

This is not a heuristic and it is not a guess with a fallback. Use `TcpStream::peek`
rather than a read: `peek` leaves the byte where the TLS acceptor expects to find it. This
is the demux the phone cannot do on its own listener (§11.3) and the host can, which is
why the two sides are shaped differently — one port here, two there.

Three rules on top of the demux:

- **No client certificate is requested.** The app has no identity the host could check
  and the token is what authenticates an upload. Asking for one would only produce
  handshake failures to debug.
- **A device paired over TLS may not upload in cleartext.** The device table records
  whether the pairing was encrypted; if it was, a cleartext upload bearing that
  `device_id` is answered `unauthorized` and the connection closes. Without this the pin
  buys nothing on the host side — a stolen token would just be replayed on the cleartext
  path — and `unauthorized` is the right reason because the app's documented response to
  it (discard the pairing, pair again) is exactly the repair.
- **A device paired in cleartext may upload over TLS.** It is accepted: the token still
  authenticates it, and encryption is never worse than none. There is no pin to check,
  and the host does not invent one.

The TLS handshake happens inside the "connections awaiting their first frame" budget of
§5 — 16 slots, 5 s each — not before it. A handshake that stalls is exactly the case that
budget exists for.

### 11.5 Where the key lives, and why it is never rotated silently

Next to the device table of §8: `$XDG_DATA_HOME/scanbus/mobile/`, the key file mode
`0600`, generated at first start if it is not there. The device store gains a per-device
flag recording whether the pairing was made over TLS, which bumps its version.

- **Generated once.** The certificate carries no assertion anybody validates — the app
  ignores dates, chains and names on purpose — so a validity window is a formality and
  should be wide. Renewal on expiry would invalidate every pairing on the host to refresh
  a claim nothing reads.
- **A key that cannot be read is a hard, loud failure**, exactly like the taken
  `upload_port` of §5: log it and bring the mobile scanners up `offline`. Regenerating
  over an unreadable key is the one mistake in this section that cannot be walked back —
  it silently breaks every paired phone at once, and each one surfaces later as a
  mysterious refusal on the next scan.
- **Rotation is a deliberate act with a known cost**: every phone paired over TLS must
  pair again. There is no "the certificate changed, continue?" path on either side, and
  adding one would train away the only defence the design has.

`mobile.require_tls` (default `false`) is the switch for the office network app-specs.md
§5 says the base protocol is not good enough for: when set, a phone that does not
advertise `tlsport` is not paired at all, and a cleartext upload is refused whatever the
device table says.

The daemon has no configuration file, so until it has one the key is read from the
environment as `SCANBUS_MOBILE_REQUIRE_TLS`, which is a line a systemd unit already knows
how to carry. Two consequences worth stating rather than discovering:

- **A value that is neither spelling leaves the switch off**, loudly. Aborting the daemon
  over an environment variable would take down a host that worked until somebody edited a
  unit file; an operator who sets this wants their phones encrypted, not their host
  stopped. The `warn` line is the only thing standing between a typo and traffic in clear.
- **The pairing refusal names the variable.** "This phone is too old for this network" and
  "somebody set a variable" are the same screen from the front of a GUI, and only one of
  them is worth calling anybody about.

### 11.6 What this buys, and what it does not

- A passive observer on the LAN no longer sees the token or the pages. That is the whole
  of what §5 of app-specs.md says is missing, and it is bought outright.
- A machine that later impersonates the host on the upload port is refused by the pin,
  with no prompt and no way for a user to click through it.
- **An active attacker at pairing time is still bounded by the six digits, not by TLS.**
  mDNS is unauthenticated: an attacker who can forge records can strip `tlsport` and get a
  cleartext pairing. What they get from it is a pairing the app marks as unencrypted and
  warns about — the visible-downgrade case is the reason the app has that marker at all,
  and the reason the host must not paper over it with a retry.
- Pairings made before TLS keep working, in cleartext, forever. The host has no
  fingerprint for them and never will; migrating them to "must be encrypted" is not
  something either side can do, and refusing them would break working pairings to gain
  nothing.

### 11.7 Nothing changes on D-Bus

No new property, no new error, no fingerprint in `PairingInfo`. Showing the fingerprint
next to the six digits was considered and rejected: it asks a human to compare 64 hex
characters that they have no second copy of, and the six digits already carry the
confirmation this design depends on. A client that wants to say "encrypted" can say it
from the backend's own state; it is not part of the pairing contract.

## 12. Being found again after the host's address changes

§10.5 used to describe this as a recommendation, "not in the base protocol, and it needs
the app side to agree". The app repository worked it through in its issue 5.1 and asked
for the host half first; what follows is the agreed shape and it supersedes that entry.

The problem is created by a decision that is otherwise right. app-specs.md §2 hands the
app an `upload_port` and §3.1 has it remember the address it was dialled from, so no
mDNS stands in front of an upload. That is what makes a sleeping phone, NAT and a
changing *phone* address irrelevant (§1). It also means **a DHCP lease change on the
host breaks every paired phone permanently** — not until a retry, not until a reboot —
with a failure that reads *could not reach your computer* about a computer that is
sitting right there, working.

The fix is one mDNS record, and it stays off the happy path: the app browses **only**
after a stored address has refused a connection.

### 12.1 The record

| | |
|---|---|
| Service type | `_scanbus-host._tcp.local.` |
| Instance name | the `host_name` of §4.2 — what the phone already shows next to the six digits |
| SRV port | the bound `upload_port` (§5) |
| TXT `id` | `host_id`, the same string `pair_request` carries — 32 lowercase hex characters |
| TXT `v` | `1` |

Four things follow from that table rather than being additional rules:

- **It is registered after the listener binds, from the bound port.** Not from the
  configured value: §5 already refuses to re-pick a taken `upload_port`, and publishing a
  port that failed to bind would hand the app an address that resolves and then refuses,
  which is the exact failure this section exists to end.
- **One record per host, not one per paired phone.** Nothing in it is per-device. The app
  matches on `id` and authenticates with its token; a record per pairing would leak how
  many phones are paired to anyone with `avahi-browse`.
- **There is no `tlsport`.** §11.4 demuxes TLS from cleartext on the first byte, so the
  host has exactly one upload port and the SRV record names it. The asymmetry with the
  phone's record (§11.3), which needs two ports, is the demux the phone cannot do and we
  can — not an oversight in one of them.
- **`v` stays `1`.** This is not a protocol version: a host that advertises and a host
  that does not both speak version 1, and the app finds out which it has by finding a
  record or not finding one. Burning a version number would strand phones for no gain,
  which is the same argument §11.3 makes about TLS.

### 12.2 `host_id` has to be persisted, and today it is not

`MobileBackend::host_id` is drawn from the CSPRNG at construction and never written down.
For `pair_request` that is enough — it only has to be stable for the length of one
handshake — and for this record it is precisely wrong: the phone stores the id it saw
while pairing and later looks for **that** id, so a host that redraws it at every start
is invisible to every phone paired before its last restart. It is §5's re-picked port
arriving by a second route, and it earns the same answer.

`host_id` therefore joins `upload_port` in `devices.json` (§8): drawn once, written down,
reused forever. An absent field means *mint one and write it*, which is what a fresh
store does anyway, so it is a `#[serde(default)]` addition and not a store version bump.

- **It is not derived from `/etc/machine-id`, a MAC address or the hostname.** An
  identifier this host broadcasts on every network it joins should not be a function of
  the machine's identity, and a random 128-bit value costs nothing and gives up nothing.
  A hostname-derived id would also break on the one event — the machine being renamed —
  that must not invalidate a pairing.
- **Phones paired before this lands keep the id they stored, which will never match a
  record.** They go on working: nothing on the upload path reads `host_id`. They lose
  only the recovery added here, which is the state they are in today, so there is no
  migration and none is offered — the *pair again* route (§12.4) is what covers them.
- The value in `pair_request` is unchanged in shape and merely becomes stable. The app
  compares TXT `id` to what it stored byte for byte, so the two must be the same string,
  not the same value differently formatted.

### 12.3 What the host does not do

- **It never browses `_scanbus-host._tcp`.** The host publishes; nothing here consumes.
  If two hosts collide on an instance name, mDNS renames one of them and no one notices:
  the app matches on TXT `id`, never on the name.
- **Advertising is not gated on a phone being paired.** It is up whenever the listener is.
  Gating it would make the record's presence a function of state the app cannot see, so a
  phone that fails to find its host could no longer tell *the host moved* from *the host
  forgot me* — and the record on an unpaired host is a hostname and a random number on a
  LAN that is already full of printers saying more than that.
- **It is unregistered at shutdown**, so the goodbye packet clears the cache. This is
  politeness rather than correctness: a stale record costs the app one resolve and one
  refused connection, which is the failure it was about to report anyway.
- **Nothing changes on D-Bus, and the host still never dials a phone** after pairing
  (§1). This record is the host saying where it is, not the host reaching for anything.

### 12.4 What the app owes, so the two halves can be checked

Built in the app repository's issue 5.1, listed here because neither half is testable
against a description of the other: browse only after a stored address refuses a
connection, one attempt per send within a 5 s budget, match `id` exactly and ignore a
sole instance that does not match, replace the stored address and port only after the
retried connection succeeds, release the multicast lock in every path — and keep the
*this computer moved, pair again* route, because a host that is older than this section
or on another subnet entirely will keep existing.
