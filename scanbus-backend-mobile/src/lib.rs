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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_core::{Stream, stream::BoxStream};
use if_addrs::{IfAddr, get_if_addrs};
use rand::Rng;
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
use tracing::{debug, warn};

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

const DEVICE_STORE_VERSION: u32 = 1;

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
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDeviceStore {
    version: u32,
    #[serde(default)]
    upload_port: u16,
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
}

impl Default for PersistedDeviceStore {
    fn default() -> Self {
        Self {
            version: DEVICE_STORE_VERSION,
            upload_port: 0,
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
    /// What `pair_request.host_id` carries. Generated once per process, not persisted:
    /// nothing on the wire needs it to survive a restart, only to be stable for the
    /// duration of one handshake.
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
    listener: Arc<Mutex<ListenerBinding>>,
    listener_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
            if let Err(error) = persist_device_store(&store_path, &store) {
                warn!(path = %store_path.display(), %error, "could not persist mobile device store");
            }
        }

        let backend = Self {
            discovery_timeout,
            connect_timeout: PAIR_CONNECT_TIMEOUT,
            confirm_timeout: PAIR_CONFIRM_TIMEOUT,
            upload_port,
            host_id: generate_host_id(),
            host_name: host_name(),
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            paired: Arc::new(Mutex::new(paired)),
            store_path: Arc::new(store_path),
            identity,
            identity_error,
            listener: Arc::new(Mutex::new(listener)),
            listener_task: Arc::new(Mutex::new(None)),
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

        let paired = Arc::clone(&self.paired);
        let subscriptions = Arc::clone(&self.subscriptions);
        let pending_uploads = Arc::clone(&self.pending_uploads);
        let next_trigger_id = Arc::clone(&self.next_trigger_id);
        let task = handle.spawn(async move {
            run_upload_listener(
                listener,
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
        if !self.listener_is_bound() {
            let detail = self
                .listener_error()
                .unwrap_or_else(|| "the mobile upload listener is down".to_owned());
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

        let mut socket = self.connect_to_record(&scanner.id, &record.address).await?;

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
            detail: format!("could not send pair_request to {}: {error}", record.address),
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

        let device = PairedDevice {
            device_id: response.device_id,
            token_sha256: hash_token(&response.token),
            profiles: response.capabilities.profiles,
            paired_at: unix_timestamp_now(),
        };
        self.store_paired_device(&scanner.id, device)?;

        Ok(())
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

    fn status_for(&self, scanner: &ScannerId) -> Status {
        if self.lock_paired().contains_key(scanner) {
            if self.listener_is_bound() {
                Status::Online
            } else {
                Status::Offline
            }
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
        if self.lock_paired().contains_key(&scanner.id) && !self.listener_is_bound() {
            let detail = self
                .listener_error()
                .unwrap_or_else(|| "the mobile upload listener is down".to_owned());
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

async fn run_upload_listener(
    listener: TcpListener,
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let paired = Arc::clone(&paired);
        let subscriptions = Arc::clone(&subscriptions);
        let pending_uploads = Arc::clone(&pending_uploads);
        let next_trigger_id = Arc::clone(&next_trigger_id);
        tokio::spawn(async move {
            let _ = handle_upload_connection(
                socket,
                paired,
                subscriptions,
                pending_uploads,
                next_trigger_id,
            )
            .await;
        });
    }
}

async fn handle_upload_connection(
    mut socket: TcpStream,
    paired: Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    subscriptions: Arc<Mutex<BTreeMap<ScannerId, mpsc::Sender<scanbus_core::ScanTrigger>>>>,
    pending_uploads: Arc<Mutex<BTreeMap<String, PendingUpload>>>,
    next_trigger_id: Arc<AtomicU64>,
) -> Result<(), ProtocolError> {
    let first = match read_upload_header(&mut socket).await {
        Ok(first) => first,
        Err(error) => {
            let _ = send_error_ack(&mut socket, ack_reason_for(&error)).await;
            return Err(error);
        }
    };
    let scanner_id = match authorize_upload(&paired, &first.device_id, &first.token) {
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

async fn stream_upload_pages(
    socket: &mut TcpStream,
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

        let next = read_upload_header(socket).await?;
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

async fn read_upload_header(socket: &mut TcpStream) -> Result<Upload, ProtocolError> {
    let frame = timeout(
        UPLOAD_FRAME_TIMEOUT,
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

fn authorize_upload(
    paired: &Arc<Mutex<BTreeMap<ScannerId, PairedDevice>>>,
    device_id: &str,
    token: &str,
) -> Result<ScannerId, ProtocolError> {
    paired
        .lock()
        .expect("mobile paired device lock poisoned")
        .iter()
        .find_map(|(scanner_id, device)| {
            (device.device_id == device_id
                && constant_time_eq(&device.token_sha256, &hash_token(token)))
            .then(|| scanner_id.clone())
        })
        .ok_or_else(|| ProtocolError::Unauthorized {
            device_id: device_id.to_owned(),
        })
}

async fn send_ok_ack(socket: &mut TcpStream) -> Result<(), ProtocolError> {
    send_ack(
        socket,
        Ack {
            status: AckStatus::Ok,
            reason: None,
        },
    )
    .await
}

async fn send_error_ack(socket: &mut TcpStream, reason: AckReason) -> Result<(), ProtocolError> {
    send_ack(
        socket,
        Ack {
            status: AckStatus::Error,
            reason: Some(reason),
        },
    )
    .await
}

async fn send_ack(socket: &mut TcpStream, ack: Ack) -> Result<(), ProtocolError> {
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
    let store: PersistedDeviceStore = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "cannot parse mobile device store {}: {error}",
            path.display()
        )
    })?;

    if store.version != DEVICE_STORE_VERSION {
        return Err(format!(
            "mobile device store version {} is unsupported (expected {})",
            store.version, DEVICE_STORE_VERSION
        ));
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
        /// What the phone read out of `pair_request`, once it has.
        request: Arc<Mutex<Option<PairRequest>>>,
        /// Set when the connection reached end-of-file — what `CancelPairing()` looks
        /// like from the app's side.
        disconnected: Arc<Mutex<bool>>,
    }

    /// How the phone answers, once it has read the request.
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let request = Arc::new(Mutex::new(None));
            let disconnected = Arc::new(Mutex::new(false));

            let seen = Arc::clone(&request);
            let closed = Arc::clone(&disconnected);
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let frame = read_frame(&mut socket, FrameKind::Control, DEFAULT_CONTROL_MAX_BYTES)
                    .await
                    .unwrap();
                let Message::PairRequest(pair_request) = parse_control_message(&frame).unwrap()
                else {
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
                        // Nothing is sent, and the socket is held open: the host has to
                        // be the one that gives up. Reading is how the app notices the
                        // host closing it, which is what `CancelPairing()` does.
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
            });

            Self {
                address,
                request,
                disconnected,
            }
        }

        fn nonce_shown(&self) -> Option<String> {
            self.request
                .lock()
                .unwrap()
                .as_ref()
                .map(|request| request.nonce.clone())
        }
    }

    /// What the fake phone puts in its TXT `id`.
    const DEVICE_ID: &str = "phone_a1b2c3";

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
        let tmp = TempDir::new().unwrap();
        let backend = backend_in(&tmp)
            .with_pairing_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        backend.remember(vec![(
            scanner_id(),
            DiscoveryRecord {
                device_id: DEVICE_ID.to_owned(),
                address: address.to_owned(),
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
                devices: BTreeMap::from([(
                    scanner_id(),
                    PersistedDevice {
                        device_id: DEVICE_ID.to_owned(),
                        token_sha256: hash_token("phone-token"),
                        profiles: vec![ProfileKind::Image],
                        paired_at: unix_timestamp_now(),
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
