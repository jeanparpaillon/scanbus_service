//! Wire protocol primitives and backend plumbing for `scanbus-backend-mobile`.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use flume::RecvTimeoutError;
use futures_core::stream::BoxStream;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use scanbus_core::{
	BackendError, ButtonsCapability, Capabilities as ScannerCapabilities, ProfileKind, RawPage,
	ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
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

/// Backend implementation for mobile scanners.
#[derive(Debug, Clone)]
pub struct MobileBackend {
	discovery_timeout: Duration,
}

impl Default for MobileBackend {
	fn default() -> Self {
		Self {
			discovery_timeout: DISCOVERY_TIMEOUT,
		}
	}
}

impl MobileBackend {
	pub fn new(discovery_timeout: Duration) -> Self {
		Self { discovery_timeout }
	}
}

#[async_trait]
impl ScannerBackend for MobileBackend {
	fn id(&self) -> &'static str {
		ID
	}

	async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
		let timeout = self.discovery_timeout;
		tokio::task::spawn_blocking(move || discover_once(timeout))
			.await
			.map_err(|error| BackendError::Other(format!("mobile discover task failed: {error}")))?
	}

	async fn ensure_installed(
		&self,
		_scanner: &ScannerInfo,
		_progress: mpsc::Sender<scanbus_core::PairingProgress>,
	) -> Result<(), BackendError> {
		Err(BackendError::Unsupported {
			backend: ID,
			operation: "ensure_installed",
		})
	}

	async fn start_listening(
		&self,
		_scanner: &ScannerInfo,
	) -> Result<BoxStream<'static, scanbus_core::ButtonPressedEvent>, BackendError> {
		Err(BackendError::Unsupported {
			backend: ID,
			operation: "start_listening",
		})
	}

	async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
		Err(BackendError::Unsupported {
			backend: ID,
			operation: "stop_listening",
		})
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

fn discover_once(timeout: Duration) -> Result<Vec<ScannerInfo>, BackendError> {
	let daemon = ServiceDaemon::new().map_err(|error| BackendError::NotReachable {
		scanner: ScannerId::from_backend(ID, "discovery").expect("static discovery id is valid"),
		detail: format!("failed to start mDNS browser: {error}"),
	})?;
	let receiver = daemon
		.browse(SERVICE_TYPE)
		.map_err(|error| BackendError::Other(format!("failed to browse {SERVICE_TYPE}: {error}")))?;

	let mut seen = BTreeMap::<ScannerId, ScannerInfo>::new();
	let started = Instant::now();
	loop {
		let elapsed = started.elapsed();
		if elapsed >= timeout {
			break;
		}
		let remaining = timeout - elapsed;
		match receiver.recv_timeout(remaining) {
			Ok(ServiceEvent::ServiceResolved(service)) => {
				if let Some(scanner) = scanner_from_service(&service) {
					if seen.contains_key(&scanner.id) {
						warn!(
							scanner_id = %scanner.id,
							instance = %service.get_fullname(),
							"duplicate mobile scanner id discovered; keeping first one"
						);
						continue;
					}
					seen.insert(scanner.id.clone(), scanner);
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

fn scanner_from_service(service: &mdns_sd::ServiceInfo) -> Option<ScannerInfo> {
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

	Some(ScannerInfo {
		id: scanner_id,
		name: instance_name(&instance),
		backend: ID.to_owned(),
		address: format!("{}:{}", ip, service.get_port()),
		capabilities: ScannerCapabilities {
			buttons: ButtonsCapability {
				count: 0,
				label_configurable: false,
			},
			..ScannerCapabilities::default()
		},
		status: Status::Online,
	})
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairRequest {
	pub v: u32,
	pub host_id: String,
	pub host_name: String,
	pub upload_port: u16,
	pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairResponse {
	pub accepted: bool,
	pub device_id: String,
	#[serde(default)]
	pub capabilities: Capabilities,
	#[serde(default)]
	pub token: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upload {
	pub v: u32,
	pub device_id: String,
	pub token: String,
	pub profile: ProfileKind,
	pub page: u32,
	pub of: u32,
	pub format: String,
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

	use super::*;

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
