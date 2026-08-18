//! Wire protocol primitives and backend plumbing for `scanbus-backend-mobile`.

pub mod tls;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_core::{Stream, stream::BoxStream};
use if_addrs::{IfAddr, get_if_addrs};
use rand::Rng;
use rustls::pki_types::ServerName;
use scanbus_backend_common::mdns;
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities as ScannerCapabilities, ProfileKind, RawPage,
    RestoreDisposition, ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tracing::{debug, info, warn};

/// Backend identifier, as reported by `ScannerBackend::id`.
pub const ID: &str = "mobile";

/// Protocol version this crate implements.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum size of any JSON control frame (64 KiB by default).
pub const DEFAULT_CONTROL_MAX_BYTES: usize = 64 * 1024;

/// Maximum size of a page frame (64 MiB by default).
pub const DEFAULT_PAGE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Maximum allowed value for `of` in an upload header.
pub const MAX_PAGES_PER_JOB: u32 = 200;

/// Service type used by mobile app instances.
pub const SERVICE_TYPE: &str = "_scanbus-mobile._tcp.local.";

/// Service type this host publishes so a paired phone can find it again once the address
/// it stored stops answering (§12.1).
///
/// Publishing only: §12.3 is explicit that the host never browses this type, so it must
/// not join the discovery list [`SERVICE_TYPE`] is in.
pub const HOST_SERVICE_TYPE: &str = "_scanbus-host._tcp.local.";

/// TXT `v` of that record. Not a protocol version — a host that advertises and a host
/// that does not both speak version 1, and the app learns which it has by finding a
/// record or not finding one (§12.1).
const HOST_RECORD_VERSION: &str = "1";

/// How long one `discover()` call browses mDNS before returning.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the TCP connect for a pairing handshake may take (mobile-backend.md §4.2).
pub const PAIR_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the host waits for `pair_response` after showing the code — long enough for
/// someone to pick up a phone and read a screen (mobile-backend.md §4.2).
pub const PAIR_CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a sighting stays dialable after the round that made it.
///
/// §4.2 forbids dialling a *remembered* address: the record has to come from a phone
/// that is advertising itself now, so that `NotReachable` is the answer once the
/// discovery session that saw it has ended. It is deliberately a few probe intervals
/// (2.9 re-probes every 5 s) rather than one round: a single mDNS browse that misses a
/// phone present on the network is packet loss, and turning that into a failed pairing
/// would be inventing an outage. A record older than this is not evidence of anything.
pub const DISCOVERY_RECORD_TTL: Duration = Duration::from_secs(30);
pub const UPLOAD_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
pub const FETCH_CLAIM_TIMEOUT: Duration = Duration::from_secs(5);
pub const UPLOAD_PAGE_BUFFER: usize = 2;
pub const DEFAULT_UPLOAD_RESOLUTION_DPI: u32 = 300;

/// Where `mobile.require_tls` (§11.5) is read from.
///
/// The daemon has no configuration file yet — nothing in it is configurable, and adding
/// a file, a format and a reload path to carry one boolean would be a subsystem this
/// issue did not ask for. The environment is what a systemd unit already has a line for
/// (`Environment=SCANBUS_MOBILE_REQUIRE_TLS=1`), which is exactly the office-network
/// deployment §11.5 describes. When a configuration file does land, `mobile.require_tls`
/// is the key this becomes and this variable is the fallback for it.
pub const REQUIRE_TLS_ENV: &str = "SCANBUS_MOBILE_REQUIRE_TLS";

/// What this crate writes. Bumped to 2 by §11.5's per-device `tls` flag.
///
/// The number is what a *newer* scanbus writes, not a compatibility gate: older stores
/// are read and upgraded in place (see [`load_store`]), because refusing one would take
/// every pairing on the host with it.
const DEVICE_STORE_VERSION: u32 = 2;

/// What one sighting of a phone is worth to a pairing: an address to dial, and the TXT
/// `id` that `pair_response.device_id` has to match.
///
/// The `device_id` is kept verbatim rather than recovered from the [`ScannerId`],
/// because the comparison in §4.2 step 6 is the whole mitigation and it has to be
/// against what was actually advertised, not against a round trip through escaping.
#[derive(Debug, Clone)]
struct DiscoveryRecord {
    device_id: String,
    address: String,
    /// Where to dial for a TLS pairing, from the `tlsport` TXT key (§11.3), or `None`
    /// when the phone advertised none and has no keystore identity to serve.
    ///
    /// The port is read as a port and then formatted against the same address the SRV
    /// record gave, rather than kept as a bare `u16`, because the only thing it is ever
    /// used for is a dial to that same phone: doing it here keeps the IPv6 bracketing
    /// hazard `address` already documents in one place, and hands the dial of §11.3
    /// something it can pass to [`MobileBackend::connect_to_record`] unchanged.
    tls_address: Option<String>,
    seen_at: Instant,
}

/// What a completed handshake left behind for a scanner.
///
/// Provisional storage: the durable, cross-restart device table is [`9.4`]'s, and it
/// also owns the upload port. This map is what makes [`ScannerBackend::forget`] have
/// something to revoke in the meantime, and it is small enough for 9.4 to replace
/// outright rather than to build alongside.
///
/// [`9.4`]: https://github.com/jeanparpaillon/scanbus_service/issues/43
#[derive(Clone)]
struct PairedDevice {
    device_id: String,
    /// SHA-256 of the phone's upload credential.
    token_sha256: String,
    profiles: Vec<ProfileKind>,
    paired_at: u64,
    /// Whether the handshake that produced this record ran over TLS (§11.5).
    ///
    /// This is the only record anywhere that the phone pinned our certificate: the app
    /// keeps the fingerprint, we keep the fact that it has one. It is what makes a
    /// cleartext upload bearing this `device_id` answerable `unauthorized` (§11.4)
    /// instead of trusted, and it cannot be recovered later — a pairing whose flag is
    /// lost can only be re-made by a human.
    tls: bool,
}

/// Redacted on purpose: the token must not reach a log through a `?device` that seemed
/// harmless at the call site.
impl fmt::Debug for PairedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedDevice")
            .field("device_id", &self.device_id)
            .field("token_sha256", &"<redacted>")
            .field("profiles", &self.profiles)
            .field("paired_at", &self.paired_at)
            .field("tls", &self.tls)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDeviceStore {
    version: u32,
    #[serde(default)]
    upload_port: u16,
    /// What `pair_request.host_id` carries, and what §12's `_scanbus-host._tcp` record
    /// puts in TXT `id`. It sits next to `upload_port` for the same reason: the phone
    /// stores it while pairing and looks for *that* id afterwards, so a value redrawn
    /// at every start would make this host invisible to every phone paired before its
    /// last restart.
    ///
    /// Empty means *mint one and write it* — which is what a fresh store means too, so
    /// this is a `#[serde(default)]` addition and not a `DEVICE_STORE_VERSION` bump
    /// (§12.2). Unlike the `tls` flag of §11.5, the default here claims nothing about a
    /// past pairing.
    #[serde(default)]
    host_id: String,
    #[serde(default)]
    devices: BTreeMap<ScannerId, PersistedDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDevice {
    device_id: String,
    token_sha256: String,
    #[serde(default)]
    profiles: Vec<ProfileKind>,
    paired_at: u64,
    /// Absent in a version 1 record, and `false` is the truth about one: it was written
    /// before this host could speak TLS at all, so that pairing was made in cleartext
    /// (§11.6). Defaulting the other way would claim a pin nobody holds and lock the
    /// phone out of the only path it knows.
    #[serde(default)]
    tls: bool,
}

impl Default for PersistedDeviceStore {
    fn default() -> Self {
        Self {
            version: DEVICE_STORE_VERSION,
            upload_port: 0,
            host_id: String::new(),
            devices: BTreeMap::new(),
        }
    }
}

impl From<&PairedDevice> for PersistedDevice {
    fn from(device: &PairedDevice) -> Self {
        Self {
            device_id: device.device_id.clone(),
            token_sha256: device.token_sha256.clone(),
            profiles: device.profiles.clone(),
            paired_at: device.paired_at,
            tls: device.tls,
        }
    }
}

impl From<PersistedDevice> for PairedDevice {
    fn from(device: PersistedDevice) -> Self {
        Self {
            device_id: device.device_id,
            token_sha256: device.token_sha256,
            profiles: device.profiles,
            paired_at: device.paired_at,
            tls: device.tls,
        }
    }
}

#[derive(Debug)]
struct ListenerBinding {
    port: u16,
    bind_error: Option<String>,
    listener: Option<std::net::TcpListener>,
    task_started: bool,
}

/// The `_scanbus-host._tcp` record this host publishes (§12.1), decided before anything
/// touches the network.
///
/// A type rather than three arguments to [`advertise_host`], because §12.1 is a rule
/// about *which* port: this is built from the [`ListenerBinding`] and never sees
/// `mobile.upload_port`, so the configured value is not in scope to be reached for by
/// mistake. Deciding it separately from publishing it is also what lets the record be
/// checked without starting a responder.
struct HostRecord<'a> {
    /// §12.1's instance name — this host's `host_name`.
    instance: &'a str,
    /// The bound port, which is the only port a phone could reach anyway.
    port: u16,
    /// TXT, in order and nothing else. `id` is the same string `pair_request` carries,
    /// byte for byte: the app compares what it stored at pairing against this, so a
    /// different formatting of the same value is a different host to the phone (§12.2).
    txt: [(&'a str, &'a str); 2],
}

impl<'a> HostRecord<'a> {
    /// `None` when the listener never bound. A record naming a port nothing listens on
    /// resolves and then refuses the connection — the exact failure §12 exists to end,
    /// arrived at by the one route §12 cannot fix.
    fn new(host_id: &'a str, host_name: &'a str, listener: &ListenerBinding) -> Option<Self> {
        listener.is_bound().then_some(Self {
            instance: host_name,
            port: listener.port,
            txt: [("id", host_id), ("v", HOST_RECORD_VERSION)],
        })
    }
}

#[derive(Debug)]
struct PendingUpload {
    scanner_id: ScannerId,
    claim: Option<oneshot::Sender<()>>,
    receiver: mpsc::Receiver<Result<RawPage, BackendError>>,
}

struct MobileSubscription {
    scanner_id: ScannerId,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    receiver: mpsc::Receiver<scanbus_core::ScanTrigger>,
}

impl MobileSubscription {
    fn new(
        scanner_id: ScannerId,
        subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
        receiver: mpsc::Receiver<scanbus_core::ScanTrigger>,
    ) -> Self {
        Self {
            scanner_id,
            subscriptions,
            receiver,
        }
    }
}

impl Stream for MobileSubscription {
    type Item = scanbus_core::ScanTrigger;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(cx)
    }
}

impl Drop for MobileSubscription {
    fn drop(&mut self) {
        self.subscriptions
            .lock()
            .expect("mobile subscriptions lock poisoned")
            .remove(&self.scanner_id);
    }
}

impl ListenerBinding {
    fn is_bound(&self) -> bool {
        self.bind_error.is_none()
    }
}

struct UploadStream(mpsc::Receiver<Result<RawPage, BackendError>>);

impl Stream for UploadStream {
    type Item = Result<RawPage, BackendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

/// Backend implementation for mobile scanners.
#[derive(Debug, Clone)]
pub struct MobileBackend {
    discovery_timeout: Duration,
    /// §4.2 step 2's 5 s, overridable so a test does not have to wait one out.
    connect_timeout: Duration,
    /// §4.2 step 4's 120 s, overridable for the same reason.
    confirm_timeout: Duration,
    upload_port: u16,
    /// What `pair_request.host_id` carries, and what §12's `_scanbus-host._tcp` record
    /// advertises in TXT `id`. Read from the store at startup and never redrawn: the
    /// phone keeps the id it saw while pairing and, once the lease changes, browses for
    /// *that* id. An id minted per process would be stable for one handshake and wrong
    /// for every restart after it.
    host_id: String,
    /// What `pair_request.host_name` carries — shown on the phone next to the code.
    host_name: String,
    /// What the last discovery rounds saw, keyed by scanner. The only source of an
    /// address for [`MobileBackend::pair`].
    discovered: Arc<Mutex<BTreeMap<ScannerId, DiscoveryRecord>>>,
    /// What pairing issued, keyed by the scanner it belongs to.
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    store_path: Arc<PathBuf>,
    /// The one certificate of §11.2, loaded once here and handed to both TLS
    /// configurations. `None` is the §11.5 failure: an identity that could not be read
    /// is reported and left alone, never regenerated, and the host simply has no TLS.
    identity: Option<Arc<tls::HostIdentity>>,
    /// Why, kept for the same reason [`ListenerBinding::bind_error`] is — the daemon has
    /// to be able to say what is wrong with a scanner that came up unusable.
    identity_error: Option<Arc<str>>,
    /// `mobile.require_tls` (§11.5): no pairing with a phone that advertises no
    /// `tlsport`, and no cleartext upload from anybody, whatever the device table says.
    ///
    /// Shared rather than copied because the upload listener is spawned during
    /// construction and holds it for the life of the process. A plain field would be read
    /// once, into that task, and [`MobileBackend::with_require_tls`] would then be a
    /// setter that silently does nothing to the half of the switch that refuses uploads.
    require_tls: Arc<AtomicBool>,
    listener: Arc<Mutex<ListenerBinding>>,
    listener_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// §12's `_scanbus-host._tcp` record, which exists for exactly as long as this guard
    /// does — dropping the last clone of the backend is what puts the goodbye packet on
    /// the wire. `None` is the tolerated failure: no responder, or no listener to name.
    ///
    /// Shared rather than owned because the daemon clones the backend; a copy per clone
    /// would publish the same record several times and unregister it at the first drop.
    advertisement: Option<Arc<mdns::Registration>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
}

impl Default for MobileBackend {
    fn default() -> Self {
        Self::new(DISCOVERY_TIMEOUT)
    }
}

impl MobileBackend {
    pub fn new(discovery_timeout: Duration) -> Self {
        Self::with_store_path(default_store_path(), discovery_timeout, 0)
    }

    pub fn with_store_path(
        store_path: PathBuf,
        discovery_timeout: Duration,
        requested_upload_port: u16,
    ) -> Self {
        let mut store = load_or_reset_store(&store_path);
        // Before anything else reads it: a store written by an older scanbus, or a
        // fresh one, has no `host_id` and §12.2 says the fix is to draw one now and
        // keep it forever.
        let mut store_changed = false;
        if store.host_id.is_empty() {
            store.host_id = generate_host_id();
            store_changed = true;
        }
        let paired = store
            .devices
            .clone()
            .into_iter()
            .map(|(scanner_id, device)| (scanner_id, PairedDevice::from(device)))
            .collect();
        let configured_port = if requested_upload_port != 0 {
            requested_upload_port
        } else {
            store.upload_port
        };
        // Next to the device table, because §11.5 puts it there: one directory holds
        // everything a pairing survives a restart on.
        let identity_dir = store_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (identity, identity_error) = match tls::HostIdentity::load_or_generate(&identity_dir) {
            Ok(identity) => {
                debug!(
                    dir = %identity_dir.display(),
                    fingerprint = identity.fingerprint_sha256(),
                    "mobile TLS identity ready"
                );
                (Some(Arc::new(identity)), None)
            }
            // Loud, and nothing else: §11.5's one irreversible mistake is to treat this
            // as a first start and mint a new key over it.
            Err(error) => {
                let error = error.to_string();
                warn!(
                    dir = %identity_dir.display(),
                    %error,
                    "mobile TLS identity is unusable; it will not be regenerated and phones paired over TLS stay offline"
                );
                (None, Some(Arc::from(error.as_str())))
            }
        };

        let listener = bind_listener(configured_port);
        let upload_port = listener.port;
        if let Some(error) = listener.bind_error.as_ref() {
            warn!(
                port = upload_port,
                %error,
                "mobile upload listener could not be bound; paired phones will stay offline"
            );
        }
        if store.upload_port != upload_port {
            store.upload_port = upload_port;
            store_changed = true;
        }
        if store_changed && let Err(error) = persist_device_store(&store_path, &store) {
            warn!(path = %store_path.display(), %error, "could not persist mobile device store");
        }

        // §12.1: after the bind, and from the bound port rather than the configured one.
        // Nothing else gates it — §12.3 keeps the record up whether or not a phone is
        // paired, because one that appeared with the first pairing would make "the host
        // moved" and "the host forgot me" the same symptom.
        let host_name = host_name();
        let advertisement =
            HostRecord::new(&store.host_id, &host_name, &listener).and_then(advertise_host);

        let require_tls = require_tls_from_env();
        if require_tls {
            // At `info`, not `debug`: this is the line somebody reads when a phone that
            // paired last week suddenly cannot, and the answer is that the host was
            // restarted with the switch on.
            info!(
                variable = REQUIRE_TLS_ENV,
                "mobile TLS is required: phones without a tlsport will not be paired and \
                 cleartext uploads are refused"
            );
        }

        let backend = Self {
            discovery_timeout,
            connect_timeout: PAIR_CONNECT_TIMEOUT,
            confirm_timeout: PAIR_CONFIRM_TIMEOUT,
            upload_port,
            host_id: store.host_id,
            host_name,
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            paired: Arc::new(Mutex::new(paired)),
            store_path: Arc::new(store_path),
            identity,
            identity_error,
            require_tls: Arc::new(AtomicBool::new(require_tls)),
            listener: Arc::new(Mutex::new(listener)),
            listener_task: Arc::new(Mutex::new(None)),
            advertisement,
            subscriptions: Arc::new(Mutex::new(BTreeMap::new())),
            pending_uploads: Arc::new(Mutex::new(BTreeMap::new())),
            next_trigger_id: Arc::new(AtomicU64::new(1)),
        };
        backend.ensure_upload_task();
        backend
    }

    /// The port `pair_request` advertises for uploads — the shared listener's port.
    pub fn upload_port(&self) -> u16 {
        self.upload_port
    }

    /// The `_scanbus-host._tcp` instance this host publishes, or `None` when it could
    /// not be published — which is a working host missing one recovery path, not a
    /// broken one.
    pub fn advertised_fullname(&self) -> Option<&str> {
        self.advertisement
            .as_deref()
            .map(mdns::Registration::fullname)
    }

    pub fn listener_is_bound(&self) -> bool {
        self.listener
            .lock()
            .expect("mobile listener lock poisoned")
            .is_bound()
    }

    /// The certificate both TLS configurations are built from, or `None` when the host
    /// has no usable identity and therefore speaks no TLS at all.
    pub fn identity(&self) -> Option<&tls::HostIdentity> {
        self.identity.as_deref()
    }

    /// What the app pinned during pairing (§11.1). `None` for the same reason as above.
    pub fn certificate_fingerprint(&self) -> Option<&str> {
        self.identity
            .as_deref()
            .map(tls::HostIdentity::fingerprint_sha256)
    }

    /// Why there is no identity, in the words §11.5 wants logged. Shaped like the
    /// listener's own bind error deliberately: both are startup failures that leave
    /// paired phones unreachable rather than aborting the daemon.
    pub fn identity_error(&self) -> Option<&str> {
        self.identity_error.as_deref()
    }

    fn listener_error(&self) -> Option<String> {
        self.listener
            .lock()
            .expect("mobile listener lock poisoned")
            .bind_error
            .clone()
    }

    /// Why paired phones cannot be served, or `None` when they can.
    ///
    /// Two startup failures answer this, and §11.5 asks for the second to behave exactly
    /// like the first: an `upload_port` somebody else already holds (§5), and a TLS
    /// identity that could not be read. Neither aborts the daemon and neither is
    /// repaired — the second least of all, since the only repair available would be to
    /// mint a key over the unreadable one and unpair every phone at once. What they do
    /// instead is leave the scanners they affect `Offline` while keeping the reason, so
    /// that `Connect()` on one of them says what is wrong instead of timing out.
    ///
    /// The identity failure is deliberately the wider of the two: it takes down every
    /// paired phone, including ones paired in cleartext that a host with no certificate
    /// could still serve perfectly well — the upload listener's cleartext half needs no
    /// identity at all (§11.4). The per-device `tls` flag of §11.5 now exists and would be
    /// enough to narrow it to the phones that actually pinned something, and it is left
    /// wide on purpose: an unreadable key is a state somebody has to be told about, and a
    /// host that keeps working for most of its phones is a host nobody looks at until the
    /// rest of them fail.
    fn unavailable_reason(&self) -> Option<String> {
        self.listener_error()
            .or_else(|| self.identity_error().map(str::to_owned))
    }

    #[cfg(test)]
    fn has_subscription(&self, scanner_id: &ScannerId) -> bool {
        self.subscriptions
            .lock()
            .expect("mobile subscriptions lock poisoned")
            .contains_key(scanner_id)
    }

    fn ensure_upload_task(&self) {
        let mut binding = self.listener.lock().expect("mobile listener lock poisoned");
        if !binding.is_bound() || binding.task_started {
            return;
        }
        let Some(listener) = binding.listener.as_ref() else {
            return;
        };
        let Ok(cloned) = listener.try_clone() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if cloned.set_nonblocking(true).is_err() {
            return;
        }
        let Ok(listener) = TcpListener::from_std(cloned) else {
            return;
        };
        binding.task_started = true;
        drop(binding);

        // The same certificate the pairing dial presents as a client certificate (§11.2),
        // and `None` only when there is no identity at all — in which case this listener
        // still serves the cleartext half of §11.4, which is what every phone paired
        // before §11 uses.
        let server_config = self
            .identity
            .as_ref()
            .map(|identity| identity.upload_server_config());
        let require_tls = Arc::clone(&self.require_tls);
        let paired = Arc::clone(&self.paired);
        let subscriptions = Arc::clone(&self.subscriptions);
        let pending_uploads = Arc::clone(&self.pending_uploads);
        let next_trigger_id = Arc::clone(&self.next_trigger_id);
        let task = handle.spawn(async move {
            run_upload_listener(
                listener,
                server_config,
                require_tls,
                paired,
                subscriptions,
                pending_uploads,
                next_trigger_id,
            )
            .await;
        });
        *self
            .listener_task
            .lock()
            .expect("mobile listener task lock poisoned") = Some(task);
    }

    /// Shortens the two handshake deadlines, for a test or a simulator run that should
    /// not take two minutes to observe a timeout ([`9.6`]).
    ///
    /// [`9.6`]: https://github.com/jeanparpaillon/scanbus_service/issues/45
    pub fn with_pairing_timeouts(mut self, connect: Duration, confirm: Duration) -> Self {
        self.connect_timeout = connect;
        self.confirm_timeout = confirm;
        self
    }

    /// Sets `mobile.require_tls` (§11.5) directly, for a test or a simulator run that
    /// cannot reach [`REQUIRE_TLS_ENV`] — a process-wide variable is not something one
    /// test can set without deciding it for every other test in the binary.
    ///
    /// Takes effect on the already-running listener too; see the field's own note.
    pub fn with_require_tls(self, require: bool) -> Self {
        self.require_tls.store(require, Ordering::Relaxed);
        self
    }

    /// Whether this host refuses cleartext outright (§11.5).
    pub fn requires_tls(&self) -> bool {
        self.require_tls.load(Ordering::Relaxed)
    }

    /// Whether an upload bearing `device_id` and `token` is from a phone this backend
    /// paired — the check [`9.4`](PairedDevice)'s listener answers `unauthorized` on.
    ///
    /// After [`ScannerBackend::forget`] this is `false` for that phone, which is the
    /// whole of what `Unpair()` does to a device that is switched off.
    pub fn is_authorized(&self, device_id: &str, token: &str) -> bool {
        self.lock_paired().values().any(|device| {
            device.device_id == device_id
                && constant_time_eq(&device.token_sha256, &hash_token(token))
        })
    }

    /// The handshake of mobile-backend.md §4.2.
    ///
    /// Everything it opens lives on this future's own stack, which is what makes
    /// `CancelPairing()` work: 1.4 cancels by aborting the task, the future is dropped,
    /// and the socket closes with it — §4.2's "the app sees the connection drop", with
    /// no cancel message to invent.
    async fn pair(
        &self,
        scanner: &ScannerInfo,
        progress: &mpsc::Sender<scanbus_core::PairingProgress>,
    ) -> Result<(), BackendError> {
        // Refusing on a broken identity, not quietly pairing in cleartext instead. A host
        // with no certificate cannot present one, so every pairing it made here would be
        // a downgrade the operator never asked for — recorded by the app as unencrypted,
        // warned about there and nowhere else, and undone only by pairing again. §11.6
        // reserves that outcome for an attacker who strips `tlsport`; the host must not
        // arrive at it on its own because a file was unreadable.
        if let Some(detail) = self.unavailable_reason() {
            return Err(BackendError::Other(format!(
                "mobile pairing is unavailable: {detail}"
            )));
        }

        let record = self
            .live_record(&scanner.id)
            .ok_or_else(|| BackendError::NotReachable {
                scanner: scanner.id.clone(),
                detail: "no live discovery record: pairing is the one moment the host dials, \
			         and it dials the address a phone is advertising now, never a \
			         remembered one"
                    .to_owned(),
            })?;

        // §11.3's port choice, and the whole of it: the key the phone advertised means it
        // has a keystore identity to serve, its absence means it has none, and there is
        // no third state to negotiate. Neither arm falls back to the other — see
        // `dial_pairing_tls` on why the obvious cleartext retry is not there.
        //
        // One deadline covers the dial and, in the TLS arm, the handshake after it, so
        // §11.3 costs no new timeout: a phone that accepts a connection and then stalls
        // in the handshake still ends the pairing at the same 5 s as a phone that never
        // accepted one.
        let deadline = Instant::now() + self.connect_timeout;
        let (response, tls) = match record.tls_address.as_deref() {
            Some(tls_address) => {
                let socket = self
                    .dial_pairing_tls(&scanner.id, tls_address, deadline)
                    .await?;
                let response = self
                    .pair_over(scanner, &record, tls_address, socket, progress)
                    .await?;
                // `true` is written here, in the arm that actually completed a handshake,
                // and not from `record.tls_address.is_some()` at the store: §11.6 makes
                // this flag the reason a later cleartext upload is refused, so it has to
                // record what the connection was, never what was hoped for.
                (response, true)
            }
            // §11.5's switch, and the only place the pairing half of it can live: after
            // this line the difference between the two phones is gone. The refusal is
            // before the dial rather than after the handshake because there is nothing to
            // learn from dialling — the absent TXT key is the whole of what the phone has
            // to say about its own TLS (§11.3), and a phone with no keystore identity
            // cannot acquire one by being asked twice.
            //
            // `Other`, not a new error: §11.7 keeps `Scanner1` unchanged, and the reason
            // string is what reaches the user. It names the switch, because "this phone is
            // too old for this network" and "somebody set a variable" look identical from
            // the front of a GUI and only one of them is worth calling anybody about.
            None if self.requires_tls() => {
                warn!(
                    scanner = %scanner.id,
                    variable = REQUIRE_TLS_ENV,
                    "refusing to pair a phone that advertises no tlsport"
                );
                return Err(BackendError::Other(format!(
                    "this phone offers no encrypted pairing port and {REQUIRE_TLS_ENV} is set: \
                     pairing it would put its token and its pages on the network in clear"
                )));
            }
            None => {
                let socket = self.connect_to_record(&scanner.id, &record.address).await?;
                let response = self
                    .pair_over(scanner, &record, &record.address, socket, progress)
                    .await?;
                (response, false)
            }
        };

        let device = PairedDevice {
            device_id: response.device_id,
            token_sha256: hash_token(&response.token),
            profiles: response.capabilities.profiles,
            paired_at: unix_timestamp_now(),
            tls,
        };
        self.store_paired_device(&scanner.id, device)?;

        Ok(())
    }

    /// Dials `address` and completes the TLS handshake of §11.3, inside what is left of
    /// `deadline`.
    ///
    /// **There is no cleartext retry here, and adding one would undo the section.** A
    /// phone that advertised `tlsport` and then cannot finish a handshake is broken, and
    /// a quiet retry on the SRV port would hide that behind a pairing that merely looks
    /// successful. The stronger reason is that an automatic downgrade is a path an
    /// attacker gets to trigger: refusing the handshake is cheaper for them than
    /// stripping the TXT key, and it would land in the same place.
    async fn dial_pairing_tls(
        &self,
        scanner_id: &ScannerId,
        address: &str,
        deadline: Instant,
    ) -> Result<TlsStream<TcpStream>, BackendError> {
        // Already refused at the top of `pair` — a host with no identity has no client
        // certificate to present and must not pair at all — so this is the shape of that
        // check rather than a second policy.
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: "this host has no TLS identity to present as a client certificate"
                    .to_owned(),
            })?;

        // The address is an IP literal, so this is an `IpAddress` name and `rustls` sends
        // no SNI — correct for one, and irrelevant to the other: nothing in the name is
        // checked at either end (§11.3). It is parsed rather than invented so that a
        // certificate a future version *did* verify would be verified against the thing
        // that was actually dialled.
        let server_name = ServerName::from(
            address
                .parse::<SocketAddr>()
                .map_err(|error| BackendError::NotReachable {
                    scanner: scanner_id.clone(),
                    detail: format!("could not parse discovered TLS address {address}: {error}"),
                })?
                .ip(),
        );

        let socket = self.connect_to_record(scanner_id, address).await?;
        let connector = TlsConnector::from(identity.pairing_client_config());

        timeout(
            deadline.saturating_duration_since(Instant::now()),
            connector.connect(server_name, socket),
        )
        .await
        .map_err(|_| BackendError::NotReachable {
            scanner: scanner_id.clone(),
            detail: format!(
                "{address} accepted a connection but did not complete a TLS handshake within \
				 the {:?} pairing budget",
                self.connect_timeout
            ),
        })?
        .map_err(|error| BackendError::NotReachable {
            scanner: scanner_id.clone(),
            detail: format!(
                "the TLS handshake with {address} failed: {error}; the phone advertised that \
				 port as its TLS one, so this pairing is not retried in cleartext"
            ),
        })
    }

    /// The handshake itself, over whichever of the two connections §11.3 chose.
    ///
    /// Generic over the socket rather than taking an enum of the two, because nothing in
    /// here may depend on which one it got: the frames on a TLS pairing are byte-identical
    /// to the frames on a cleartext one, and that is what keeps `pair_request` and
    /// `pair_response` unchanged by §11.
    async fn pair_over<S: AsyncRead + AsyncWrite + Unpin + Send>(
        &self,
        scanner: &ScannerInfo,
        record: &DiscoveryRecord,
        dialled: &str,
        mut socket: S,
        progress: &mpsc::Sender<scanbus_core::PairingProgress>,
    ) -> Result<PairResponse, BackendError> {
        // Nothing below ever logs `nonce`: it is the whole security value of the
        // comparison, and a code sitting in a log is a code an attacker can replay.
        let nonce = generate_nonce();
        let request = Message::PairRequest(PairRequest {
            v: PROTOCOL_VERSION,
            host_id: self.host_id.clone(),
            host_name: self.host_name.clone(),
            upload_port: self.upload_port,
            nonce: nonce.clone(),
        });
        let payload = serialize_control_message(&request).map_err(|error| {
            BackendError::Other(format!("could not encode pair_request: {error}"))
        })?;
        write_frame(
            &mut socket,
            FrameKind::Control,
            &payload,
            DEFAULT_CONTROL_MAX_BYTES,
        )
        .await
        .map_err(|error| BackendError::NotReachable {
            scanner: scanner.id.clone(),
            detail: format!("could not send pair_request to {dialled}: {error}"),
        })?;

        // Announced only once the request is on the wire, so a client cannot be showing
        // a code the phone was never sent.
        progress
            .send(scanbus_core::PairingProgress::AwaitingConfirmation { code: nonce })
            .await
            .map_err(|_| {
                BackendError::Other(
                    "the pairing was abandoned before the code could be shown".to_owned(),
                )
            })?;
        debug!(scanner_id = %scanner.id, "waiting for the phone to confirm");

        let frame = timeout(
            self.confirm_timeout,
            read_frame(&mut socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES),
        )
        .await
        .map_err(|_| {
            BackendError::Other(format!(
                "the phone did not confirm within {:?}",
                self.confirm_timeout
            ))
        })?
        .map_err(|error| BackendError::Other(format!("no usable pair_response: {error}")))?;

        let message = parse_control_message(&frame)
            .map_err(|error| BackendError::Other(format!("no usable pair_response: {error}")))?;
        let Message::PairResponse(response) = message else {
            return Err(BackendError::Other(
                "the phone answered the pairing with something other than a pair_response"
                    .to_owned(),
            ));
        };

        if !response.accepted {
            return Err(BackendError::Other("rejected on the device".to_owned()));
        }

        // §4.2 step 6. The TXT record and the response arrive over different paths, so a
        // mismatch means the thing that answered is not the thing that advertised —
        // without this, a phone on the same network could answer a pairing meant for
        // another and take over its ScannerId, and with it its token slot.
        if response.device_id != record.device_id {
            return Err(BackendError::Other(format!(
                "pair_response came back with device_id {:?}, but {:?} is what the mDNS \
				 TXT record advertised; the device that answered is not the device that \
				 was dialled",
                response.device_id, record.device_id
            )));
        }

        if response.token.is_empty() {
            return Err(BackendError::Other(
                "the phone accepted the pairing without issuing a token, so no upload \
				 from it could ever be recognised"
                    .to_owned(),
            ));
        }

        Ok(response)
    }

    /// The most recent sighting of `scanner`, if one is still live.
    ///
    /// Expiry is applied here rather than only on the way in, so a session that ended
    /// silently — no round to overwrite the map — stops being an address to dial.
    fn live_record(&self, scanner: &ScannerId) -> Option<DiscoveryRecord> {
        let mut discovered = self.lock_discovered();
        discovered.retain(|_, record| record.seen_at.elapsed() < DISCOVERY_RECORD_TTL);
        discovered.get(scanner).cloned()
    }

    /// Records what a round found, and drops what has gone quiet.
    fn remember(&self, records: Vec<(ScannerId, DiscoveryRecord)>) {
        let mut discovered = self.lock_discovered();
        discovered.retain(|_, record| record.seen_at.elapsed() < DISCOVERY_RECORD_TTL);
        discovered.extend(records);
    }

    /// What the phone advertised when it paired, for a scanner that is paired.
    ///
    /// Read back into every [`ScannerInfo`] this backend reports, so `SupportedProfiles`
    /// keeps saying what the phone can do rather than reverting to the daemon's full
    /// list at the next discovery round.
    fn advertised_profiles(&self, scanner: &ScannerId) -> Vec<ProfileKind> {
        self.lock_paired()
            .get(scanner)
            .map(|device| device.profiles.clone())
            .unwrap_or_default()
    }

    /// A paired phone is only as reachable as the host's half of the connection it is
    /// going to make.
    ///
    /// An unpaired one stays `Online` whatever is wrong here: it was discovered, it is
    /// answering, and what is broken is the host's ability to serve a pairing rather than
    /// anything about the phone. [`MobileBackend::pair`] is where that surfaces, with the
    /// reason attached.
    fn status_for(&self, scanner: &ScannerId) -> Status {
        if !self.lock_paired().contains_key(scanner) {
            return Status::Online;
        }
        if self.unavailable_reason().is_some() {
            Status::Offline
        } else {
            Status::Online
        }
    }

    fn lock_discovered(&self) -> std::sync::MutexGuard<'_, BTreeMap<ScannerId, DiscoveryRecord>> {
        self.discovered
            .lock()
            .expect("mobile discovery cache lock poisoned")
    }

    fn lock_paired(&self) -> std::sync::MutexGuard<'_, BTreeMap<ScannerId, PairedDevice>> {
        self.paired
            .lock()
            .expect("mobile paired device lock poisoned")
    }

    fn store_paired_device(
        &self,
        scanner_id: &ScannerId,
        device: PairedDevice,
    ) -> Result<(), BackendError> {
        let mut paired = self.lock_paired();
        paired.insert(scanner_id.clone(), device);
        if let Err(error) = self.persist_paired_locked(&paired) {
            paired.remove(scanner_id);
            return Err(BackendError::Other(error));
        }
        Ok(())
    }

    fn persist_paired_locked(
        &self,
        paired: &BTreeMap<ScannerId, PairedDevice>,
    ) -> Result<(), String> {
        let mut store = load_or_reset_store(self.store_path.as_ref());
        store.upload_port = self.upload_port;
        store.devices = paired
            .iter()
            .map(|(scanner_id, device)| (scanner_id.clone(), PersistedDevice::from(device)))
            .collect();
        persist_device_store(self.store_path.as_ref(), &store)
    }

    fn prune_unrestored_locked(&self, restored: &[ScannerId]) -> Result<(), String> {
        let restored: std::collections::BTreeSet<_> = restored.iter().cloned().collect();
        let mut paired = self.lock_paired();
        let before = paired.len();
        paired.retain(|scanner_id, _| restored.contains(scanner_id));
        if paired.len() != before {
            self.persist_paired_locked(&paired)?;
        }
        Ok(())
    }

    async fn connect_to_record(
        &self,
        scanner_id: &ScannerId,
        address: &str,
    ) -> Result<TcpStream, BackendError> {
        let parsed: SocketAddr = address
            .parse()
            .map_err(|error| BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!("could not parse discovered address {address}: {error}"),
            })?;

        if let SocketAddr::V6(addr) = parsed
            && addr.ip().is_unicast_link_local()
            && addr.scope_id() == 0
        {
            return self.connect_link_local(scanner_id, address, addr).await;
        }

        self.connect_socket_addr(scanner_id, address, parsed).await
    }

    async fn connect_link_local(
        &self,
        scanner_id: &ScannerId,
        display_address: &str,
        address: SocketAddrV6,
    ) -> Result<TcpStream, BackendError> {
        let candidates = link_local_candidates(*address.ip(), address.port()).map_err(|error| {
            BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!(
                    "could not choose an interface for link-local address {display_address}: \
						 {error}"
                ),
            }
        })?;

        if candidates.is_empty() {
            return Err(BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!(
                    "could not connect to {display_address}: no non-loopback IPv6 interface \
					 with a scope id is available for link-local pairing"
                ),
            });
        }

        let mut last_error = None;
        for candidate in candidates {
            match self
                .connect_socket_addr(scanner_id, display_address, SocketAddr::V6(candidate))
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.expect("link-local candidates are non-empty"))
    }

    async fn connect_socket_addr(
        &self,
        scanner_id: &ScannerId,
        display_address: &str,
        address: SocketAddr,
    ) -> Result<TcpStream, BackendError> {
        timeout(self.connect_timeout, TcpStream::connect(address))
            .await
            .map_err(|_| BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!(
                    "{display_address} did not accept a connection within {:?}",
                    self.connect_timeout
                ),
            })?
            .map_err(|error| BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!("could not connect to {display_address}: {error}"),
            })
    }
}

#[async_trait]
impl ScannerBackend for MobileBackend {
    fn id(&self) -> &'static str {
        ID
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let timeout = self.discovery_timeout;
        let found = tokio::task::spawn_blocking(move || discover_once(timeout))
            .await
            .map_err(|error| {
                BackendError::Other(format!("mobile discover task failed: {error}"))
            })??;

        let mut scanners = Vec::with_capacity(found.len());
        let mut records = Vec::with_capacity(found.len());
        for (mut info, record) in found {
            // A phone that has already paired keeps saying what it can produce: the
            // TXT record has no room for `profiles`, and reverting `SupportedProfiles`
            // to the daemon's full list at the next round would advertise a profile
            // the app told us it cannot do.
            info.capabilities.profiles = self.advertised_profiles(&info.id);
            info.status = self.status_for(&info.id);
            records.push((info.id.clone(), record));
            scanners.push(info);
        }
        self.remember(records);

        Ok(scanners)
    }

    /// The handshake of mobile-backend.md §4.2, run inline: there is nothing to
    /// *install* for a mobile scanner, so this is where pairing's "slow part" — a human
    /// reading a code off a phone — lives instead.
    ///
    /// It sends no `Ready` and no `Failed`: 1.4's driver already turns the returned
    /// `Err` into `PairingState="failed"` with this error's message, and `Ready` maps to
    /// no state at all. Sending either would only duplicate a transition.
    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<scanbus_core::PairingProgress>,
    ) -> Result<(), BackendError> {
        self.pair(scanner, &progress).await
    }

    /// A phone has no buttons to listen for, so this is a pending subscription rather
    /// than a stream that ends.
    ///
    /// It has to succeed: 1.4's sequence is `ensure_installed` → `start_listening` →
    /// `Done`, so a backend that refuses here can never reach `Paired=true`, however
    /// well the handshake went. What a phone triggers a job with is an *upload*, not a
    /// button press ([`9.5`]), and the listener that receives one is a single shared
    /// socket ([`9.4`]) — neither belongs on this per-scanner button stream, and
    /// `buttons.count` is already 0 for every phone this backend discovers.
    ///
    /// [`9.4`]: https://github.com/jeanparpaillon/scanbus_service/issues/43
    /// [`9.5`]: https://github.com/jeanparpaillon/scanbus_service/issues/44
    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, scanbus_core::ScanTrigger>, BackendError> {
        if self.lock_paired().contains_key(&scanner.id)
            && let Some(detail) = self.unavailable_reason()
        {
            return Err(BackendError::Other(format!(
                "mobile uploads are unavailable: {detail}"
            )));
        }
        let (sender, receiver) = mpsc::channel(8);
        self.subscriptions
            .lock()
            .expect("mobile subscriptions lock poisoned")
            .insert(scanner.id.clone(), sender);
        Ok(Box::pin(MobileSubscription::new(
            scanner.id.clone(),
            Arc::clone(&self.subscriptions),
            receiver,
        )))
    }

    /// Nothing per-scanner is listening, so there is nothing to stop — and the trait
    /// requires the no-op case to be `Ok(())`, not an error.
    async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        self.subscriptions
            .lock()
            .expect("mobile subscriptions lock poisoned")
            .remove(scanner_id);
        Ok(())
    }

    async fn restore_disposition(&self, scanner: &ScannerInfo) -> RestoreDisposition {
        if self.lock_paired().contains_key(&scanner.id) {
            RestoreDisposition::Paired
        } else {
            RestoreDisposition::Failed(
                "the pairing secret is missing; pair the phone again".to_owned(),
            )
        }
    }

    async fn prune_unrestored_pairings(&self, restored: &[ScannerId]) -> Result<(), BackendError> {
        self.prune_unrestored_locked(restored)
            .map_err(BackendError::Other)
    }

    /// Revokes the token a pairing issued, so an upload bearing it comes back
    /// `unauthorized` — the trait method mobile-backend.md §4.4 adds for `Unpair()`.
    ///
    /// The phone is not told: there is no channel to tell it on, and `Unpair()` has to
    /// work with the phone switched off. It learns at its next upload.
    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        let mut paired = self.lock_paired();
        if paired.remove(scanner_id).is_some() {
            self.persist_paired_locked(&paired)
                .map_err(BackendError::Other)?;
            debug!(scanner_id = %scanner_id, "mobile pairing revoked");
        }
        Ok(())
    }

    async fn set_button_mapping(
        &self,
        _scanner_id: &ScannerId,
        _button_index: u32,
        _profile: Option<ProfileKind>,
        _options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unsupported {
            backend: ID,
            operation: "set_button_mapping",
        })
    }

    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        let mut pending = self
            .pending_uploads
            .lock()
            .expect("mobile pending upload lock poisoned");
        let Some(mut upload) = pending.remove(trigger_id) else {
            return Err(BackendError::UnknownJob {
                scanner: scanner_id.clone(),
                job: trigger_id.to_owned(),
            });
        };
        if &upload.scanner_id != scanner_id {
            return Err(BackendError::UnknownJob {
                scanner: scanner_id.clone(),
                job: trigger_id.to_owned(),
            });
        }
        if let Some(claim) = upload.claim.take() {
            let _ = claim.send(());
        }
        Ok(Box::pin(UploadStream(upload.receiver)))
    }
}

impl Drop for MobileBackend {
    fn drop(&mut self) {
        if Arc::strong_count(&self.listener_task) != 1 {
            return;
        }
        self.listener
            .lock()
            .expect("mobile listener lock poisoned")
            .listener = None;
        if let Some(task) = self
            .listener_task
            .lock()
            .expect("mobile listener task lock poisoned")
            .take()
        {
            task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_upload_listener(
    listener: TcpListener,
    server_config: Option<Arc<rustls::ServerConfig>>,
    require_tls: Arc<AtomicBool>,
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let server_config = server_config.clone();
        let require_tls = Arc::clone(&require_tls);
        let paired = Arc::clone(&paired);
        let subscriptions = Arc::clone(&subscriptions);
        let pending_uploads = Arc::clone(&pending_uploads);
        let next_trigger_id = Arc::clone(&next_trigger_id);
        // The demux and, on a TLS connection, the handshake happen in here rather than in
        // the accept loop above: §11.4 spends them from the "connections awaiting their
        // first frame" budget of §5, and a phone that opens a connection and then stalls
        // mid-handshake must cost this listener no more than one that opens a connection
        // and sends no frame. Doing either before the spawn would let one such phone stop
        // every other one from being accepted.
        tokio::spawn(async move {
            let _ = handle_upload_connection(
                socket,
                server_config,
                require_tls.load(Ordering::Relaxed),
                paired,
                subscriptions,
                pending_uploads,
                next_trigger_id,
            )
            .await;
        });
    }
}

/// The first byte of every TLS connection there is: `ContentType.handshake` (§11.4).
const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// The top byte of our `u32` length prefix. The first frame of an upload is a control
/// frame capped at 64 KiB (§3), so on a cleartext connection it cannot be anything else.
const FRAME_LENGTH_TOP_BYTE: u8 = 0x00;

/// One accepted connection, before it is known which protocol it speaks.
///
/// §11.4's demux, and the reason the listener needs no second port the way the phone does
/// (§11.3): [`TcpStream::peek`] leaves the byte where it was, so the TLS acceptor still
/// finds the `ClientHello` it is looking for. A read here would consume the byte, and
/// there would be no way to give it back.
///
/// The deadline covers the peek, the handshake and the first frame together. It is one
/// budget rather than three because the three are one thing from the listener's side —
/// a connection that has not yet said anything it can be held to — and §11.4 asks for the
/// handshake to be inside that budget rather than in front of it.
#[allow(clippy::too_many_arguments)]
async fn handle_upload_connection(
    socket: TcpStream,
    server_config: Option<Arc<rustls::ServerConfig>>,
    require_tls: bool,
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
) -> Result<(), ProtocolError> {
    let deadline = Instant::now() + UPLOAD_FRAME_TIMEOUT;

    let mut first_byte = [0_u8; 1];
    let peeked = timeout(
        deadline.saturating_duration_since(Instant::now()),
        socket.peek(&mut first_byte),
    )
    .await
    .map_err(|_| ProtocolError::Malformed {
        context: "connected to the upload port and sent nothing".to_owned(),
    })?
    .map_err(|error| ProtocolError::Malformed {
        context: format!("could not read the first byte of an upload connection: {error}"),
    })?;
    if peeked == 0 {
        return Err(ProtocolError::Malformed {
            context: "an upload connection closed before its first byte".to_owned(),
        });
    }

    match first_byte[0] {
        TLS_HANDSHAKE_CONTENT_TYPE => {
            // No identity, so nothing to hand the acceptor. This is unreachable through a
            // pairing — a host with no certificate refuses to pair at all — and it is not
            // answered with an error frame, because whatever is at the other end is
            // waiting for a `ServerHello`, not for JSON.
            let Some(server_config) = server_config else {
                return Err(ProtocolError::Malformed {
                    context: "a phone opened a TLS upload connection, but this host has no \
					          TLS identity to answer it with"
                        .to_owned(),
                });
            };
            let socket = timeout(
                deadline.saturating_duration_since(Instant::now()),
                tokio_rustls::TlsAcceptor::from(server_config).accept(socket),
            )
            .await
            .map_err(|_| ProtocolError::Malformed {
                context: "an upload TLS handshake did not finish inside the first-frame budget"
                    .to_owned(),
            })?
            .map_err(|error| ProtocolError::Malformed {
                context: format!("an upload TLS handshake failed: {error}"),
            })?;
            serve_upload(
                socket,
                true,
                require_tls,
                deadline,
                paired,
                subscriptions,
                pending_uploads,
                next_trigger_id,
            )
            .await
        }
        // Cleartext, exactly as before §11: every phone paired before it dials this and
        // must keep working forever (§11.6) — unless `require_tls` is set, in which case
        // it is served far enough to be told `unauthorized` and no further.
        FRAME_LENGTH_TOP_BYTE => {
            serve_upload(
                socket,
                false,
                require_tls,
                deadline,
                paired,
                subscriptions,
                pending_uploads,
                next_trigger_id,
            )
            .await
        }
        // Not this protocol under either reading, so there is nobody to send an ack to.
        // Dropping the socket is the whole of the answer.
        other => {
            debug!(
                first_byte = format!("0x{other:02x}"),
                "closing a connection on the mobile upload port: not a TLS handshake and not \
				 a control frame"
            );
            Err(ProtocolError::Malformed {
                context: format!("first byte 0x{other:02x} is neither TLS nor a control frame"),
            })
        }
    }
}

/// An upload, over whichever of the two transports [`handle_upload_connection`] found.
///
/// Generic over the socket for the same reason [`MobileBackend::pair_over`] is: the frames
/// of §3 are byte-identical on both, and nothing from here down is allowed to know which
/// one it got. The one thing that does know is `over_tls`, and it — with the `require_tls`
/// it is weighed against — goes exactly one place: [`authorize_upload`].
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
async fn serve_upload<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut socket: S,
    over_tls: bool,
    require_tls: bool,
    deadline: Instant,
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
) -> Result<(), ProtocolError> {
    // What is left of the budget the handshake was also spent from.
    let first = match read_upload_header(
        &mut socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .await
    {
        Ok(first) => first,
        Err(error) => {
            let _ = send_error_ack(&mut socket, ack_reason_for(&error)).await;
            return Err(error);
        }
    };
    let scanner_id = match authorize_upload(
        &paired,
        &first.device_id,
        &first.token,
        over_tls,
        require_tls,
    ) {
        Ok(scanner_id) => scanner_id,
        Err(error) => {
            let _ = send_error_ack(&mut socket, ack_reason_for(&error)).await;
            return Err(error);
        }
    };
    let sender = {
        subscriptions
            .lock()
            .expect("mobile subscriptions lock poisoned")
            .get(&scanner_id)
            .cloned()
    };
    let sender = match sender {
        Some(sender) => sender,
        None => {
            let error = ProtocolError::NotConnected {
                device_id: first.device_id.clone(),
            };
            let _ = send_error_ack(&mut socket, ack_reason_for(&error)).await;
            return Err(error);
        }
    };

    let trigger_id = format!("upload-{}", next_trigger_id.fetch_add(1, Ordering::Relaxed));
    let trigger =
        scanbus_core::ScanTrigger::push(scanner_id.clone(), trigger_id.clone(), first.profile);
    let (pages_tx, pages_rx) = mpsc::channel(UPLOAD_PAGE_BUFFER);
    let (claim_tx, claim_rx) = oneshot::channel();
    pending_uploads
        .lock()
        .expect("mobile pending upload lock poisoned")
        .insert(
            trigger_id.clone(),
            PendingUpload {
                scanner_id: scanner_id.clone(),
                claim: Some(claim_tx),
                receiver: pages_rx,
            },
        );

    if sender.send(trigger).await.is_err() {
        pending_uploads
            .lock()
            .expect("mobile pending upload lock poisoned")
            .remove(&trigger_id);
        return send_error_ack(&mut socket, AckReason::NotConnected).await;
    }

    if timeout(FETCH_CLAIM_TIMEOUT, claim_rx).await.is_err() {
        pending_uploads
            .lock()
            .expect("mobile pending upload lock poisoned")
            .remove(&trigger_id);
        return send_error_ack(&mut socket, AckReason::Malformed).await;
    }

    let result = stream_upload_pages(&mut socket, first, pages_tx).await;
    if let Err(ref error) = result {
        let _ = send_error_ack(&mut socket, ack_reason_for(error)).await;
    }
    result
}

async fn stream_upload_pages<S: AsyncRead + AsyncWrite + Unpin + Send>(
    socket: &mut S,
    first: Upload,
    pages_tx: mpsc::Sender<Result<RawPage, BackendError>>,
) -> Result<(), ProtocolError> {
    let mut current = first;
    loop {
        let format = current
            .format
            .parse::<scanbus_core::PageFormat>()
            .map_err(|error| ProtocolError::Malformed {
                context: error.to_string(),
            })?;
        let bytes = timeout(
            UPLOAD_FRAME_TIMEOUT,
            read_frame(socket, FrameKind::Page, DEFAULT_PAGE_MAX_BYTES),
        )
        .await
        .map_err(|_| ProtocolError::Malformed {
            context: "timed out waiting for a page frame".to_owned(),
        })??;

        if pages_tx
            .send(Ok(RawPage {
                index: current.page - 1,
                format,
                resolution_dpi: DEFAULT_UPLOAD_RESOLUTION_DPI,
                data: bytes,
            }))
            .await
            .is_err()
        {
            return Err(ProtocolError::Malformed {
                context: "nobody fetched the uploaded pages".to_owned(),
            });
        }
        send_ok_ack(socket).await?;

        if current.page == current.of {
            return Ok(());
        }

        let next = read_upload_header(socket, UPLOAD_FRAME_TIMEOUT).await?;
        if next.device_id != current.device_id || next.token != current.token {
            return Err(ProtocolError::Unauthorized {
                device_id: next.device_id,
            });
        }
        if next.profile != current.profile {
            return Err(ProtocolError::Malformed {
                context: "profile changed mid-upload".to_owned(),
            });
        }
        if next.of != current.of || next.page != current.page + 1 {
            return Err(ProtocolError::Malformed {
                context: format!(
                    "expected page {} of {}, got page {} of {}",
                    current.page + 1,
                    current.of,
                    next.page,
                    next.of
                ),
            });
        }
        current = next;
    }
}

/// Reads one upload header, within `budget`.
///
/// The budget is a parameter rather than [`UPLOAD_FRAME_TIMEOUT`] because the *first*
/// header shares its deadline with the demux and the TLS handshake that may have preceded
/// it (§11.4); every header after that gets the full frame timeout of its own.
async fn read_upload_header<S: AsyncRead + AsyncWrite + Unpin + Send>(
    socket: &mut S,
    budget: Duration,
) -> Result<Upload, ProtocolError> {
    let frame = timeout(
        budget,
        read_frame(socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES),
    )
    .await
    .map_err(|_| ProtocolError::Malformed {
        context: "timed out waiting for an upload header".to_owned(),
    })??;
    let message = parse_control_message(&frame)?;
    message.validate_version()?;
    let Message::Upload(upload) = message else {
        return Err(ProtocolError::Malformed {
            context: "expected an upload control message".to_owned(),
        });
    };
    upload.validate_page_bounds()?;
    Ok(upload)
}

/// Whether this upload may proceed, and for which scanner.
///
/// Two rules, and §11.4 is explicit that they are not symmetric:
///
/// - **A device paired over TLS may not upload in cleartext.** Without this the pin buys
///   the host nothing: a token captured before the pairing was encrypted, or lifted from a
///   phone afterwards, would simply be replayed on the cleartext path that has to stay
///   open for everybody else. `unauthorized` is the reason rather than a new one because
///   the app's documented response to it — discard the pairing and pair again — is exactly
///   the repair, and a reason the app does not know would leave it with nothing to do.
/// - **A device paired in cleartext may upload over TLS.** Accepted: the token still
///   authenticates it and encryption is never worse than none. There is no pin to check
///   and the host does not invent one — a fingerprint learned at first upload is a
///   fingerprint of whatever answered (§11.1).
///
/// `require_tls` (§11.5) collapses both into one: on a host that is set that way no
/// cleartext upload is authorized at all, whatever the device table says about who sent
/// it. That is what makes it a network policy rather than a per-pairing property — the
/// operator who sets it is saying nothing unencrypted crosses this LAN, and a phone
/// paired before the switch was thrown is exactly the case they mean.
#[allow(clippy::fn_params_excessive_bools)]
fn authorize_upload(
    paired: &Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    device_id: &str,
    token: &str,
    over_tls: bool,
    require_tls: bool,
) -> Result<ScannerId, ProtocolError> {
    let unauthorized = || ProtocolError::Unauthorized {
        device_id: device_id.to_owned(),
    };

    let (scanner_id, paired_over_tls) = paired
        .lock()
        .expect("mobile paired device lock poisoned")
        .iter()
        .find_map(|(scanner_id, device)| {
            (device.device_id == device_id
                && constant_time_eq(&device.token_sha256, &hash_token(token)))
            .then(|| (scanner_id.clone(), device.tls))
        })
        .ok_or_else(unauthorized)?;

    if !over_tls && (paired_over_tls || require_tls) {
        // Loud, because the things that produce it are a phone downgraded by something on
        // the network, a token being replayed by something else, and a switch somebody
        // set — and none of the three is visible anywhere but here. The token itself is
        // never logged. `required` is what separates "somebody is attacking this pairing"
        // from "this phone predates the policy", which is the difference between calling
        // the operator and calling the police.
        warn!(
            %device_id,
            scanner = %scanner_id,
            pinned = paired_over_tls,
            required = require_tls,
            "refusing a cleartext upload; the phone will be told to pair again"
        );
        return Err(unauthorized());
    }

    Ok(scanner_id)
}

async fn send_ok_ack<S: AsyncRead + AsyncWrite + Unpin + Send>(
    socket: &mut S,
) -> Result<(), ProtocolError> {
    send_ack(
        socket,
        Ack {
            status: AckStatus::Ok,
            reason: None,
        },
    )
    .await
}

async fn send_error_ack<S: AsyncRead + AsyncWrite + Unpin + Send>(
    socket: &mut S,
    reason: AckReason,
) -> Result<(), ProtocolError> {
    send_ack(
        socket,
        Ack {
            status: AckStatus::Error,
            reason: Some(reason),
        },
    )
    .await
}

async fn send_ack<S: AsyncRead + AsyncWrite + Unpin + Send>(
    socket: &mut S,
    ack: Ack,
) -> Result<(), ProtocolError> {
    let payload = serialize_control_message(&Message::Ack(ack))?;
    write_frame(
        socket,
        FrameKind::Control,
        &payload,
        DEFAULT_CONTROL_MAX_BYTES,
    )
    .await
}

fn ack_reason_for(error: &ProtocolError) -> AckReason {
    match error {
        ProtocolError::Unauthorized { .. } => AckReason::Unauthorized,
        ProtocolError::UnsupportedVersion { .. } => AckReason::UnsupportedVersion,
        ProtocolError::NotConnected { .. } => AckReason::NotConnected,
        ProtocolError::Malformed { .. } => AckReason::Malformed,
        ProtocolError::TooLarge { .. } => AckReason::TooLarge,
        ProtocolError::Unsupported { .. } => AckReason::Unsupported,
    }
}

/// Compares two secrets without returning early on the first differing byte.
///
/// The length is allowed to leak — it is not the secret — but the prefix is not: an
/// upload path that answers faster the sooner a token diverges hands an attacker on the
/// LAN a way to guess it one byte at a time.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn hash_token(token: &str) -> String {
    hex_lower(&Sha256::digest(token.as_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// A CSPRNG-drawn host identifier for `pair_request.host_id`.
fn generate_host_id() -> String {
    use fmt::Write as _;

    let bytes: [u8; 16] = rand::rng().random();
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            // Infallible: writing into a String.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// What the phone shows next to the code, so the user knows which machine is asking.
///
/// `/proc/sys/kernel/hostname` rather than a crate: this daemon is a systemd user
/// service on Linux and the file is always there. A host with no readable hostname gets
/// a constant rather than an empty line on a phone screen.
fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "scanbus".to_owned())
}

/// Publish §12.1's record, or say once why this host will not be findable after its
/// address changes.
///
/// Failure here is deliberately not the failure a taken `upload_port` is. A port that
/// cannot be bound means no phone can upload at all; a responder that will not start
/// means one recovery path is missing on a host whose scanning works, and it is only
/// read after a stored address has already refused a connection. So it is one `warn`,
/// every scanner stays online, and construction still succeeds.
fn advertise_host(record: HostRecord<'_>) -> Option<Arc<mdns::Registration>> {
    match mdns::register(HOST_SERVICE_TYPE, record.instance, record.port, &record.txt) {
        Ok(registration) => {
            debug!(
                fullname = registration.fullname(),
                port = record.port,
                "advertising this host, so a paired phone can find it after the lease changes"
            );
            Some(Arc::new(registration))
        }
        Err(error) => {
            warn!(
                service_type = HOST_SERVICE_TYPE,
                %error,
                "could not advertise this host; paired phones keep working, but one whose \
                 stored address stops answering will not find this host on its own"
            );
            None
        }
    }
}

/// `mobile.require_tls` as [`REQUIRE_TLS_ENV`] spells it. Default `false` (§11.5).
fn require_tls_from_env() -> bool {
    require_tls_setting(std::env::var_os(REQUIRE_TLS_ENV).as_deref())
}

/// The reading of it, split out because a process-wide variable cannot be set by one test
/// without setting it for every other test in the binary.
///
/// A value that is neither spelling is a typo — `ture`, `enabled`, `TLS` — and the quiet
/// way to handle it is to leave the switch off, which is the failure somebody discovers
/// by finding their traffic in clear months later. So it is refused loudly and still left
/// off: aborting over an environment variable would take out a daemon that was working
/// before somebody edited a unit file, and an operator who set this wants their phones to
/// keep scanning, not their host to stop.
fn require_tls_setting(value: Option<&std::ffi::OsStr>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value = value.to_string_lossy().trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" => true,
        "" | "0" | "false" | "no" | "off" => false,
        other => {
            warn!(
                variable = REQUIRE_TLS_ENV,
                value = other,
                "unrecognised value; mobile TLS is NOT required. Use 1 or 0"
            );
            false
        }
    }
}

fn default_store_path() -> PathBuf {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data_home)
            .join("scanbus")
            .join("mobile")
            .join("devices.json");
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("scanbus")
        .join("mobile")
        .join("devices.json")
}

fn load_or_reset_store(path: &Path) -> PersistedDeviceStore {
    match load_store(path) {
        Ok(store) => store,
        Err(reason) => {
            rename_store_aside(path, &reason);
            PersistedDeviceStore::default()
        }
    }
}

fn load_store(path: &Path) -> Result<PersistedDeviceStore, String> {
    if !path.exists() {
        return Ok(PersistedDeviceStore::default());
    }

    let bytes = fs::read(path).map_err(|error| {
        format!(
            "cannot read mobile device store {}: {error}",
            path.display()
        )
    })?;
    let mut store: PersistedDeviceStore = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "cannot parse mobile device store {}: {error}",
            path.display()
        )
    })?;

    // Only a store from the future is unreadable. An older one is upgraded in place:
    // every field added since is `#[serde(default)]` and the default is what the older
    // record actually meant, so the upgrade is the deserialization itself and the version
    // is only stamped forward for the next write. Treating an old store the way
    // [`rename_store_aside`] treats a corrupt one would drop every pairing on the host on
    // the upgrade that introduced the field — §11.6 is explicit that pairings made before
    // TLS keep working.
    if store.version > DEVICE_STORE_VERSION {
        return Err(format!(
            "mobile device store version {} is newer than this scanbus understands \
			 (writes {})",
            store.version, DEVICE_STORE_VERSION
        ));
    }
    if store.version < DEVICE_STORE_VERSION {
        debug!(
            path = %path.display(),
            from = store.version,
            to = DEVICE_STORE_VERSION,
            "upgrading mobile device store; pairings without a tls flag are cleartext"
        );
        store.version = DEVICE_STORE_VERSION;
    }

    Ok(store)
}

fn rename_store_aside(path: &Path, reason: &str) {
    if !path.exists() {
        return;
    }

    let stamp = unix_timestamp_now();
    let aside = path.with_extension(format!("json.unreadable.{stamp}"));
    match fs::rename(path, &aside) {
        Ok(()) => warn!(
            from = %path.display(),
            to = %aside.display(),
            reason,
            "mobile device store is unreadable; renamed aside and starting empty"
        ),
        Err(error) => warn!(
            path = %path.display(),
            %error,
            reason,
            "mobile device store is unreadable; starting empty without renaming"
        ),
    }
}

fn persist_device_store(path: &Path, store: &PersistedDeviceStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cannot resolve parent directory for {}", path.display()))?;
    ensure_store_dir(parent)?;

    let payload = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("cannot serialize mobile device store: {error}"))?;
    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|error| {
            format!(
                "cannot open temporary device store {}: {error}",
                tmp.display()
            )
        })?;
    file.write_all(&payload).map_err(|error| {
        format!(
            "cannot write temporary device store {}: {error}",
            tmp.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "cannot fsync temporary device store {}: {error}",
            tmp.display()
        )
    })?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|error| {
        format!(
            "cannot enforce mode 0600 on temporary device store {}: {error}",
            tmp.display()
        )
    })?;

    fs::rename(&tmp, path)
        .map_err(|error| format!("cannot replace device store {}: {error}", path.display()))?;
    let dir = File::open(parent).map_err(|error| {
        format!(
            "cannot open device store directory {}: {error}",
            parent.display()
        )
    })?;
    dir.sync_all().map_err(|error| {
        format!(
            "cannot fsync device store directory {} after rename: {error}",
            parent.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        format!(
            "cannot enforce mode 0600 on device store {}: {error}",
            path.display()
        )
    })?;

    Ok(())
}

fn ensure_store_dir(path: &Path) -> Result<(), String> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "cannot create mobile device store directory {}: {error}",
                path.display()
            )
        })?;
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "cannot enforce mode 0700 on mobile device store directory {}: {error}",
            path.display()
        )
    })
}

fn bind_listener(requested_port: u16) -> ListenerBinding {
    match bind_listener_once(requested_port) {
        Ok((listener, port)) => ListenerBinding {
            port,
            bind_error: None,
            listener: Some(listener),
            task_started: false,
        },
        Err(error) => ListenerBinding {
            port: requested_port,
            bind_error: Some(error),
            listener: None,
            task_started: false,
        },
    }
}

fn bind_listener_once(requested_port: u16) -> Result<(std::net::TcpListener, u16), String> {
    match std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, requested_port)) {
        Ok(listener) => {
            let port = listener
                .local_addr()
                .map_err(|error| {
                    format!("could not read the bound IPv6 listener address: {error}")
                })?
                .port();
            Ok((listener, port))
        }
        Err(ipv6_error) => {
            let listener =
                std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, requested_port))
                    .map_err(|ipv4_error| {
                        format!(
                            "could not bind mobile upload listener on port {requested_port}: \
							 IPv6: {ipv6_error}; IPv4 fallback: {ipv4_error}"
                        )
                    })?;
            let port = listener
                .local_addr()
                .map_err(|error| {
                    format!("could not read the bound IPv4 listener address: {error}")
                })?
                .port();
            Ok((listener, port))
        }
    }
}

/// The six digits of §4.2 step 2: uniform over `000000`–`999999`, zero-padded.
///
/// `random_range` over the full range and then formatting is what keeps `000042` as
/// likely as `482913` — sampling six digits from a character set, or formatting a
/// number without the padding, would either bias the draw or silently shorten a code
/// the user has to compare character by character.
fn generate_nonce() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000_u32))
}

fn discover_once(timeout: Duration) -> Result<Vec<(ScannerInfo, DiscoveryRecord)>, BackendError> {
    let services = mdns::browse(&[SERVICE_TYPE], timeout).map_err(|error| match error {
        // A browser that cannot start is a host-side problem, not "no phone answered",
        // so it keeps the shape the daemon can act on.
        mdns::BrowseError::DaemonUnavailable(_) => BackendError::NotReachable {
            scanner: ScannerId::from_backend(ID, "discovery")
                .expect("static discovery id is valid"),
            detail: error.to_string(),
        },
        mdns::BrowseError::BrowseRefused(_) => BackendError::Other(error.to_string()),
    })?;

    let mut seen = BTreeMap::<ScannerId, (ScannerInfo, DiscoveryRecord)>::new();
    for service in &services {
        let Some((scanner, record)) = scanner_from_service(service) else {
            continue;
        };
        if seen.contains_key(&scanner.id) {
            warn!(
                scanner_id = %scanner.id,
                instance = %service.get_fullname(),
                "duplicate mobile scanner id discovered; keeping first one"
            );
            continue;
        }
        seen.insert(scanner.id.clone(), (scanner, record));
    }

    Ok(seen.into_values().collect())
}

fn scanner_from_service(service: &mdns_sd::ServiceInfo) -> Option<(ScannerInfo, DiscoveryRecord)> {
    let instance = service.get_fullname().to_owned();
    let properties = service.get_properties();

    let id = match properties.get_property_val_str("id") {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            debug!(instance = %instance, "dropping mobile service with no txt id");
            return None;
        }
    };

    let version = match properties
        .get_property_val_str("v")
        .and_then(|v| v.parse::<u32>().ok())
    {
        Some(v) => v,
        None => {
            debug!(instance = %instance, "dropping mobile service with missing/invalid txt v");
            return None;
        }
    };

    if version != PROTOCOL_VERSION {
        debug!(
            instance = %instance,
            seen = version,
            supported = PROTOCOL_VERSION,
            "dropping mobile service with unsupported protocol version"
        );
        return None;
    }

    let scanner_id = match ScannerId::from_backend(ID, id) {
        Ok(scanner_id) => scanner_id,
        Err(error) => {
            debug!(instance = %instance, %error, "dropping mobile service with invalid txt id");
            return None;
        }
    };

    let Some(ip) = service.get_addresses().iter().next() else {
        debug!(instance = %instance, "dropping mobile service with no resolved address");
        return None;
    };

    // `TcpStream::connect` accepts `host:port` for IPv4 and `[host]:port` for IPv6.
    // Hand-formatting with `"{}:{}"` turns a link-local IPv6 phone such as
    // `fe80::...` into the ambiguous `fe80::...:45481`, which the socket layer rejects
    // before any network attempt happens.
    let address = SocketAddr::new(*ip, service.get_port()).to_string();

    // §11.3. Absence is a statement — "no keystore identity" — and there is deliberately
    // no `tls=0`, so a phone written before §11 reads exactly as it always did and no
    // other TXT key changes meaning.
    //
    // A key that is present but unusable is treated as absent rather than dropping the
    // service, and the reason is that it buys nothing: an attacker who can forge this
    // record can strip the key outright, and §11.6 already accounts for that path — the
    // pairing that follows is cleartext, the app marks it unencrypted and warns, and the
    // six digits are what bound it. A garbled value reaches the same place, so refusing
    // to show the phone at all would only take a working cleartext pairing away from
    // someone whose app has a bug. It is logged at `warn` and not at `debug` because,
    // unlike the drops above, this one is nobody's normal traffic.
    let tls_address = match properties.get_property_val_str("tlsport") {
        None => None,
        Some(value) => match value.trim().parse::<u16>() {
            // Port 0 means "any port" to a bind and is not connectable, so it is not a
            // port a phone can be serving on.
            Ok(port) if port != 0 => Some(SocketAddr::new(*ip, port).to_string()),
            _ => {
                warn!(
                    instance = %instance,
                    tlsport = %value,
                    "ignoring unusable tlsport; pairing this phone will be cleartext"
                );
                None
            }
        },
    };

    Some((
        ScannerInfo {
            id: scanner_id,
            name: instance_name(&instance),
            backend: ID.to_owned(),
            address: address.clone(),
            capabilities: ScannerCapabilities {
                buttons: ButtonsCapability {
                    count: 0,
                    label_configurable: false,
                    labels: Vec::new(),
                },
                ..ScannerCapabilities::default()
            },
            status: Status::Online,
        },
        DiscoveryRecord {
            device_id: id.to_owned(),
            address,
            tls_address,
            seen_at: Instant::now(),
        },
    ))
}

fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|value| value.strip_suffix('.'))
        .unwrap_or(fullname)
        .to_owned()
}

fn link_local_candidates(ip: Ipv6Addr, port: u16) -> std::io::Result<Vec<SocketAddrV6>> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for interface in get_if_addrs()? {
        let is_loopback = interface.is_loopback();
        let IfAddr::V6(ref addr) = interface.addr else {
            continue;
        };
        if !addr.is_link_local() || is_loopback {
            continue;
        }
        let Some(index) = interface.index.filter(|index| *index != 0) else {
            continue;
        };
        if seen.insert(index) {
            candidates.push(SocketAddrV6::new(ip, port, 0, index));
        }
    }

    Ok(candidates)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Control,
    Page,
}

impl FrameKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Page => "page",
        }
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Protocol-level refusal and parse errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unauthorized for device_id={device_id}")]
    Unauthorized { device_id: String },
    #[error("unsupported protocol version: seen={seen}, supported={supported}")]
    UnsupportedVersion { seen: u32, supported: u32 },
    #[error("not connected for device_id={device_id}")]
    NotConnected { device_id: String },
    #[error("malformed frame or message: {context}")]
    Malformed { context: String },
    #[error("frame too large: kind={kind}, len={len}, max={max}")]
    TooLarge {
        kind: FrameKind,
        len: u32,
        max: usize,
    },
    #[error("unsupported message type: {message_type}")]
    Unsupported { message_type: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    PairRequest(PairRequest),
    PairResponse(PairResponse),
    Upload(Upload),
    Ack(Ack),
}

impl Message {
    pub fn validate_version(&self) -> Result<(), ProtocolError> {
        match self {
            Message::PairRequest(msg) => validate_version(msg.v),
            Message::Upload(msg) => validate_version(msg.v),
            Message::PairResponse(_) | Message::Ack(_) => Ok(()),
        }
    }
}

fn validate_version(v: u32) -> Result<(), ProtocolError> {
    if v == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion {
            seen: v,
            supported: PROTOCOL_VERSION,
        })
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairRequest {
    pub v: u32,
    pub host_id: String,
    pub host_name: String,
    pub upload_port: u16,
    /// The six digits the user compares. **Never logged**, at any level — see this
    /// type's [`fmt::Debug`].
    pub nonce: String,
}

/// The nonce is redacted, and it is redacted *here* rather than at each call site: a
/// `debug!(?request)` added later is exactly how a code that is only meaningful because
/// nobody else has seen it ends up in a log file.
impl fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairRequest")
            .field("v", &self.v)
            .field("host_id", &self.host_id)
            .field("host_name", &self.host_name)
            .field("upload_port", &self.upload_port)
            .field("nonce", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairResponse {
    pub accepted: bool,
    pub device_id: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// The upload credential. **Never logged**, at any level — see this type's
    /// [`fmt::Debug`].
    #[serde(default)]
    pub token: String,
}

/// Same reasoning as [`PairRequest`]'s: the token is redacted at the type.
impl fmt::Debug for PairResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairResponse")
            .field("accepted", &self.accepted)
            .field("device_id", &self.device_id)
            .field("capabilities", &self.capabilities)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Capabilities {
    #[serde(default, deserialize_with = "deserialize_profiles_lossy")]
    pub profiles: Vec<ProfileKind>,
}

fn deserialize_profiles_lossy<'de, D>(deserializer: D) -> Result<Vec<ProfileKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    let mut parsed = Vec::with_capacity(raw.len());
    for value in raw {
        if let Ok(kind) = value.parse::<ProfileKind>() {
            parsed.push(kind);
        }
    }
    Ok(parsed)
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upload {
    pub v: u32,
    pub device_id: String,
    /// The credential the phone got when it paired. **Never logged**, at any level.
    pub token: String,
    pub profile: ProfileKind,
    pub page: u32,
    pub of: u32,
    pub format: String,
}

/// An upload header is the one message the daemon will want to trace per page (9.5), so
/// its token is redacted at the type for the same reason [`PairRequest`]'s nonce is.
impl fmt::Debug for Upload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upload")
            .field("v", &self.v)
            .field("device_id", &self.device_id)
            .field("token", &"<redacted>")
            .field("profile", &self.profile)
            .field("page", &self.page)
            .field("of", &self.of)
            .field("format", &self.format)
            .finish()
    }
}

impl Upload {
    pub fn validate_page_bounds(&self) -> Result<(), ProtocolError> {
        if self.page == 0 || self.of == 0 {
            return Err(ProtocolError::Malformed {
                context: format!(
                    "page/of must be >= 1, got page={}, of={}",
                    self.page, self.of
                ),
            });
        }
        if self.page > self.of {
            return Err(ProtocolError::Malformed {
                context: format!("page must be <= of, got page={}, of={}", self.page, self.of),
            });
        }
        if self.of > MAX_PAGES_PER_JOB {
            return Err(ProtocolError::Malformed {
                context: format!("of must be <= {}, got {}", MAX_PAGES_PER_JOB, self.of),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ack {
    pub status: AckStatus,
    #[serde(default)]
    pub reason: Option<AckReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckReason {
    Unauthorized,
    UnsupportedVersion,
    NotConnected,
    Malformed,
    TooLarge,
    Unsupported,
}

impl fmt::Display for AckReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            AckReason::Unauthorized => "unauthorized",
            AckReason::UnsupportedVersion => "unsupported_version",
            AckReason::NotConnected => "not_connected",
            AckReason::Malformed => "malformed",
            AckReason::TooLarge => "too_large",
            AckReason::Unsupported => "unsupported",
        };
        f.write_str(value)
    }
}

pub fn parse_control_message(bytes: &[u8]) -> Result<Message, ProtocolError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|err| ProtocolError::Malformed {
            context: format!("invalid JSON: {err}"),
        })?;

    let msg_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ProtocolError::Malformed {
            context: "missing or non-string 'type' field".to_string(),
        })?;

    match msg_type.as_str() {
        "pair_request" | "pair_response" | "upload" | "ack" => {
            serde_json::from_value::<Message>(value).map_err(|err| ProtocolError::Malformed {
                context: format!("message parse failed for type '{msg_type}': {err}"),
            })
        }
        other => Err(ProtocolError::Unsupported {
            message_type: other.to_string(),
        }),
    }
}

pub fn serialize_control_message(message: &Message) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(message).map_err(|err| ProtocolError::Malformed {
        context: format!("message serialization failed: {err}"),
    })
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    kind: FrameKind,
    max_bytes: usize,
) -> Result<Vec<u8>, ProtocolError> {
    let len = reader
        .read_u32()
        .await
        .map_err(|err| ProtocolError::Malformed {
            context: format!("failed to read {kind:?} frame length: {err}"),
        })?;

    if len == 0 {
        return Err(ProtocolError::Malformed {
            context: format!("zero-length {} frame", kind.as_str()),
        });
    }

    if len as usize > max_bytes {
        return Err(ProtocolError::TooLarge {
            kind,
            len,
            max: max_bytes,
        });
    }

    let mut buffer = vec![0_u8; len as usize];
    reader
        .read_exact(&mut buffer)
        .await
        .map_err(|err| ProtocolError::Malformed {
            context: format!(
                "truncated {} frame: expected {} bytes payload: {}",
                kind.as_str(),
                len,
                err
            ),
        })?;

    Ok(buffer)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: FrameKind,
    payload: &[u8],
    max_bytes: usize,
) -> Result<(), ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::Malformed {
            context: format!("zero-length {} frame", kind.as_str()),
        });
    }

    if payload.len() > max_bytes {
        return Err(ProtocolError::TooLarge {
            kind,
            len: payload.len() as u32,
            max: max_bytes,
        });
    }

    writer
        .write_u32(payload.len() as u32)
        .await
        .map_err(|err| ProtocolError::Malformed {
            context: format!("failed to write {} frame length: {}", kind.as_str(), err),
        })?;
    writer
        .write_all(payload)
        .await
        .map_err(|err| ProtocolError::Malformed {
            context: format!("failed to write {} frame payload: {}", kind.as_str(), err),
        })?;
    writer
        .flush()
        .await
        .map_err(|err| ProtocolError::Malformed {
            context: format!("failed to flush {} frame: {}", kind.as_str(), err),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io, pin::Pin, task::Poll};

    use futures_util::StreamExt as _;
    use tempfile::TempDir;
    use tokio::io::{AsyncRead, ReadBuf, duplex};
    use tokio::net::TcpListener;

    use super::*;

    /// A phone that answers one pairing, and what it did while doing so.
    ///
    /// Stands in for the simulator of [9.6] until that exists: the acceptance cases of
    /// 9.3 are all about what the *host* does with a given answer, and a `TcpListener`
    /// that speaks the four framing rules is the whole of what is needed to produce one.
    ///
    /// [9.6]: https://github.com/jeanparpaillon/scanbus_service/issues/45
    struct FakePhone {
        address: String,
        /// The second port of §11.3, when this phone has a keystore identity — what it
        /// would advertise as `tlsport`.
        tls_address: Option<String>,
        /// What the phone read out of `pair_request`, once it has.
        request: Arc<Mutex<Option<PairRequest>>>,
        /// Set when the connection reached end-of-file — what `CancelPairing()` looks
        /// like from the app's side.
        disconnected: Arc<Mutex<bool>>,
        /// The client certificate the host presented, which is the thing the real app
        /// pins (§11.2).
        client_certificate: Arc<Mutex<Option<rustls::pki_types::CertificateDer<'static>>>>,
        /// How many times the cleartext SRV port was dialled. Zero is the assertion in
        /// every TLS test: §11.3 keeps that port cleartext forever, so a host that
        /// touches it has either ignored `tlsport` or retried after a failed handshake.
        cleartext_dials: Arc<AtomicU64>,
    }

    /// What the phone serves on its second port, if it has one (§11.3).
    #[derive(Clone, Copy)]
    enum PhoneTls {
        /// No `tlsport` in TXT: a phone with no keystore identity, or one built before
        /// §11.
        None,
        /// A TLS port with its own self-signed keystore certificate, offering these
        /// protocol versions.
        Serves(&'static [&'static rustls::SupportedProtocolVersion]),
        /// A port advertised as TLS that answers with something else. A broken phone —
        /// and the case a cleartext retry would paper over.
        Broken,
    }

    /// How the phone answers, once it has read the request.
    #[derive(Clone, Copy)]
    enum Answer {
        /// Accept, with this `device_id` — the same one it advertised, or another.
        Accept { device_id: &'static str },
        /// The user tapped reject.
        Reject,
        /// Stay connected and say nothing, ever.
        Silence,
    }

    impl FakePhone {
        async fn listen(answer: Answer) -> Self {
            Self::listen_with_tls(answer, PhoneTls::None).await
        }

        async fn listen_with_tls(answer: Answer, tls: PhoneTls) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let request = Arc::new(Mutex::new(None));
            let disconnected = Arc::new(Mutex::new(false));
            let client_certificate = Arc::new(Mutex::new(None));
            let cleartext_dials = Arc::new(AtomicU64::new(0));

            let seen = Arc::clone(&request);
            let closed = Arc::clone(&disconnected);
            let dials = Arc::clone(&cleartext_dials);
            tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                dials.fetch_add(1, Ordering::SeqCst);
                answer_pairing(socket, answer, seen, closed).await;
            });

            let tls_address = match tls {
                PhoneTls::None => None,
                PhoneTls::Serves(versions) => {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let tls_address = listener.local_addr().unwrap().to_string();
                    let config = Arc::new(phone_server_config(
                        versions,
                        Arc::clone(&client_certificate),
                    ));
                    let seen = Arc::clone(&request);
                    let closed = Arc::clone(&disconnected);
                    tokio::spawn(async move {
                        let (socket, _) = listener.accept().await.unwrap();
                        let socket = tokio_rustls::TlsAcceptor::from(config)
                            .accept(socket)
                            .await
                            .expect("the host completed the handshake");
                        answer_pairing(socket, answer, seen, closed).await;
                    });
                    Some(tls_address)
                }
                PhoneTls::Broken => {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let tls_address = listener.local_addr().unwrap().to_string();
                    tokio::spawn(async move {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        // The pairing framing, on the port it said was TLS. Not noise: a
                        // phone that got its two ports the wrong way round is the most
                        // likely form of this bug, and it is the one a cleartext retry
                        // would turn into a silent downgrade.
                        let _ = socket.write_all(b"\0\0\0\x04null").await;
                    });
                    Some(tls_address)
                }
            };

            Self {
                address,
                tls_address,
                request,
                disconnected,
                client_certificate,
                cleartext_dials,
            }
        }

        fn nonce_shown(&self) -> Option<String> {
            self.request
                .lock()
                .unwrap()
                .as_ref()
                .map(|request| request.nonce.clone())
        }

        /// The certificate the host presented as a client certificate, i.e. what the app
        /// would pin.
        fn pinned_certificate(&self) -> Option<rustls::pki_types::CertificateDer<'static>> {
            self.client_certificate.lock().unwrap().clone()
        }

        fn cleartext_dials(&self) -> u64 {
            self.cleartext_dials.load(Ordering::SeqCst)
        }
    }

    /// One pairing, over whichever of the phone's two ports the host chose.
    async fn answer_pairing<S: AsyncRead + AsyncWrite + Unpin>(
        mut socket: S,
        answer: Answer,
        seen: Arc<Mutex<Option<PairRequest>>>,
        closed: Arc<Mutex<bool>>,
    ) {
        let frame = read_frame(&mut socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap();
        let Message::PairRequest(pair_request) = parse_control_message(&frame).unwrap() else {
            panic!("the host sent something that is not a pair_request");
        };
        *seen.lock().unwrap() = Some(pair_request);

        let response = match answer {
            Answer::Accept { device_id } => PairResponse {
                accepted: true,
                device_id: device_id.to_owned(),
                capabilities: Capabilities {
                    profiles: vec![ProfileKind::Image],
                },
                token: "phone-token".to_owned(),
            },
            Answer::Reject => PairResponse {
                accepted: false,
                device_id: DEVICE_ID.to_owned(),
                capabilities: Capabilities::default(),
                token: String::new(),
            },
            Answer::Silence => {
                // Nothing is sent, and the socket is held open: the host has to be the
                // one that gives up. Reading is how the app notices the host closing it,
                // which is what `CancelPairing()` does.
                let mut byte = [0_u8; 1];
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    *closed.lock().unwrap() = true;
                }
                return;
            }
        };

        let payload = serialize_control_message(&Message::PairResponse(response)).unwrap();
        write_frame(
            &mut socket,
            FrameKind::Control,
            &payload,
            DEFAULT_CONTROL_MAX_BYTES,
        )
        .await
        .unwrap();
    }

    /// The phone's side of the pairing TLS: a fresh self-signed `CN=scanbus` keystore
    /// certificate that nothing on the host could ever vouch for, and a
    /// `CertificateRequest` with **no** `certificate_authorities` — which is the shape
    /// §11.3 warns a client library can silently decline to answer.
    fn phone_server_config(
        versions: &[&'static rustls::SupportedProtocolVersion],
        seen: Arc<Mutex<Option<rustls::pki_types::CertificateDer<'static>>>>,
    ) -> rustls::ServerConfig {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "scanbus");
        let certificate = params.self_signed(&key_pair).unwrap();

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(versions)
            .unwrap()
            .with_client_cert_verifier(Arc::new(PhoneAcceptsAnyClient { seen, provider }))
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
            )
            .unwrap()
    }

    /// The app does not validate the certificate it is about to pin — it pins it. This
    /// records it instead, which is the same thing minus the storage.
    #[derive(Debug)]
    struct PhoneAcceptsAnyClient {
        seen: Arc<Mutex<Option<rustls::pki_types::CertificateDer<'static>>>>,
        provider: Arc<rustls::crypto::CryptoProvider>,
    }

    impl rustls::server::danger::ClientCertVerifier for PhoneAcceptsAnyClient {
        fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
            &[]
        }

        fn verify_client_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
            *self.seen.lock().unwrap() = Some(end_entity.clone().into_owned());
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// What the fake phone puts in its TXT `id`.
    const DEVICE_ID: &str = "phone_a1b2c3";

    /// A `host_id` that was already in the store — the 32 lowercase hex of §12.1, fixed
    /// rather than drawn so a test can say which value it expects to survive.
    const HOST_ID: &str = "0123456789abcdef0123456789abcdef";

    /// A phone with no TLS 1.3 — every Android 8 and 9 one (§11.3).
    const TLS12_ONLY: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];

    fn scanner_id() -> ScannerId {
        ScannerId::from_backend(ID, DEVICE_ID).unwrap()
    }

    fn scanner_info() -> ScannerInfo {
        ScannerInfo {
            id: scanner_id(),
            name: "Pixel 7".to_owned(),
            backend: ID.to_owned(),
            address: "127.0.0.1:1".to_owned(),
            capabilities: ScannerCapabilities::default(),
            status: Status::Online,
        }
    }

    /// A backend that has just seen `address`, with the handshake deadlines shortened so
    /// a timeout is observable inside a test.
    fn backend_that_saw(address: &str) -> (TempDir, MobileBackend) {
        backend_that_saw_ports(address, None)
    }

    /// The same, for a phone that also advertised a `tlsport`.
    fn backend_that_saw_phone(phone: &FakePhone) -> (TempDir, MobileBackend) {
        backend_that_saw_ports(&phone.address, phone.tls_address.clone())
    }

    fn backend_that_saw_ports(
        address: &str,
        tls_address: Option<String>,
    ) -> (TempDir, MobileBackend) {
        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp)
            .with_pairing_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        backend.remember(vec![(
            scanner_id(),
            DiscoveryRecord {
                device_id: DEVICE_ID.to_owned(),
                address: address.to_owned(),
                tls_address,
                seen_at: Instant::now(),
            },
        )]);
        (tmp, backend)
    }

    fn backend_in(tmp: &TempDir) -> MobileBackend {
        MobileBackend::with_store_path(tmp.path().join("devices.json"), DISCOVERY_TIMEOUT, 0)
    }

    /// Runs one pairing and returns its outcome along with every progress step it sent.
    async fn pair_with(
        backend: &MobileBackend,
    ) -> (Result<(), BackendError>, Vec<scanbus_core::PairingProgress>) {
        let (tx, mut rx) = mpsc::channel(8);
        let outcome = backend.ensure_installed(&scanner_info(), tx).await;

        let mut steps = Vec::new();
        while let Ok(step) = rx.try_recv() {
            steps.push(step);
        }
        (outcome, steps)
    }

    /// The happy path of §4.2: the code reaches the client, the phone gets the same one,
    /// and what it answers with is what the backend remembers.
    #[tokio::test]
    async fn a_confirmed_pairing_shows_the_code_and_keeps_what_the_phone_returned() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);

        let (outcome, steps) = pair_with(&backend).await;
        outcome.unwrap();

        // Exactly one transition, and it is the one that puts a code on screen.
        let [scanbus_core::PairingProgress::AwaitingConfirmation { code }] = steps.as_slice()
        else {
            panic!("expected one AwaitingConfirmation, got {steps:?}");
        };
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        // Byte-identical to what the phone would print — the point of the comparison.
        assert_eq!(phone.nonce_shown().as_deref(), Some(code.as_str()));

        // The token is what an upload will be checked against, and the profiles are what
        // `SupportedProfiles` narrows to.
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));
        assert!(!backend.is_authorized(DEVICE_ID, "some-other-token"));
        assert_eq!(
            backend.advertised_profiles(&scanner_id()),
            vec![ProfileKind::Image]
        );
        // No `tlsport` was advertised, so the pairing was cleartext and is recorded as
        // such — §11.6's "never retroactively upgraded" starts here.
        assert!(!backend.lock_paired()[&scanner_id()].tls);
    }

    /// §11.3 and §11.2 together, and the one test that would catch a second certificate
    /// being minted for either role: the host dials the advertised `tlsport`, and what
    /// arrives at the phone as a client certificate is the identity the upload listener
    /// will present.
    #[tokio::test]
    async fn a_phone_that_advertises_tlsport_is_dialled_there_and_pinned_to_our_certificate() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Serves(rustls::ALL_VERSIONS),
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);

        let (outcome, steps) = pair_with(&backend).await;
        outcome.unwrap();

        let pinned = phone
            .pinned_certificate()
            .expect("the phone was sent a client certificate to pin");
        assert_eq!(
            hex_lower(&Sha256::digest(pinned.as_ref())),
            backend.certificate_fingerprint().unwrap()
        );
        assert_eq!(&pinned, backend.identity().unwrap().certificate());

        // The SRV port is cleartext forever (§11.3): a host that touched it here either
        // ignored `tlsport` or retried after the handshake.
        assert_eq!(phone.cleartext_dials(), 0);
        assert!(backend.lock_paired()[&scanner_id()].tls);

        // And the pairing itself is unchanged by the transport: same six digits, same
        // frames, same stored token.
        let [scanbus_core::PairingProgress::AwaitingConfirmation { code }] = steps.as_slice()
        else {
            panic!("expected one AwaitingConfirmation, got {steps:?}");
        };
        assert_eq!(phone.nonce_shown().as_deref(), Some(code.as_str()));
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));
    }

    /// Android 8 and 9 have no TLS 1.3, and §11.3 accepts 1.2 rather than refusing those
    /// phones for a guarantee the six digits already give.
    #[tokio::test]
    async fn a_phone_that_offers_only_tls_1_2_still_pairs() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Serves(TLS12_ONLY),
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);

        pair_with(&backend).await.0.unwrap();

        assert!(phone.pinned_certificate().is_some());
        assert!(backend.lock_paired()[&scanner_id()].tls);
    }

    /// The rule with no second chance: a phone that advertised `tlsport` and cannot
    /// finish a handshake there is broken, and the SRV port that would have worked is
    /// left alone. An automatic downgrade is something an attacker gets to trigger, and
    /// it is cheaper for them to break a handshake than to strip the TXT key.
    #[tokio::test]
    async fn a_tlsport_that_does_not_speak_tls_is_not_retried_in_cleartext() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Broken,
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);

        let (outcome, steps) = pair_with(&backend).await;

        let error = outcome.expect_err("a failed handshake ends the pairing");
        assert!(
            matches!(&error, BackendError::NotReachable { detail, .. } if detail.contains("not retried in cleartext")),
            "{error:?}"
        );
        assert_eq!(phone.cleartext_dials(), 0);
        // No code was ever shown, so nothing asked a human to confirm a pairing that was
        // never going to be stored.
        assert!(steps.is_empty(), "{steps:?}");
        assert!(backend.lock_paired().is_empty());
    }

    /// §11.5's switch, pairing half. The phone here is not broken — it is an older or
    /// smaller build with no keystore identity, which every other test in this file pairs
    /// happily. On the network §11.5 describes it is refused instead, and refused before
    /// the dial: the absent TXT key is everything the phone has to say about its own TLS,
    /// and it cannot acquire one by being asked again.
    #[tokio::test]
    async fn require_tls_refuses_to_pair_a_phone_that_advertises_no_tlsport() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        let backend = backend.with_require_tls(true);

        let (outcome, steps) = pair_with(&backend).await;

        let error = outcome.expect_err("a phone with no tlsport is not paired here");
        // Named, per the acceptance: "this phone is too old for this network" and
        // "somebody set a variable" are the same screen otherwise.
        assert!(
            error.to_string().contains(REQUIRE_TLS_ENV),
            "the reason has to name the switch: {error}"
        );
        // Nothing was dialled and nobody was asked to confirm anything.
        assert!(steps.is_empty(), "{steps:?}");
        assert_eq!(phone.nonce_shown(), None);
        assert!(backend.lock_paired().is_empty());
        assert!(!backend.is_authorized(DEVICE_ID, "phone-token"));
    }

    /// And the other half of the switch: it refuses one kind of phone, not pairing. A
    /// phone that advertises `tlsport` pairs exactly as it does with the switch off,
    /// which is what makes this a policy and not an outage.
    #[tokio::test]
    async fn require_tls_pairs_a_phone_that_advertises_tlsport_as_usual() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Serves(rustls::ALL_VERSIONS),
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);
        let backend = backend.with_require_tls(true);

        pair_with(&backend).await.0.unwrap();

        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));
        assert!(backend.lock_paired()[&scanner_id()].tls);
    }

    /// The phone advertised what it can do, so `SupportedProfiles` says that and not the
    /// daemon's full list — including across the discovery round that follows.
    #[tokio::test]
    async fn a_paired_phone_keeps_narrowing_supported_profiles() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();

        let mut info = scanner_info();
        assert_eq!(
            info.supported_profiles(),
            vec![ProfileKind::Image, ProfileKind::Document],
            "an info the backend has not filled in yet keeps the daemon's list"
        );

        info.capabilities.profiles = backend.advertised_profiles(&info.id);
        assert_eq!(info.supported_profiles(), vec![ProfileKind::Image]);
    }

    /// §4.2 step 5, with the message the API document promises verbatim.
    #[tokio::test]
    async fn a_rejection_on_the_device_is_reported_as_such() {
        let phone = FakePhone::listen(Answer::Reject).await;
        let (_tmp, backend) = backend_that_saw(&phone.address);

        let (outcome, _) = pair_with(&backend).await;
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "rejected on the device",
            "this string is what PairingError shows a user"
        );
        assert!(!backend.is_authorized(DEVICE_ID, "phone-token"));
    }

    /// §4.2 step 6: the response and the TXT record come over different paths, and a
    /// mismatch is another device answering.
    #[tokio::test]
    async fn a_response_from_another_device_is_refused_and_stores_nothing() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: "phone_deadbeef",
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);

        let (outcome, _) = pair_with(&backend).await;
        let message = outcome.unwrap_err().to_string();
        assert!(
            message.contains("phone_deadbeef") && message.contains(DEVICE_ID),
            "the error has to name both ids: {message}"
        );
        assert!(!backend.is_authorized("phone_deadbeef", "phone-token"));
        assert!(backend.lock_paired().is_empty());
    }

    /// A phone that accepts the connection and never answers is a failure, not a hang.
    #[tokio::test]
    async fn silence_from_the_phone_ends_the_pairing_at_the_deadline() {
        let phone = FakePhone::listen(Answer::Silence).await;
        let (_tmp, backend) = backend_that_saw(&phone.address);

        let (outcome, steps) = pair_with(&backend).await;
        let message = outcome.unwrap_err().to_string();
        assert!(
            message.contains("did not confirm within"),
            "the error has to name the timeout: {message}"
        );
        // The code was shown before the wait, so the client had something to display for
        // the whole of it.
        assert!(matches!(
            steps.as_slice(),
            [scanbus_core::PairingProgress::AwaitingConfirmation { .. }]
        ));
        assert!(backend.lock_paired().is_empty());
    }

    /// `CancelPairing()` is 1.4 aborting the task; from the phone's side that is the
    /// socket closing, with no cancel message involved.
    #[tokio::test]
    async fn dropping_the_pairing_closes_the_socket_the_app_is_waiting_on() {
        let phone = FakePhone::listen(Answer::Silence).await;
        // Long deadlines: what ends this pairing must be the abort, not a timeout.
        let (_tmp, backend) = backend_that_saw(&phone.address);
        let backend = backend.with_pairing_timeouts(PAIR_CONNECT_TIMEOUT, PAIR_CONFIRM_TIMEOUT);

        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let _ = backend.ensure_installed(&scanner_info(), tx).await;
        });

        // Cancel only once the code is up, which is when a user would.
        let step = rx.recv().await.unwrap();
        assert!(matches!(
            step,
            scanbus_core::PairingProgress::AwaitingConfirmation { .. }
        ));
        task.abort();

        // The phone's read returns end-of-file rather than hanging for 120 s.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*phone.disconnected.lock().unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the app should have seen the connection drop");
    }

    /// The rule that makes pairing the one moment the host dials: no live sighting, no
    /// address, and nothing opened.
    #[tokio::test]
    async fn pairing_a_phone_that_was_never_seen_is_not_reachable() {
        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp);

        let (outcome, steps) = pair_with(&backend).await;
        assert!(
            matches!(outcome, Err(BackendError::NotReachable { .. })),
            "expected NotReachable, got {outcome:?}"
        );
        // No socket was opened, so no code was ever shown.
        assert!(steps.is_empty());
    }

    /// And a sighting from a session that has ended is not an address either — this is
    /// what stops a remembered address being dialled.
    #[tokio::test]
    async fn a_stale_sighting_is_not_an_address_to_dial() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp);
        backend.lock_discovered().insert(
            scanner_id(),
            DiscoveryRecord {
                device_id: DEVICE_ID.to_owned(),
                address: phone.address.clone(),
                tls_address: None,
                seen_at: Instant::now() - DISCOVERY_RECORD_TTL - Duration::from_secs(1),
            },
        );

        let (outcome, _) = pair_with(&backend).await;
        assert!(
            matches!(outcome, Err(BackendError::NotReachable { .. })),
            "expected NotReachable, got {outcome:?}"
        );
        // And the expired entry is gone rather than lingering to be found next time.
        assert!(backend.lock_discovered().is_empty());
    }

    /// `Unpair()` calls this, and after it an upload with the old token is unauthorized —
    /// with the phone never told, because it may be switched off.
    #[tokio::test]
    async fn forgetting_a_phone_revokes_the_token_it_uploads_with() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));

        backend.forget(&scanner_id()).await.unwrap();
        assert!(!backend.is_authorized(DEVICE_ID, "phone-token"));

        // Idempotent: `Unpair()` on a scanner already forgotten is not an error.
        backend.forget(&scanner_id()).await.unwrap();
    }

    /// A phone has no buttons, but pairing still has to reach `Done`, so this is a
    /// pending subscription and not the refusal it was before 9.3.
    #[tokio::test]
    async fn listening_to_a_phone_succeeds_and_stays_registered_until_dropped() {
        use futures_util::StreamExt as _;

        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp);
        let mut events = backend.start_listening(&scanner_info()).await.unwrap();
        assert!(backend.has_subscription(&scanner_id()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err(),
            "a mobile subscription should stay pending, not end immediately"
        );

        drop(events);
        assert!(!backend.has_subscription(&scanner_id()));
        backend.stop_listening(&scanner_id()).await.unwrap();
    }

    #[tokio::test]
    async fn stop_listening_deregisters_a_mobile_subscription() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();

        let _subscription = backend.start_listening(&scanner_info()).await.unwrap();
        assert!(backend.has_subscription(&scanner_id()));

        backend.stop_listening(&scanner_id()).await.unwrap();
        assert!(!backend.has_subscription(&scanner_id()));
    }

    async fn upload_once(port: u16, upload: Upload, page: Vec<u8>) -> Ack {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let payload = serialize_control_message(&Message::Upload(upload)).unwrap();
        write_frame(
            &mut socket,
            FrameKind::Control,
            &payload,
            DEFAULT_CONTROL_MAX_BYTES,
        )
        .await
        .unwrap();
        write_frame(&mut socket, FrameKind::Page, &page, DEFAULT_PAGE_MAX_BYTES)
            .await
            .unwrap();

        let ack = read_frame(&mut socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap();
        let Message::Ack(ack) = parse_control_message(&ack).unwrap() else {
            panic!("expected ack");
        };
        ack
    }

    /// What the app's side of a TLS upload learns, beyond the ack (§11.4).
    struct TlsUpload {
        ack: Ack,
        /// The leaf certificate the host presented. The real app compares this against
        /// what it pinned while pairing and hangs up if it differs.
        server_certificate: rustls::pki_types::CertificateDer<'static>,
        /// Whether a `CertificateRequest` arrived. It must not: the app has no identity
        /// and would answer it empty.
        asked_for_a_client_certificate: bool,
    }

    /// The same upload as [`upload_once`], over TLS — the app's side of §11.4.
    async fn upload_once_tls(port: u16, upload: Upload, page: Vec<u8>) -> TlsUpload {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let phone = Arc::new(PhoneUploadClient {
            provider: Arc::clone(&provider),
            certificate_requests: Arc::new(AtomicU64::new(0)),
        });
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(
                Arc::clone(&phone) as Arc<dyn rustls::client::danger::ServerCertVerifier>
            )
            // Not `with_no_client_auth`, which would make the assertion below untestable:
            // this resolver is what a `CertificateRequest` would reach, so counting its
            // calls is how "no client certificate is requested" is observed from here.
            .with_client_cert_resolver(
                Arc::clone(&phone) as Arc<dyn rustls::client::ResolvesClientCert>
            );

        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut socket = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(
                ServerName::from(std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)),
                socket,
            )
            .await
            .expect("the host completed the handshake");
        let server_certificate = socket
            .get_ref()
            .1
            .peer_certificates()
            .expect("the host presented a certificate")
            .first()
            .expect("a leaf certificate")
            .clone()
            .into_owned();

        let payload = serialize_control_message(&Message::Upload(upload)).unwrap();
        write_frame(
            &mut socket,
            FrameKind::Control,
            &payload,
            DEFAULT_CONTROL_MAX_BYTES,
        )
        .await
        .unwrap();
        write_frame(&mut socket, FrameKind::Page, &page, DEFAULT_PAGE_MAX_BYTES)
            .await
            .unwrap();

        let ack = read_frame(&mut socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap();
        let Message::Ack(ack) = parse_control_message(&ack).unwrap() else {
            panic!("expected ack");
        };
        TlsUpload {
            ack,
            server_certificate,
            asked_for_a_client_certificate: phone.certificate_requests.load(Ordering::SeqCst) > 0,
        }
    }

    /// The app uploading: it pins rather than validates, so it accepts any chain here and
    /// checks the fingerprint itself; and it has no certificate of its own to offer.
    #[derive(Debug)]
    struct PhoneUploadClient {
        provider: Arc<rustls::crypto::CryptoProvider>,
        certificate_requests: Arc<AtomicU64>,
    }

    impl rustls::client::ResolvesClientCert for PhoneUploadClient {
        fn resolve(
            &self,
            _root_hint_subjects: &[&[u8]],
            _sigschemes: &[rustls::SignatureScheme],
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            self.certificate_requests.fetch_add(1, Ordering::SeqCst);
            None
        }

        fn has_certs(&self) -> bool {
            false
        }
    }

    impl rustls::client::danger::ServerCertVerifier for PhoneUploadClient {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// One page, uploaded and fetched, with the trigger dance the listener requires: it
    /// waits for somebody to claim the pages before it acks, so a test that only waits for
    /// the ack deadlocks.
    async fn fetch_one_page(
        backend: &MobileBackend,
        triggers: &mut BoxStream<'static, scanbus_core::ScanTrigger>,
    ) {
        let trigger = triggers.next().await.unwrap();
        let mut pages = backend
            .fetch_pages(&scanner_id(), &trigger.id)
            .await
            .unwrap();
        pages.next().await.unwrap().unwrap();
        assert!(pages.next().await.is_none());
    }

    #[tokio::test]
    async fn an_upload_emits_a_push_trigger_and_fetches_its_page() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();

        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();
        let upload = Upload {
            v: PROTOCOL_VERSION,
            device_id: DEVICE_ID.to_owned(),
            token: "phone-token".to_owned(),
            profile: ProfileKind::Image,
            page: 1,
            of: 1,
            format: "jpeg".to_owned(),
        };
        let page = vec![0xff, 0xd8, 0xff, 0xd9];
        let send = tokio::spawn(upload_once(backend.upload_port(), upload, page.clone()));

        let trigger = triggers.next().await.unwrap();
        assert_eq!(trigger.scanner_id, scanner_id());
        assert_eq!(
            trigger.kind,
            scanbus_core::TriggerKind::Push {
                profile: ProfileKind::Image
            }
        );

        let mut pages = backend
            .fetch_pages(&scanner_id(), &trigger.id)
            .await
            .unwrap();
        let received = pages.next().await.unwrap().unwrap();
        assert_eq!(received.index, 0);
        assert_eq!(received.format, scanbus_core::PageFormat::Jpeg);
        assert_eq!(received.resolution_dpi, DEFAULT_UPLOAD_RESOLUTION_DPI);
        assert_eq!(received.data, page);
        assert!(pages.next().await.is_none());

        let ack = send.await.unwrap();
        assert_eq!(ack.status, AckStatus::Ok);
        assert_eq!(ack.reason, None);
    }

    #[tokio::test]
    async fn an_upload_with_a_bad_token_is_unauthorized() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();
        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();

        let ack = upload_once(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "wrong-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        )
        .await;

        assert_eq!(ack.status, AckStatus::Error);
        assert_eq!(ack.reason, Some(AckReason::Unauthorized));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), triggers.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_upload_without_an_active_subscription_is_not_connected() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();

        let ack = upload_once(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        )
        .await;

        assert_eq!(ack.status, AckStatus::Error);
        assert_eq!(ack.reason, Some(AckReason::NotConnected));
    }

    /// The other half of §11.2, and the one an upload proves: the certificate the phone
    /// pinned during pairing is the certificate the listener presents. A second identity
    /// minted for the server role passes every other test in this file and fails here.
    #[tokio::test]
    async fn an_upload_over_tls_presents_the_certificate_the_pairing_pinned() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Serves(rustls::ALL_VERSIONS),
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);
        pair_with(&backend).await.0.unwrap();
        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();

        let send = tokio::spawn(upload_once_tls(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        ));
        fetch_one_page(&backend, &mut triggers).await;

        let upload = send.await.unwrap();
        assert_eq!(upload.ack.status, AckStatus::Ok);
        assert_eq!(
            upload.server_certificate,
            phone
                .pinned_certificate()
                .expect("the pairing pinned a certificate")
        );
        // §11.4: no client certificate is requested. The app has none to send, and a
        // `CertificateRequest` here would only produce handshake failures to explain.
        assert!(!upload.asked_for_a_client_certificate);
    }

    /// §11.4's asymmetric rule, and the acceptance case with no counterpart on the app
    /// side: the token is the right one, and it is refused because it arrived in clear.
    #[tokio::test]
    async fn a_cleartext_upload_from_a_phone_paired_over_tls_is_unauthorized() {
        let phone = FakePhone::listen_with_tls(
            Answer::Accept {
                device_id: DEVICE_ID,
            },
            PhoneTls::Serves(rustls::ALL_VERSIONS),
        )
        .await;
        let (_tmp, backend) = backend_that_saw_phone(&phone);
        pair_with(&backend).await.0.unwrap();
        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();

        let ack = upload_once(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        )
        .await;

        assert_eq!(ack.status, AckStatus::Error);
        // The reason the app already knows how to repair: discard the pairing, pair again.
        assert_eq!(ack.reason, Some(AckReason::Unauthorized));
        // And no job was started off it, which is the thing a replay would be after.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), triggers.next())
                .await
                .is_err()
        );
    }

    /// The converse, which §11.4 allows: the token still authenticates the phone, there is
    /// no pin to check, and the host does not invent one from whatever answered.
    #[tokio::test]
    async fn a_phone_paired_in_cleartext_may_upload_over_tls() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();
        assert!(!backend.lock_paired()[&scanner_id()].tls);
        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();

        let send = tokio::spawn(upload_once_tls(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        ));
        fetch_one_page(&backend, &mut triggers).await;

        assert_eq!(send.await.unwrap().ack.status, AckStatus::Ok);
        // Still a cleartext pairing: nothing about an encrypted upload upgrades the flag,
        // because there was never a fingerprint for it to mean anything against (§11.6).
        assert!(!backend.lock_paired()[&scanner_id()].tls);
    }

    /// The third row of §11.4's table. Nothing is sent back — whatever is at the other end
    /// is not speaking this protocol under either reading, so there is nobody to ack to.
    #[tokio::test]
    async fn a_first_byte_that_is_neither_tls_nor_a_frame_is_closed_without_an_ack() {
        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp);

        let mut socket = TcpStream::connect(("127.0.0.1", backend.upload_port()))
            .await
            .unwrap();
        socket.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();

        let mut answer = Vec::new();
        let outcome = tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut answer))
            .await
            .expect("the host closed the connection instead of holding it open");

        match outcome {
            Ok(0) => {}
            // The host drops the socket with the bytes it peeked at still unread, and
            // Linux answers that with an RST rather than a FIN. Both are the connection
            // being closed; what matters is that nothing came back through it.
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("expected the connection to be closed, got {other:?}"),
        }
        assert!(answer.is_empty(), "the host answered {answer:?}");
    }

    /// §11.5's switch, upload half, and the case it exists for. This phone paired in
    /// cleartext, legitimately, and §11.6 promises that pairing keeps working — but the
    /// promise is to the *default* host. An operator who sets the switch is saying nothing
    /// unencrypted crosses this LAN, and a phone paired before they said it is precisely
    /// who they mean; the device table's answer is overridden rather than consulted.
    ///
    /// The refusal is still an ack rather than a dropped socket: `unauthorized` is what
    /// the app knows how to repair, and re-pairing over TLS is the repair.
    #[tokio::test]
    async fn require_tls_refuses_a_cleartext_upload_from_a_phone_paired_before_the_switch() {
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let (_tmp, backend) = backend_that_saw(&phone.address);
        pair_with(&backend).await.0.unwrap();
        assert!(
            !backend.lock_paired()[&scanner_id()].tls,
            "a cleartext pairing is what makes this the interesting case"
        );

        // Thrown after the pairing, exactly as a restart with the variable set would be.
        let backend = backend.with_require_tls(true);
        let mut triggers = backend.start_listening(&scanner_info()).await.unwrap();

        let ack = upload_once(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        )
        .await;

        assert_eq!(ack.status, AckStatus::Error);
        assert_eq!(ack.reason, Some(AckReason::Unauthorized));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), triggers.next())
                .await
                .is_err(),
            "no job may start off a refused upload"
        );

        // The same phone, the same token, over TLS: served. The switch refuses a
        // transport, not a pairing — a phone that gets an app update and starts dialling
        // TLS needs no attention from anybody.
        let send = tokio::spawn(upload_once_tls(
            backend.upload_port(),
            Upload {
                v: PROTOCOL_VERSION,
                device_id: DEVICE_ID.to_owned(),
                token: "phone-token".to_owned(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_owned(),
            },
            vec![0xff, 0xd8, 0xff, 0xd9],
        ));
        fetch_one_page(&backend, &mut triggers).await;
        assert_eq!(send.await.unwrap().ack.status, AckStatus::Ok);
    }

    /// Default `false`, and the two spellings §11.5's key is read in. A typo leaves the
    /// switch off, which is the whole reason it is also logged at `warn`.
    #[test]
    fn require_tls_is_off_unless_the_variable_says_otherwise() {
        use std::ffi::OsStr;

        assert!(!require_tls_setting(None), "the default is off (§11.5)");
        for on in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert!(require_tls_setting(Some(OsStr::new(on))), "{on}");
        }
        for off in ["", "0", "false", "no", "off", "ture", "enabled"] {
            assert!(!require_tls_setting(Some(OsStr::new(off))), "{off}");
        }
    }

    #[test]
    fn upload_port_is_chosen_once_and_persisted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");

        let first = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        let first_port = first.upload_port();
        assert!(first_port > 0);

        drop(first);

        let second = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(second.upload_port(), first_port);
    }

    /// §12.2, and the reason this issue exists at all: the phone stores the `host_id` it
    /// saw while pairing and later browses for *that* id. A host that redraws it — which
    /// is what `MobileBackend::new` did before — is invisible to every phone paired
    /// before its last restart, so the id has to be minted once and written down.
    #[test]
    fn the_host_id_is_minted_once_written_down_and_never_redrawn() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");

        let first = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        let minted = first.host_id.clone();
        assert_eq!(minted.len(), 32, "§12.1's TXT id is 32 hex characters");
        assert!(
            minted
                .chars()
                .all(|digit| matches!(digit, '0'..='9' | 'a'..='f')),
            "lowercase hex only, or the phone's comparison fails on formatting: {minted}"
        );
        assert_eq!(
            load_store(&path).unwrap().host_id,
            minted,
            "an id that only lives in the process is the bug this fixes"
        );
        drop(first);

        let second = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(
            second.host_id, minted,
            "a restart must reuse the id the phones stored"
        );
        assert_eq!(load_store(&path).unwrap().host_id, minted);
    }

    /// The store written before §12 existed has no `host_id` key at all. Absent means
    /// *mint one and write it*, exactly as a fresh store does — no `DEVICE_STORE_VERSION`
    /// bump, because unlike §11.5's `tls` flag the default here makes no claim about a
    /// past pairing — and the pairings in it are untouched by the upgrade.
    #[test]
    fn a_store_written_before_host_ids_gains_one_and_keeps_its_pairing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        ensure_store_dir(tmp.path()).unwrap();
        let token_sha256 = hash_token("phone-token");
        fs::write(
            &path,
            format!(
                r#"{{
                  "version": {DEVICE_STORE_VERSION},
                  "upload_port": 0,
                  "devices": {{
                    "{scanner}": {{
                      "device_id": "{DEVICE_ID}",
                      "token_sha256": "{token_sha256}",
                      "profiles": ["image"],
                      "paired_at": 1700000000,
                      "tls": false
                    }}
                  }}
                }}"#,
                scanner = scanner_id()
            ),
        )
        .unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(
            backend.host_id.len(),
            32,
            "an absent id is minted, not empty"
        );
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));

        let store = load_store(&path).unwrap();
        assert_eq!(store.host_id, backend.host_id, "and written back at once");
        assert_eq!(
            store.version, DEVICE_STORE_VERSION,
            "gaining an id is not a version bump"
        );
        assert!(store.devices.contains_key(&scanner_id()));
    }

    /// And the other direction: a store that already carries an id is left alone. This is
    /// the case every restart after the first takes, so getting it wrong unpairs the host
    /// silently rather than loudly.
    #[test]
    fn a_store_that_already_has_a_host_id_keeps_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        persist_device_store(
            &path,
            &PersistedDeviceStore {
                version: DEVICE_STORE_VERSION,
                upload_port: 0,
                host_id: HOST_ID.to_owned(),
                devices: BTreeMap::new(),
            },
        )
        .unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(backend.host_id, HOST_ID);
        assert_eq!(load_store(&path).unwrap().host_id, HOST_ID);
    }

    /// §12.1 in one assertion: the record follows the *bind*, not the configuration.
    /// Binding port 0 is what makes the two numbers differ — the configured port is 0,
    /// which no phone could ever dial, so a record carrying it could only have come from
    /// the wrong side of that rule.
    #[test]
    fn the_host_record_names_the_bound_port_and_exactly_two_txt_keys() {
        let listener = bind_listener(0);
        assert!(
            listener.is_bound(),
            "an ephemeral port must be bindable: {:?}",
            listener.bind_error
        );

        let record = HostRecord::new(HOST_ID, "workshop", &listener)
            .expect("a bound listener is advertisable");

        assert_eq!(record.instance, "workshop", "the instance is host_name()");
        assert_eq!(record.port, listener.port);
        assert_ne!(
            record.port, 0,
            "the configured port never reaches the record"
        );
        assert_eq!(
            record.txt,
            [("id", HOST_ID), ("v", "1")],
            "TXT is exactly id and v, in that order"
        );
    }

    /// The degraded case, and the one where publishing would be worse than not: a phone
    /// that resolves this record and is then refused by the port it names sees the same
    /// *could not reach your computer* §12 exists to end, only now after a working
    /// lookup. So a host with no listener advertises nothing.
    #[test]
    fn a_listener_that_never_bound_is_not_advertised() {
        let blocker = std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, 0))
            .or_else(|_| std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)))
            .unwrap();
        let listener = bind_listener(blocker.local_addr().unwrap().port());
        assert!(!listener.is_bound());

        assert!(HostRecord::new(HOST_ID, "workshop", &listener).is_none());

        drop(blocker);
    }

    #[tokio::test]
    async fn device_store_hashes_the_token_and_uses_private_permissions() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0)
            .with_pairing_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        backend.remember(vec![(
            scanner_id(),
            DiscoveryRecord {
                device_id: DEVICE_ID.to_owned(),
                address: phone.address,
                tls_address: None,
                seen_at: Instant::now(),
            },
        )]);

        pair_with(&backend).await.0.unwrap();

        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("phone-token"));
        assert!(text.contains(&hash_token("phone-token")));

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn restore_disposition_is_failed_when_the_backend_store_lost_the_secret() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);

        assert_eq!(
            backend.restore_disposition(&scanner_info()).await,
            RestoreDisposition::Failed(
                "the pairing secret is missing; pair the phone again".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn prune_unrestored_pairings_drops_tokens_the_daemon_store_no_longer_names() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        let phone = FakePhone::listen(Answer::Accept {
            device_id: DEVICE_ID,
        })
        .await;
        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0)
            .with_pairing_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        backend.remember(vec![(
            scanner_id(),
            DiscoveryRecord {
                device_id: DEVICE_ID.to_owned(),
                address: phone.address,
                tls_address: None,
                seen_at: Instant::now(),
            },
        )]);
        pair_with(&backend).await.0.unwrap();
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));

        backend.prune_unrestored_pairings(&[]).await.unwrap();

        assert!(!backend.is_authorized(DEVICE_ID, "phone-token"));
        let store = load_store(&path).unwrap();
        assert!(store.devices.is_empty());
    }

    #[tokio::test]
    async fn an_occupied_persisted_port_leaves_paired_phones_offline_and_the_port_unchanged() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        let blocker = std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, 0))
            .or_else(|_| std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)))
            .unwrap();
        let port = blocker.local_addr().unwrap().port();
        persist_device_store(
            &path,
            &PersistedDeviceStore {
                version: DEVICE_STORE_VERSION,
                upload_port: port,
                host_id: String::new(),
                devices: BTreeMap::from([(
                    scanner_id(),
                    PersistedDevice {
                        device_id: DEVICE_ID.to_owned(),
                        token_sha256: hash_token("phone-token"),
                        profiles: vec![ProfileKind::Image],
                        paired_at: unix_timestamp_now(),
                        tls: false,
                    },
                )]),
            },
        )
        .unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(backend.upload_port(), port);
        assert!(!backend.listener_is_bound());
        assert!(
            backend
                .listener_error()
                .unwrap()
                .contains(&format!("port {port}"))
        );
        assert_eq!(backend.status_for(&scanner_id()), Status::Offline);

        let error = match backend.start_listening(&scanner_info()).await {
            Ok(_) => panic!(
                "a paired mobile scanner must refuse Connect() when the shared listener is down"
            ),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("mobile uploads are unavailable"),
            "unexpected error: {error}"
        );

        let store = load_store(&path).unwrap();
        assert_eq!(store.upload_port, port);

        drop(blocker);
    }

    /// §11.5's other startup failure, and the one with a wrong answer available: a key
    /// that cannot be read must leave the phones offline and the file alone. The
    /// tempting repair — treat it as a first start, mint a new key — unpairs every phone
    /// on the host at once and cannot be undone from either side.
    #[tokio::test]
    async fn an_unreadable_tls_key_leaves_paired_phones_offline_and_the_key_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        persist_device_store(
            &path,
            &PersistedDeviceStore {
                version: DEVICE_STORE_VERSION,
                upload_port: 0,
                host_id: String::new(),
                devices: BTreeMap::from([(
                    scanner_id(),
                    PersistedDevice {
                        device_id: DEVICE_ID.to_owned(),
                        token_sha256: hash_token("phone-token"),
                        profiles: vec![ProfileKind::Image],
                        paired_at: unix_timestamp_now(),
                        tls: false,
                    },
                )]),
            },
        )
        .unwrap();

        // A host that has run once, so there is a real pair on disk to damage — the
        // interesting case is not a missing identity but a broken one.
        let fingerprint = tls::HostIdentity::load_or_generate(tmp.path())
            .unwrap()
            .fingerprint_sha256()
            .to_owned();
        let key_path = tmp.path().join(tls::KEY_FILE);
        let good_key = fs::read(&key_path).unwrap();
        let damaged = b"-----BEGIN PRIVATE KEY-----\nnot base64\n".to_vec();
        fs::write(&key_path, &damaged).unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);

        // The listener is fine. Nothing about this failure is the port's fault, and a
        // test that let the two run together could not tell which one it was observing.
        assert!(backend.listener_is_bound());
        assert!(backend.listener_error().is_none());
        assert!(backend.identity().is_none());
        assert!(
            backend.identity_error().unwrap().contains(tls::KEY_FILE),
            "the reason has to name the file an operator must fix: {:?}",
            backend.identity_error()
        );
        assert_eq!(backend.status_for(&scanner_id()), Status::Offline);

        // The point of the whole test.
        assert_eq!(fs::read(&key_path).unwrap(), damaged);
        assert_eq!(
            tls::HostIdentity::load_or_generate(tmp.path())
                .unwrap_err()
                .to_string(),
            backend.identity_error().unwrap(),
            "a second start must fail the same way rather than having repaired anything"
        );

        let error = match backend.start_listening(&scanner_info()).await {
            Ok(_) => {
                panic!("a paired mobile scanner must refuse Connect() when the host has no key")
            }
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("mobile uploads are unavailable"),
            "unexpected error: {error}"
        );

        // Pairing refuses too, rather than silently making a cleartext pairing the
        // operator would only discover from the app's warning (§11.6).
        let (outcome, _) = pair_with(&backend).await;
        let error = outcome.expect_err("pairing must refuse while the host has no identity");
        assert!(
            error.to_string().contains("mobile pairing is unavailable"),
            "unexpected error: {error}"
        );

        // Why refusing was worth it: put the key back and the host is the same host the
        // phones pinned. A regeneration would have made this restore impossible.
        fs::write(&key_path, &good_key).unwrap();
        assert_eq!(
            tls::HostIdentity::load_or_generate(tmp.path())
                .unwrap()
                .fingerprint_sha256(),
            fingerprint
        );
    }

    /// §12.2's whole point: the id a phone stores while pairing and looks for after a
    /// lease change has to outlive the process that drew it. A store with no `host_id`
    /// is both the pre-§12 store and the fresh one, so both mint — and neither mints
    /// again, because a second value would be invisible to every phone paired against
    /// the first.
    #[tokio::test]
    async fn a_store_without_a_host_id_mints_one_and_then_keeps_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        let minted = load_store(&path).unwrap().host_id;
        assert_eq!(
            backend.host_id, minted,
            "the id written to disk is the one the handshake announces"
        );
        assert_eq!(minted.len(), 32, "a 128-bit id in hex: {minted}");
        assert!(
            minted.chars().all(|c| c.is_ascii_hexdigit()),
            "the app compares TXT id byte for byte, so the formatting is part of the \
             contract: {minted}"
        );

        let restarted = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert_eq!(
            load_store(&path).unwrap().host_id,
            minted,
            "a restart must not redraw the id every paired phone is looking for"
        );
        assert_eq!(restarted.host_id, minted);
    }

    /// The other half: an id already on disk is what the phones hold, so startup has no
    /// business touching it — not even to normalize it — and it is that id, not a fresh
    /// one, that the backend puts in `pair_request.host_id`.
    #[tokio::test]
    async fn a_store_that_already_has_a_host_id_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        persist_device_store(
            &path,
            &PersistedDeviceStore {
                version: DEVICE_STORE_VERSION,
                upload_port: 0,
                host_id: "0123456789abcdef0123456789abcdef".to_owned(),
                devices: BTreeMap::new(),
            },
        )
        .unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);

        assert_eq!(
            load_store(&path).unwrap().host_id,
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            backend.host_id, "0123456789abcdef0123456789abcdef",
            "the handshake has to announce the persisted id, not a per-process one"
        );
    }

    /// The upgrade §11.6 depends on: a store written before TLS existed keeps its
    /// pairings, and they keep working. The version bump makes the wrong answer available
    /// — a version check that only accepts its own number would send this file through
    /// `rename_store_aside` and unpair the host on a package upgrade — so the absent flag
    /// has to read as `false` rather than as an unreadable store.
    #[tokio::test]
    async fn a_version_1_store_upgrades_in_place_and_its_pairings_are_cleartext() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        ensure_store_dir(tmp.path()).unwrap();
        let token_sha256 = hash_token("phone-token");
        fs::write(
            &path,
            format!(
                r#"{{
                  "version": 1,
                  "upload_port": 0,
                  "devices": {{
                    "{scanner}": {{
                      "device_id": "{DEVICE_ID}",
                      "token_sha256": "{token_sha256}",
                      "profiles": ["image"],
                      "paired_at": 1700000000
                    }}
                  }}
                }}"#,
                scanner = scanner_id()
            ),
        )
        .unwrap();

        let store = load_store(&path).unwrap();
        assert_eq!(
            store.version, DEVICE_STORE_VERSION,
            "an old store is upgraded on read, not refused"
        );
        let device = &store.devices[&scanner_id()];
        assert!(
            !device.tls,
            "a pairing made before the host had a certificate cannot have been pinned"
        );

        // And the same thing through a real start: the phone is still paired, still
        // authorized by the token it was issued, and still online.
        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert!(backend.is_authorized(DEVICE_ID, "phone-token"));
        assert_eq!(backend.status_for(&scanner_id()), Status::Online);
        assert!(!backend.lock_paired()[&scanner_id()].tls);

        let paired = backend.lock_paired().clone();
        backend.persist_paired_locked(&paired).unwrap();
        let rewritten = fs::read_to_string(&path).unwrap();
        assert!(
            rewritten.contains("\"version\": 2"),
            "the next write stamps the new version: {rewritten}"
        );
        assert!(
            rewritten.contains("\"tls\": false"),
            "the flag is written out explicitly once known: {rewritten}"
        );
        assert!(rewritten.contains(&token_sha256), "the pairing survived");
    }

    /// The one direction that is not an upgrade. A store this build cannot understand was
    /// written by a newer scanbus, and reading it with defaults would silently discard
    /// whatever that version recorded — a `tls` flag among it, if a downgrade is what got
    /// us here. Refusing is the same path a corrupt store takes: renamed aside, loudly.
    #[test]
    fn a_store_from_a_newer_scanbus_is_refused_rather_than_read_with_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        ensure_store_dir(tmp.path()).unwrap();
        fs::write(
            &path,
            format!(
                r#"{{"version": {}, "upload_port": 0, "devices": {{}}}}"#,
                DEVICE_STORE_VERSION + 1
            ),
        )
        .unwrap();

        let error = load_store(&path).unwrap_err();
        assert!(error.contains("newer than this scanbus"), "{error}");

        assert!(load_or_reset_store(&path).devices.is_empty());
        assert!(!path.exists(), "the unreadable store is renamed aside");
    }

    /// The flag is only worth having if it survives a restart: it is the sole record that
    /// a phone pinned us, and §11.4 answers `unauthorized` on the strength of it.
    #[tokio::test]
    async fn the_tls_flag_survives_a_restart() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("devices.json");
        persist_device_store(
            &path,
            &PersistedDeviceStore {
                version: DEVICE_STORE_VERSION,
                upload_port: 0,
                host_id: String::new(),
                devices: BTreeMap::from([(
                    scanner_id(),
                    PersistedDevice {
                        device_id: DEVICE_ID.to_owned(),
                        token_sha256: hash_token("phone-token"),
                        profiles: vec![ProfileKind::Image],
                        paired_at: unix_timestamp_now(),
                        tls: true,
                    },
                )]),
            },
        )
        .unwrap();

        let backend = MobileBackend::with_store_path(path.clone(), DISCOVERY_TIMEOUT, 0);
        assert!(backend.lock_paired()[&scanner_id()].tls);

        // Round-tripped rather than only read: the rewrite is where a flag that the
        // in-memory record dropped would disappear for good.
        let paired = backend.lock_paired().clone();
        backend.persist_paired_locked(&paired).unwrap();
        assert!(load_store(&path).unwrap().devices[&scanner_id()].tls);
    }

    /// One sighting of a phone, with whatever TXT it advertised.
    fn service_with(ip: &str, txt: &[(&str, &str)]) -> mdns_sd::ServiceInfo {
        mdns_sd::ServiceInfo::new(SERVICE_TYPE, "phone", "phone.local.", ip, 45481, txt).unwrap()
    }

    #[test]
    fn a_tlsport_txt_key_is_read_as_the_port_to_dial_for_tls() {
        let service = service_with(
            "192.168.1.5",
            &[("v", "1"), ("id", DEVICE_ID), ("tlsport", "8443")],
        );

        let (scanner, record) = scanner_from_service(&service).unwrap();

        // The SRV port is untouched: §11.3 keeps it cleartext forever, because it is the
        // only port a host built before §11 knows how to find.
        assert_eq!(record.address, "192.168.1.5:45481");
        assert_eq!(scanner.address, "192.168.1.5:45481");
        assert_eq!(record.tls_address.as_deref(), Some("192.168.1.5:8443"));
    }

    /// The formatting `address` is careful about, applied to the second port too: a
    /// hand-rolled `"{ip}:{port}"` would make `fe80::1:8443` out of an IPv6 phone, which
    /// the socket layer rejects before any network attempt happens.
    #[test]
    fn a_tlsport_on_an_ipv6_phone_is_bracketed_like_the_srv_address() {
        let service = service_with(
            "fe80::1",
            &[("v", "1"), ("id", DEVICE_ID), ("tlsport", "8443")],
        );

        let (_, record) = scanner_from_service(&service).unwrap();

        assert_eq!(record.address, "[fe80::1]:45481");
        assert_eq!(record.tls_address.as_deref(), Some("[fe80::1]:8443"));
        assert!(record.tls_address.unwrap().parse::<SocketAddr>().is_ok());
    }

    /// No `tlsport` is a phone with no keystore identity, and nothing else about the
    /// record changes — a phone advertising what it always advertised still pairs.
    #[test]
    fn a_record_without_tlsport_is_a_phone_with_no_tls() {
        let service = service_with("192.168.1.5", &[("v", "1"), ("id", DEVICE_ID)]);

        let (scanner, record) = scanner_from_service(&service).unwrap();

        assert_eq!(scanner.id, scanner_id());
        assert_eq!(record.device_id, DEVICE_ID);
        assert_eq!(record.address, "192.168.1.5:45481");
        assert_eq!(record.tls_address, None);
    }

    /// A value that is not a port a phone can be serving on is treated as absent, not as
    /// a reason to hide the phone: an attacker who can forge this record can strip the
    /// key outright (§11.6), so dropping the service here would deny someone a working
    /// cleartext pairing without denying an attacker anything.
    #[test]
    fn an_unusable_tlsport_reads_as_no_tls_rather_than_dropping_the_phone() {
        for value in ["0", "not-a-port", "70000", "-1", "8443 8444", ""] {
            let service = service_with(
                "192.168.1.5",
                &[("v", "1"), ("id", DEVICE_ID), ("tlsport", value)],
            );

            let (scanner, record) = scanner_from_service(&service)
                .unwrap_or_else(|| panic!("tlsport={value:?} dropped the whole service"));

            assert_eq!(scanner.address, "192.168.1.5:45481");
            assert_eq!(record.tls_address, None, "tlsport={value:?}");
        }
    }

    /// The nonce is the whole security value of the comparison: six digits, every one of
    /// them drawn, leading zeros included.
    #[test]
    fn nonces_cover_the_whole_six_digit_range() {
        let mut leading = [0_u32; 10];
        let mut distinct = std::collections::HashSet::new();

        for _ in 0..1000 {
            let nonce = generate_nonce();
            assert_eq!(nonce.len(), 6, "{nonce:?} is not six digits");
            assert!(
                nonce.bytes().all(|b| b.is_ascii_digit()),
                "{nonce:?} is not all digits"
            );
            leading[(nonce.as_bytes()[0] - b'0') as usize] += 1;
            distinct.insert(nonce);
        }

        // Every leading digit including `0`, which is what the zero-padding is for: a
        // generator that formatted the number without it, or drew from `100000..`, would
        // leave this bucket empty. With 1000 draws each bucket expects 100, and the
        // chance of any one coming up empty by luck is about 10^-46.
        for (digit, count) in leading.iter().enumerate() {
            assert!(*count > 0, "no nonce ever started with {digit}");
        }
        // A generator stuck on a handful of values would still pass the checks above.
        assert!(
            distinct.len() > 900,
            "only {} distinct nonces in 1000 draws",
            distinct.len()
        );
    }

    #[test]
    fn ipv6_discovery_addresses_keep_the_brackets_tcp_connect_needs() {
        let address =
            SocketAddr::new("fe80::78a0:89ff:fe33:e293".parse().unwrap(), 45481).to_string();
        assert_eq!(address, "[fe80::78a0:89ff:fe33:e293]:45481");
    }

    #[test]
    fn link_local_candidates_apply_scope_ids_to_the_target_ip() {
        let target: Ipv6Addr = "fe80::78a0:89ff:fe33:e293".parse().unwrap();
        let candidates = link_local_candidates(target, 45481).unwrap();
        for candidate in candidates {
            assert_eq!(candidate.ip(), &target);
            assert_eq!(candidate.port(), 45481);
            assert_ne!(candidate.scope_id(), 0);
        }
    }

    #[test]
    fn secrets_are_redacted_from_the_debug_output() {
        let request = PairRequest {
            v: 1,
            host_id: "host".to_owned(),
            host_name: "desktop".to_owned(),
            upload_port: 4242,
            nonce: "482913".to_owned(),
        };
        let response = PairResponse {
            accepted: true,
            device_id: DEVICE_ID.to_owned(),
            capabilities: Capabilities::default(),
            token: "phone-token".to_owned(),
        };
        let upload = Upload {
            v: 1,
            device_id: DEVICE_ID.to_owned(),
            token: "phone-token".to_owned(),
            profile: ProfileKind::Image,
            page: 1,
            of: 1,
            format: "jpeg".to_owned(),
        };

        assert!(!format!("{request:?}").contains("482913"));
        assert!(!format!("{response:?}").contains("phone-token"));
        assert!(!format!("{upload:?}").contains("phone-token"));
    }

    #[tokio::test]
    async fn unknown_message_type_is_refused_as_unsupported() {
        let bytes = br#"{"type":"nonsense"}"#;
        let err = parse_control_message(bytes).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::Unsupported {
                message_type: "nonsense".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn upload_version_is_checked_after_decode() {
        let bytes = br#"{"type":"upload","v":2,"device_id":"d","token":"t","profile":"image","page":1,"of":1,"format":"jpeg"}"#;
        let message = parse_control_message(bytes).unwrap();
        let err = message.validate_version().unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnsupportedVersion {
                seen: 2,
                supported: PROTOCOL_VERSION,
            }
        );
    }

    #[tokio::test]
    async fn too_large_length_is_refused_before_payload_read() {
        struct PrefixOnlyReader {
            prefix: [u8; 4],
            pos: usize,
            panic_on_payload_read: bool,
        }

        impl AsyncRead for PrefixOnlyReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<Result<(), io::Error>> {
                if self.pos < 4 {
                    let remaining = 4 - self.pos;
                    let to_copy = remaining.min(buf.remaining());
                    buf.put_slice(&self.prefix[self.pos..self.pos + to_copy]);
                    self.pos += to_copy;
                    return Poll::Ready(Ok(()));
                }
                if self.panic_on_payload_read {
                    panic!("reader was asked for payload bytes");
                }
                Poll::Ready(Ok(()))
            }
        }

        let mut reader = PrefixOnlyReader {
            prefix: u32::MAX.to_be_bytes(),
            pos: 0,
            panic_on_payload_read: true,
        };

        let err = read_frame(&mut reader, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ProtocolError::TooLarge {
                kind: FrameKind::Control,
                len: u32::MAX,
                max: DEFAULT_CONTROL_MAX_BYTES,
            }
        );
    }

    #[tokio::test]
    async fn truncated_frame_is_malformed() {
        let mut reader = PrefixAndPartialPayload {
            prefix: 100_u32.to_be_bytes(),
            prefix_pos: 0,
            payload: vec![1_u8; 40],
            payload_pos: 0,
        };

        let err = read_frame(&mut reader, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));
    }

    struct PrefixAndPartialPayload {
        prefix: [u8; 4],
        prefix_pos: usize,
        payload: Vec<u8>,
        payload_pos: usize,
    }

    impl AsyncRead for PrefixAndPartialPayload {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<(), io::Error>> {
            if self.prefix_pos < 4 {
                let remaining = 4 - self.prefix_pos;
                let to_copy = remaining.min(buf.remaining());
                buf.put_slice(&self.prefix[self.prefix_pos..self.prefix_pos + to_copy]);
                self.prefix_pos += to_copy;
                return Poll::Ready(Ok(()));
            }

            if self.payload_pos >= self.payload.len() {
                return Poll::Ready(Ok(()));
            }

            let remaining = self.payload.len() - self.payload_pos;
            let to_copy = remaining.min(buf.remaining());
            buf.put_slice(&self.payload[self.payload_pos..self.payload_pos + to_copy]);
            self.payload_pos += to_copy;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn pair_response_unknown_profile_is_skipped() {
        let bytes = br#"{"type":"pair_response","accepted":true,"device_id":"d","token":"t","capabilities":{"profiles":["image","document","hologram"]}}"#;
        let message = parse_control_message(bytes).unwrap();
        let Message::PairResponse(response) = message else {
            panic!("expected pair_response");
        };
        assert_eq!(
            response.capabilities.profiles,
            vec![ProfileKind::Image, ProfileKind::Document]
        );
    }

    #[tokio::test]
    async fn round_trip_messages() {
        let messages = vec![
            Message::PairRequest(PairRequest {
                v: 1,
                host_id: "host-id".to_string(),
                host_name: "host".to_string(),
                upload_port: 4242,
                nonce: "000123".to_string(),
            }),
            Message::PairResponse(PairResponse {
                accepted: true,
                device_id: "dev".to_string(),
                capabilities: Capabilities {
                    profiles: vec![ProfileKind::Image, ProfileKind::Document],
                },
                token: "secret".to_string(),
            }),
            Message::Upload(Upload {
                v: 1,
                device_id: "dev".to_string(),
                token: "secret".to_string(),
                profile: ProfileKind::Image,
                page: 1,
                of: 1,
                format: "jpeg".to_string(),
            }),
            Message::Ack(Ack {
                status: AckStatus::Error,
                reason: Some(AckReason::UnsupportedVersion),
            }),
        ];

        for original in messages {
            let bytes = serialize_control_message(&original).unwrap();
            let decoded = parse_control_message(&bytes).unwrap();
            assert_eq!(decoded, original);
        }
    }

    #[tokio::test]
    async fn round_trip_64_mib_page_frame() {
        let payload = vec![0x7f; DEFAULT_PAGE_MAX_BYTES];
        let (mut left, mut right) = duplex(DEFAULT_PAGE_MAX_BYTES + 1024);

        let write = write_frame(&mut left, FrameKind::Page, &payload, DEFAULT_PAGE_MAX_BYTES);
        let read = read_frame(&mut right, FrameKind::Page, DEFAULT_PAGE_MAX_BYTES);
        let (write_result, read_result) = tokio::join!(write, read);

        write_result.unwrap();
        let decoded = read_result.unwrap();
        assert_eq!(decoded.len(), DEFAULT_PAGE_MAX_BYTES);
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn zero_length_frame_is_malformed() {
        let mut reader = PrefixOnlyReaderNoPayload {
            prefix: 0_u32.to_be_bytes(),
            pos: 0,
        };

        let err = read_frame(&mut reader, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));
    }

    struct PrefixOnlyReaderNoPayload {
        prefix: [u8; 4],
        pos: usize,
    }

    impl AsyncRead for PrefixOnlyReaderNoPayload {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<(), io::Error>> {
            if self.pos < 4 {
                let remaining = 4 - self.pos;
                let to_copy = remaining.min(buf.remaining());
                buf.put_slice(&self.prefix[self.pos..self.pos + to_copy]);
                self.pos += to_copy;
            }
            Poll::Ready(Ok(()))
        }
    }
}
