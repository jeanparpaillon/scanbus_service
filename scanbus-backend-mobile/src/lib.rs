//! Wire protocol primitives and backend plumbing for `scanbus-backend-mobile`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use flume::RecvTimeoutError;
use futures_core::stream::BoxStream;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use rand::Rng;
use scanbus_core::{
	BackendError, ButtonsCapability, Capabilities as ScannerCapabilities, ProfileKind, RawPage,
	ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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
	/// The phone's upload credential. Never logged and never rendered by [`fmt::Debug`]
	/// — see [`PairedDevice`]'s manual impl.
	token: String,
	profiles: Vec<ProfileKind>,
}

/// Redacted on purpose: the token must not reach a log through a `?device` that seemed
/// harmless at the call site.
impl fmt::Debug for PairedDevice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PairedDevice")
			.field("device_id", &self.device_id)
			.field("token", &"<redacted>")
			.field("profiles", &self.profiles)
			.finish()
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
	/// What `pair_request.upload_port` carries: where the phone uploads from now on.
	///
	/// Zero until [`9.4`](PairedDevice) binds the shared listener and hands its port
	/// here — a phone paired against a zero is paired but has nowhere to send, which is
	/// exactly as far as this issue goes.
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
}

impl Default for MobileBackend {
	fn default() -> Self {
		Self::new(DISCOVERY_TIMEOUT)
	}
}

impl MobileBackend {
	pub fn new(discovery_timeout: Duration) -> Self {
		Self {
			discovery_timeout,
			connect_timeout: PAIR_CONNECT_TIMEOUT,
			confirm_timeout: PAIR_CONFIRM_TIMEOUT,
			upload_port: 0,
			host_id: generate_host_id(),
			host_name: host_name(),
			discovered: Arc::new(Mutex::new(BTreeMap::new())),
			paired: Arc::new(Mutex::new(BTreeMap::new())),
		}
	}

	/// The port `pair_request` advertises for uploads — [`9.4`](PairedDevice)'s listener.
	pub fn with_upload_port(mut self, upload_port: u16) -> Self {
		self.upload_port = upload_port;
		self
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
			device.device_id == device_id && constant_time_eq(&device.token, token)
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
		let record = self.live_record(&scanner.id).ok_or_else(|| BackendError::NotReachable {
			scanner: scanner.id.clone(),
			detail: "no live discovery record: pairing is the one moment the host dials, \
			         and it dials the address a phone is advertising now, never a \
			         remembered one"
				.to_owned(),
		})?;

		let mut socket = timeout(self.connect_timeout, TcpStream::connect(&record.address))
			.await
			.map_err(|_| BackendError::NotReachable {
				scanner: scanner.id.clone(),
				detail: format!(
					"{} did not accept a connection within {:?}",
					record.address, self.connect_timeout
				),
			})?
			.map_err(|error| BackendError::NotReachable {
				scanner: scanner.id.clone(),
				detail: format!("could not connect to {}: {error}", record.address),
			})?;

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

		self.lock_paired().insert(
			scanner.id.clone(),
			PairedDevice {
				device_id: response.device_id,
				token: response.token,
				profiles: response.capabilities.profiles,
			},
		);

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

	/// A phone has no buttons to listen for, so this is an empty stream rather than a
	/// refusal.
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
		_scanner: &ScannerInfo,
	) -> Result<BoxStream<'static, scanbus_core::ButtonPressedEvent>, BackendError> {
		Ok(Box::pin(futures_util::stream::empty()))
	}

	/// Nothing per-scanner is listening, so there is nothing to stop — and the trait
	/// requires the no-op case to be `Ok(())`, not an error.
	async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
		Ok(())
	}

	/// Revokes the token a pairing issued, so an upload bearing it comes back
	/// `unauthorized` — the trait method mobile-backend.md §4.4 adds for `Unpair()`.
	///
	/// The phone is not told: there is no channel to tell it on, and `Unpair()` has to
	/// work with the phone switched off. It learns at its next upload.
	async fn forget(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
		if self.lock_paired().remove(scanner_id).is_some() {
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
		_scanner_id: &ScannerId,
		_job_id: &str,
	) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
		Err(BackendError::Unsupported {
			backend: ID,
			operation: "fetch_pages",
		})
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

/// A CSPRNG-drawn host identifier for `pair_request.host_id`.
fn generate_host_id() -> String {
	use fmt::Write as _;

	let bytes: [u8; 16] = rand::rng().random();
	bytes.iter().fold(String::with_capacity(32), |mut out, byte| {
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
	let daemon = ServiceDaemon::new().map_err(|error| BackendError::NotReachable {
		scanner: ScannerId::from_backend(ID, "discovery").expect("static discovery id is valid"),
		detail: format!("failed to start mDNS browser: {error}"),
	})?;
	let receiver = daemon
		.browse(SERVICE_TYPE)
		.map_err(|error| BackendError::Other(format!("failed to browse {SERVICE_TYPE}: {error}")))?;

	let mut seen = BTreeMap::<ScannerId, (ScannerInfo, DiscoveryRecord)>::new();
	let started = Instant::now();
	loop {
		let elapsed = started.elapsed();
		if elapsed >= timeout {
			break;
		}
		let remaining = timeout - elapsed;
		match receiver.recv_timeout(remaining) {
			Ok(ServiceEvent::ServiceResolved(service)) => {
				if let Some((scanner, record)) = scanner_from_service(&service) {
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
			}
			Ok(
				ServiceEvent::SearchStarted(_)
				| ServiceEvent::ServiceFound(_, _)
				| ServiceEvent::ServiceRemoved(_, _)
				| ServiceEvent::SearchStopped(_),
			) => {}
			Err(RecvTimeoutError::Timeout) => break,
			Err(RecvTimeoutError::Disconnected) => {
				warn!("mDNS browser disconnected before timeout");
				break;
			}
		}
	}

	if let Err(error) = daemon.stop_browse(SERVICE_TYPE) {
		debug!(%error, "mobile discover stop_browse failed; continuing");
	}
	if let Err(error) = daemon.shutdown() {
		debug!(%error, "mobile discover daemon shutdown failed; continuing");
	}

	Ok(seen.into_values().collect())
}

fn scanner_from_service(
	service: &mdns_sd::ServiceInfo,
) -> Option<(ScannerInfo, DiscoveryRecord)> {
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

	let address = format!("{}:{}", ip, service.get_port());

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
				context: format!("page/of must be >= 1, got page={}, of={}", self.page, self.of),
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
	let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| ProtocolError::Malformed {
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
	let len = reader.read_u32().await.map_err(|err| ProtocolError::Malformed {
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
	writer.flush().await.map_err(|err| ProtocolError::Malformed {
		context: format!("failed to flush {} frame: {}", kind.as_str(), err),
	})?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::{io, pin::Pin, task::Poll};

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

				let payload =
					serialize_control_message(&Message::PairResponse(response)).unwrap();
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
	fn backend_that_saw(address: &str) -> MobileBackend {
		let backend = MobileBackend::default()
			.with_upload_port(4242)
			.with_pairing_timeouts(Duration::from_millis(200), Duration::from_millis(200));
		backend.remember(vec![(
			scanner_id(),
			DiscoveryRecord {
				device_id: DEVICE_ID.to_owned(),
				address: address.to_owned(),
				seen_at: Instant::now(),
			},
		)]);
		backend
	}

	/// Runs one pairing and returns its outcome along with every progress step it sent.
	async fn pair_with(
		backend: &MobileBackend,
	) -> (
		Result<(), BackendError>,
		Vec<scanbus_core::PairingProgress>,
	) {
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
		let backend = backend_that_saw(&phone.address);

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
		let backend = backend_that_saw(&phone.address);
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
		let backend = backend_that_saw(&phone.address);

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
		let backend = backend_that_saw(&phone.address);

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
		let backend = backend_that_saw(&phone.address);

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
		let backend = backend_that_saw(&phone.address)
			.with_pairing_timeouts(PAIR_CONNECT_TIMEOUT, PAIR_CONFIRM_TIMEOUT);

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
		let backend = MobileBackend::default();

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
		let backend = MobileBackend::default();
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
		let backend = backend_that_saw(&phone.address);
		pair_with(&backend).await.0.unwrap();
		assert!(backend.is_authorized(DEVICE_ID, "phone-token"));

		backend.forget(&scanner_id()).await.unwrap();
		assert!(!backend.is_authorized(DEVICE_ID, "phone-token"));

		// Idempotent: `Unpair()` on a scanner already forgotten is not an error.
		backend.forget(&scanner_id()).await.unwrap();
	}

	/// A phone has no buttons, but pairing still has to reach `Done`, so this is an
	/// empty stream and not the refusal it was before 9.3.
	#[tokio::test]
	async fn listening_to_a_phone_succeeds_and_yields_nothing() {
		use futures_util::StreamExt as _;

		let backend = MobileBackend::default();
		let mut events = backend.start_listening(&scanner_info()).await.unwrap();
		assert!(events.next().await.is_none());
		backend.stop_listening(&scanner_id()).await.unwrap();
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
